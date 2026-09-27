//! Descriptor-pinned namespace primitives for synthetic C3-B1A validation.
//!
//! Every mutable operation below is relative to an already-open directory
//! descriptor.  Paths are accepted only at the outer boundary and are never
//! followed again after the descriptor identity has been recorded.

use std::collections::BTreeSet;
use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::path::{Component, Path, PathBuf};

const MANIFEST: &str = ".quotabar-c3b1-owner";
const MANIFEST_HEADER: &str = "quotabar-c3b1-owner-v2";
const PRIVATE_MODE: u32 = 0o700;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NamespaceError {
    Collision,
    Mismatch,
    Io,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct OwnedEntry {
    name: CString,
    dev: u64,
    ino: u64,
    mode: u32,
}

/// A manifest-first namespace.  `parent` and `root` pin the only two
/// directories that cleanup can mutate; cleanup never recurses or glob-matches.
#[derive(Debug)]
pub(crate) struct ValidationNamespace {
    parent: File,
    root: File,
    root_name: CString,
    root_entry: OwnedEntry,
    entries: Vec<OwnedEntry>,
}

impl ValidationNamespace {
    pub(crate) fn prepare(root: PathBuf) -> Result<Self, NamespaceError> {
        reject_collision(&root)?;
        let parent_path = root.parent().ok_or(NamespaceError::Collision)?;
        let root_name = c_name(root.file_name().ok_or(NamespaceError::Collision)?)?;
        let parent = open_dir(parent_path)?;
        let parent_stat = stat_fd(&parent)?;
        validate_dir_stat(&parent_stat, None)?;
        if stat_at(parent.as_raw_fd(), &root_name)?.is_some() {
            return Err(NamespaceError::Collision);
        }
        mkdir_at(parent.as_raw_fd(), &root_name, PRIVATE_MODE)?;
        let root_file = open_dir_at(parent.as_raw_fd(), &root_name)?;
        let root_stat = stat_fd(&root_file)?;
        validate_dir_stat(&root_stat, Some(PRIVATE_MODE))?;
        let root_entry = OwnedEntry::from_stat(root_name.clone(), &root_stat, PRIVATE_MODE);
        let manifest = CString::new(MANIFEST).map_err(|_| NamespaceError::Collision)?;
        let mut manifest_file = create_file_at(root_file.as_raw_fd(), &manifest, 0o600)?;
        manifest_file.flush().map_err(|_| NamespaceError::Io)?;
        let manifest_stat =
            stat_at(root_file.as_raw_fd(), &manifest)?.ok_or(NamespaceError::Mismatch)?;
        validate_file_stat(&manifest_stat, 0o600)?;
        let mut namespace = Self {
            parent,
            root: root_file,
            root_name,
            root_entry,
            entries: vec![OwnedEntry::from_stat(manifest, &manifest_stat, 0o600)],
        };
        namespace.persist_inventory()?;
        Ok(namespace)
    }

    /// Only direct normal names are admitted.  This makes the cleanup manifest
    /// exact and prevents a future caller from smuggling an ancestor traversal.
    pub(crate) fn create_private_dir(&mut self, relative: &str) -> Result<(), NamespaceError> {
        let name = checked_direct_name(relative)?;
        if stat_at(self.root.as_raw_fd(), &name)?.is_some() {
            return Err(NamespaceError::Collision);
        }
        mkdir_at(self.root.as_raw_fd(), &name, PRIVATE_MODE)?;
        let child = open_dir_at(self.root.as_raw_fd(), &name)?;
        let child_stat = stat_fd(&child)?;
        validate_dir_stat(&child_stat, Some(PRIVATE_MODE))?;
        self.entries
            .push(OwnedEntry::from_stat(name, &child_stat, PRIVATE_MODE));
        self.persist_inventory()?;
        Ok(())
    }

    pub(crate) fn reopen(root: PathBuf) -> Result<Self, NamespaceError> {
        reject_collision(&root)?;
        let parent_path = root.parent().ok_or(NamespaceError::Collision)?;
        let root_name = c_name(root.file_name().ok_or(NamespaceError::Collision)?)?;
        let parent = open_dir(parent_path)?;
        let root_file = open_dir_at(parent.as_raw_fd(), &root_name)?;
        let root_stat = stat_fd(&root_file)?;
        validate_dir_stat(&root_stat, Some(PRIVATE_MODE))?;
        let root_entry = OwnedEntry::from_stat(root_name.clone(), &root_stat, PRIVATE_MODE);
        let manifest = CString::new(MANIFEST).map_err(|_| NamespaceError::Collision)?;
        let manifest_stat =
            stat_at(root_file.as_raw_fd(), &manifest)?.ok_or(NamespaceError::Mismatch)?;
        validate_file_stat(&manifest_stat, 0o600)?;
        let mut entries = vec![OwnedEntry::from_stat(
            manifest.clone(),
            &manifest_stat,
            0o600,
        )];
        entries.extend(parse_inventory(&read_file_at(
            root_file.as_raw_fd(),
            &manifest,
        )?)?);
        Ok(Self {
            parent,
            root: root_file,
            root_name,
            root_entry,
            entries,
        })
    }

    /// Exact bottom-up cleanup.  A descriptor/stat mismatch stops before any
    /// unknown object is removed; the final root removal is `unlinkat` against
    /// the pinned parent descriptor rather than a path-based recursive API.
    pub(crate) fn cleanup(self) -> Result<(), NamespaceError> {
        let root_stat = stat_fd(&self.root)?;
        if !self.root_entry.matches(&root_stat) {
            return Err(NamespaceError::Mismatch);
        }
        let parent_root =
            stat_at(self.parent.as_raw_fd(), &self.root_name)?.ok_or(NamespaceError::Mismatch)?;
        if !self.root_entry.matches(&parent_root) {
            return Err(NamespaceError::Mismatch);
        }
        let manifest = self
            .entries
            .iter()
            .find(|entry| entry.name.as_c_str().to_bytes() == MANIFEST.as_bytes())
            .ok_or(NamespaceError::Mismatch)?;
        if parse_inventory(&read_file_at(self.root.as_raw_fd(), &manifest.name)?)?
            != self
                .entries
                .iter()
                .filter(|entry| entry.name.as_c_str().to_bytes() != MANIFEST.as_bytes())
                .cloned()
                .collect::<Vec<_>>()
        {
            return Err(NamespaceError::Mismatch);
        }
        // Preflight the full manifest before unlinking anything.  In
        // particular, an attacker-added or replaced entry cannot leave a
        // partially cleaned namespace behind.
        let actual = direct_names(&self.root)?;
        let expected: BTreeSet<Vec<u8>> = self
            .entries
            .iter()
            .map(|entry| entry.name.as_bytes().to_vec())
            .collect();
        if actual != expected {
            return Err(NamespaceError::Mismatch);
        }
        // All entry identities must be checked before deletion begins.  The
        // subsequent loop is mutation-only, preventing reverse-order partial
        // cleanup when an earlier entry has been replaced in place.
        for entry in &self.entries {
            let stat =
                stat_at(self.root.as_raw_fd(), &entry.name)?.ok_or(NamespaceError::Mismatch)?;
            if !entry.matches(&stat) {
                return Err(NamespaceError::Mismatch);
            }
        }
        for entry in self.entries.iter().rev() {
            let is_manifest = entry.name.as_c_str().to_bytes() == MANIFEST.as_bytes();
            unlink_at(
                self.root.as_raw_fd(),
                &entry.name,
                if is_manifest { 0 } else { libc::AT_REMOVEDIR },
            )?;
        }
        unlink_at(self.parent.as_raw_fd(), &self.root_name, libc::AT_REMOVEDIR)
    }

    fn persist_inventory(&mut self) -> Result<(), NamespaceError> {
        let manifest = self
            .entries
            .iter()
            .find(|entry| entry.name.as_c_str().to_bytes() == MANIFEST.as_bytes())
            .ok_or(NamespaceError::Mismatch)?;
        let bytes = render_inventory(&self.entries);
        let mut file = unsafe {
            file_from_fd(libc::openat(
                self.root.as_raw_fd(),
                manifest.name.as_ptr(),
                libc::O_WRONLY | libc::O_TRUNC | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            ))
        }?;
        file.write_all(&bytes).map_err(|_| NamespaceError::Io)?;
        file.sync_all().map_err(|_| NamespaceError::Io)
    }
}

impl OwnedEntry {
    fn from_stat(name: CString, stat: &libc::stat, mode: u32) -> Self {
        Self {
            name,
            dev: stat.st_dev as u64,
            ino: stat.st_ino as u64,
            mode,
        }
    }

    fn matches(&self, stat: &libc::stat) -> bool {
        stat.st_uid == unsafe { libc::geteuid() }
            && stat.st_dev as u64 == self.dev
            && stat.st_ino as u64 == self.ino
            && (stat.st_mode as u32 & 0o777) == self.mode
    }
}

fn render_inventory(entries: &[OwnedEntry]) -> Vec<u8> {
    let mut text = format!("{MANIFEST_HEADER}\n");
    for entry in entries
        .iter()
        .filter(|entry| entry.name.as_c_str().to_bytes() != MANIFEST.as_bytes())
    {
        text.push_str(&format!(
            "{}\t{}\t{}\t{}\n",
            entry.name.to_string_lossy(),
            entry.dev,
            entry.ino,
            entry.mode
        ));
    }
    text.into_bytes()
}

fn parse_inventory(bytes: &[u8]) -> Result<Vec<OwnedEntry>, NamespaceError> {
    let text = std::str::from_utf8(bytes).map_err(|_| NamespaceError::Mismatch)?;
    let mut lines = text.lines();
    if lines.next() != Some(MANIFEST_HEADER) {
        return Err(NamespaceError::Mismatch);
    }
    let mut entries = Vec::new();
    for line in lines {
        let mut fields = line.split('\t');
        let name = checked_direct_name(fields.next().ok_or(NamespaceError::Mismatch)?)?;
        let dev = fields
            .next()
            .ok_or(NamespaceError::Mismatch)?
            .parse()
            .map_err(|_| NamespaceError::Mismatch)?;
        let ino = fields
            .next()
            .ok_or(NamespaceError::Mismatch)?
            .parse()
            .map_err(|_| NamespaceError::Mismatch)?;
        let mode = fields
            .next()
            .ok_or(NamespaceError::Mismatch)?
            .parse()
            .map_err(|_| NamespaceError::Mismatch)?;
        if fields.next().is_some() || mode != PRIVATE_MODE {
            return Err(NamespaceError::Mismatch);
        }
        entries.push(OwnedEntry {
            name,
            dev,
            ino,
            mode,
        });
    }
    Ok(entries)
}

fn c_name(value: &std::ffi::OsStr) -> Result<CString, NamespaceError> {
    CString::new(value.as_encoded_bytes()).map_err(|_| NamespaceError::Collision)
}

fn checked_direct_name(input: &str) -> Result<CString, NamespaceError> {
    let path = Path::new(input);
    if path.is_absolute()
        || path.components().count() != 1
        || !matches!(path.components().next(), Some(Component::Normal(_)))
    {
        return Err(NamespaceError::Collision);
    }
    c_name(path.as_os_str())
}

fn reject_collision(root: &Path) -> Result<(), NamespaceError> {
    let rendered = root.to_string_lossy();
    if rendered == "/Applications/Claude.app"
        || rendered.contains("/Claude.app/")
        || rendered.contains("claude-current-state")
        || rendered.contains("com.anthropic")
        || rendered.contains("/Applications/")
    {
        return Err(NamespaceError::Collision);
    }
    Ok(())
}

fn open_dir(path: &Path) -> Result<File, NamespaceError> {
    if !path.is_absolute() {
        return Err(NamespaceError::Collision);
    }
    // Opening only the final component with O_NOFOLLOW leaves every ancestor
    // vulnerable to substitution.  Walk the absolute path one component at a
    // time through pinned descriptors so each ancestor is itself no-follow.
    let slash = CString::new("/").map_err(|_| NamespaceError::Io)?;
    let mut current = unsafe { file_from_fd(libc::open(slash.as_ptr(), dir_flags())) }?;
    for component in path.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        let name = c_name(name)?;
        current = open_dir_at(current.as_raw_fd(), &name)?;
        if (stat_fd(&current)?.st_mode & libc::S_IFMT) != libc::S_IFDIR {
            return Err(NamespaceError::Mismatch);
        }
    }
    Ok(current)
}
fn open_dir_at(dirfd: i32, name: &CStr) -> Result<File, NamespaceError> {
    unsafe { file_from_fd(libc::openat(dirfd, name.as_ptr(), dir_flags())) }
}
unsafe fn file_from_fd(fd: i32) -> Result<File, NamespaceError> {
    if fd < 0 {
        Err(NamespaceError::Io)
    } else {
        Ok(File::from_raw_fd(fd))
    }
}
fn dir_flags() -> i32 {
    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC
}
fn mkdir_at(dirfd: i32, name: &CStr, mode: u32) -> Result<(), NamespaceError> {
    if unsafe { libc::mkdirat(dirfd, name.as_ptr(), mode as libc::mode_t) } == 0 {
        Ok(())
    } else {
        Err(NamespaceError::Collision)
    }
}
fn create_file_at(dirfd: i32, name: &CStr, mode: u32) -> Result<File, NamespaceError> {
    unsafe {
        file_from_fd(libc::openat(
            dirfd,
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode,
        ))
    }
}
fn read_file_at(dirfd: i32, name: &CStr) -> Result<Vec<u8>, NamespaceError> {
    let mut file = unsafe {
        file_from_fd(libc::openat(
            dirfd,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        ))
    }?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|_| NamespaceError::Io)?;
    Ok(bytes)
}
fn stat_fd(file: &File) -> Result<libc::stat, NamespaceError> {
    let mut stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(file.as_raw_fd(), &mut stat) } == 0 {
        Ok(stat)
    } else {
        Err(NamespaceError::Io)
    }
}
fn stat_at(dirfd: i32, name: &CStr) -> Result<Option<libc::stat>, NamespaceError> {
    let mut stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatat(dirfd, name.as_ptr(), &mut stat, libc::AT_SYMLINK_NOFOLLOW) } == 0 {
        Ok(Some(stat))
    } else if std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
        Ok(None)
    } else {
        Err(NamespaceError::Io)
    }
}
fn validate_dir_stat(stat: &libc::stat, expected_mode: Option<u32>) -> Result<(), NamespaceError> {
    if (stat.st_mode & libc::S_IFMT) != libc::S_IFDIR
        || stat.st_uid != unsafe { libc::geteuid() }
        || expected_mode.is_some_and(|mode| (stat.st_mode as u32 & 0o777) != mode)
    {
        Err(NamespaceError::Mismatch)
    } else {
        Ok(())
    }
}
fn validate_file_stat(stat: &libc::stat, mode: u32) -> Result<(), NamespaceError> {
    if (stat.st_mode & libc::S_IFMT) != libc::S_IFREG
        || stat.st_uid != unsafe { libc::geteuid() }
        || (stat.st_mode as u32 & 0o777) != mode
    {
        Err(NamespaceError::Mismatch)
    } else {
        Ok(())
    }
}
fn unlink_at(dirfd: i32, name: &CStr, flags: i32) -> Result<(), NamespaceError> {
    if unsafe { libc::unlinkat(dirfd, name.as_ptr(), flags) } == 0 {
        Ok(())
    } else {
        Err(NamespaceError::Mismatch)
    }
}

fn direct_names(directory: &File) -> Result<BTreeSet<Vec<u8>>, NamespaceError> {
    let duplicate = unsafe { libc::dup(directory.as_raw_fd()) };
    if duplicate < 0 {
        return Err(NamespaceError::Io);
    }
    let stream = unsafe { libc::fdopendir(duplicate) };
    if stream.is_null() {
        unsafe { libc::close(duplicate) };
        return Err(NamespaceError::Io);
    }
    let mut names = BTreeSet::new();
    loop {
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name != b"." && name != b".." {
            names.insert(name.to_vec());
        }
    }
    if unsafe { libc::closedir(stream) } != 0 {
        return Err(NamespaceError::Io);
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use uuid::Uuid;

    fn root() -> PathBuf {
        std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("quotabar-c3b1-ns-{}", Uuid::new_v4()))
    }

    #[test]
    fn exact_manifest_cleanup_is_bounded() {
        let root = root();
        let mut namespace = ValidationNamespace::prepare(root.clone()).unwrap();
        namespace.create_private_dir("slots").unwrap();
        namespace.create_private_dir("observer").unwrap();
        namespace.cleanup().unwrap();
        assert!(!root.exists());
    }

    #[test]
    fn collisions_and_substitution_fail_closed() {
        assert_eq!(
            ValidationNamespace::prepare(PathBuf::from("/Applications/Claude.app")).unwrap_err(),
            NamespaceError::Collision
        );
        let root = root();
        let mut namespace = ValidationNamespace::prepare(root.clone()).unwrap();
        namespace.create_private_dir("slots").unwrap();
        fs::remove_dir(root.join("slots")).unwrap();
        std::os::unix::fs::symlink("/tmp", root.join("slots")).unwrap();
        assert_eq!(namespace.cleanup().unwrap_err(), NamespaceError::Mismatch);
        let _ = fs::remove_file(root.join("slots"));
        let _ = fs::remove_file(root.join(MANIFEST));
        let _ = fs::remove_dir(root);
    }

    #[test]
    fn cleanup_preflights_unknown_entries_before_any_deletion() {
        let root = root();
        let mut namespace = ValidationNamespace::prepare(root.clone()).unwrap();
        namespace.create_private_dir("slots").unwrap();
        fs::write(root.join("unexpected"), b"x").unwrap();
        assert_eq!(namespace.cleanup().unwrap_err(), NamespaceError::Mismatch);
        assert!(root.join(MANIFEST).exists());
        assert!(root.join("slots").exists());
        assert!(root.join("unexpected").exists());
        let _ = fs::remove_file(root.join("unexpected"));
        let _ = fs::remove_dir(root.join("slots"));
        let _ = fs::remove_file(root.join(MANIFEST));
        let _ = fs::remove_dir(root);
    }

    #[test]
    fn altered_manifest_causes_zero_cleanup() {
        let root = root();
        let mut namespace = ValidationNamespace::prepare(root.clone()).unwrap();
        namespace.create_private_dir("slots").unwrap();
        fs::write(root.join(MANIFEST), b"altered\n").unwrap();
        assert_eq!(namespace.cleanup().unwrap_err(), NamespaceError::Mismatch);
        assert!(root.join(MANIFEST).exists());
        assert!(root.join("slots").exists());
        let _ = fs::remove_file(root.join(MANIFEST));
        let _ = fs::remove_dir(root.join("slots"));
        let _ = fs::remove_dir(root);
    }

    #[test]
    fn replaced_earlier_reverse_entry_causes_zero_deletion() {
        let root = root();
        let mut namespace = ValidationNamespace::prepare(root.clone()).unwrap();
        namespace.create_private_dir("slots").unwrap();
        namespace.create_private_dir("observer").unwrap();
        fs::remove_dir(root.join("slots")).unwrap();
        fs::create_dir(root.join("slots")).unwrap();
        assert_eq!(namespace.cleanup().unwrap_err(), NamespaceError::Mismatch);
        assert!(root.join(MANIFEST).exists());
        assert!(root.join("slots").exists());
        assert!(root.join("observer").exists());
        let _ = fs::remove_dir(root.join("slots"));
        let _ = fs::remove_dir(root.join("observer"));
        let _ = fs::remove_file(root.join(MANIFEST));
        let _ = fs::remove_dir(root);
    }

    #[test]
    fn reopened_inventory_retains_zero_delete_proof() {
        let root = root();
        let mut namespace = ValidationNamespace::prepare(root.clone()).unwrap();
        namespace.create_private_dir("slots").unwrap();
        namespace.create_private_dir("observer").unwrap();
        drop(namespace);
        fs::remove_dir(root.join("slots")).unwrap();
        fs::create_dir(root.join("slots")).unwrap();
        let reopened = ValidationNamespace::reopen(root.clone()).unwrap();
        assert_eq!(reopened.cleanup().unwrap_err(), NamespaceError::Mismatch);
        assert!(root.join(MANIFEST).exists());
        assert!(root.join("observer").exists());
        let _ = fs::remove_dir(root.join("slots"));
        let _ = fs::remove_dir(root.join("observer"));
        let _ = fs::remove_file(root.join(MANIFEST));
        let _ = fs::remove_dir(root);
    }

    #[test]
    fn prepare_rejects_symlinked_ancestor() {
        let base = root();
        fs::create_dir(&base).unwrap();
        let redirected = base.join("redirected");
        fs::create_dir(&redirected).unwrap();
        let linked = base.join("linked");
        std::os::unix::fs::symlink(&redirected, &linked).unwrap();
        assert!(matches!(
            ValidationNamespace::prepare(linked.join("child")),
            Err(NamespaceError::Io | NamespaceError::Collision | NamespaceError::Mismatch)
        ));
        let _ = fs::remove_file(&linked);
        let _ = fs::remove_dir(&redirected);
        let _ = fs::remove_dir(&base);
    }
}
