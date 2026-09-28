//! Descriptor-pinned namespace primitives for synthetic C3-B1A validation.
//!
//! Every mutable operation below is relative to an already-open directory
//! descriptor.  Paths are accepted only at the outer boundary and are never
//! followed again after the descriptor identity has been recorded.

use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::path::{Component, Path, PathBuf};

const MANIFEST: &str = ".quotabar-c3b1-owner";
const MANIFEST_HEADER: &str = "quotabar-c3b1-owner-v2";
const PRIVATE_MODE: u32 = 0o700;
const MAX_ENTRIES: usize = 16;
const MAX_MANIFEST_BYTES: usize = 64 * 1024;

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
    kind: EntryKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EntryKind {
    File,
    Directory,
}

/// A capability returned only for a freshly created root.  Existing roots are
/// never promoted back into this mutable type after a restart.
#[derive(Debug)]
pub(crate) struct PreparedValidationNamespace {
    parent: File,
    root: File,
    root_name: CString,
    root_entry: OwnedEntry,
    entries: Vec<OwnedEntry>,
    mutation_valid: bool,
    #[cfg(test)]
    persist_failpoint: Option<PersistFailpoint>,
}

impl PreparedValidationNamespace {
    pub(crate) fn prepare_new(root: PathBuf) -> Result<Self, NamespaceError> {
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
            mutation_valid: true,
            #[cfg(test)]
            persist_failpoint: None,
        };
        namespace.persist_inventory()?;
        Ok(namespace)
    }

    /// Only direct normal names are admitted, preventing ancestor traversal.
    pub(crate) fn create_private_dir(&mut self, relative: &str) -> Result<(), NamespaceError> {
        if !self.mutation_valid {
            return Err(NamespaceError::Mismatch);
        }
        // The manifest entry itself consumes one of the fixed inventory slots.
        // Check before stat/mkdir so a seventeenth entry cannot reach disk.
        if self.entries.len() >= MAX_ENTRIES {
            return Err(NamespaceError::Mismatch);
        }
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
        self.persist_inventory()
    }

    /// Inspecting a pre-existing root is diagnostic-only.  It intentionally
    /// grants neither create nor persist capability, even if every byte looks
    /// self-consistent.
    pub(crate) fn inspect_existing(root: PathBuf) -> Result<NamespaceDiagnostic, NamespaceError> {
        let _inspection = (|| -> Result<(), NamespaceError> {
            reject_collision(&root)?;
            let parent_path = root.parent().ok_or(NamespaceError::Collision)?;
            let root_name = c_name(root.file_name().ok_or(NamespaceError::Collision)?)?;
            let parent = open_dir(parent_path)?;
            let root_file = open_dir_at(parent.as_raw_fd(), &root_name)?;
            let root_stat = stat_fd(&root_file)?;
            validate_dir_stat(&root_stat, Some(PRIVATE_MODE))?;
            let manifest = CString::new(MANIFEST).map_err(|_| NamespaceError::Collision)?;
            let manifest_stat =
                stat_at(root_file.as_raw_fd(), &manifest)?.ok_or(NamespaceError::Mismatch)?;
            validate_file_stat(&manifest_stat, 0o600)?;
            let bytes = read_file_at_bounded(root_file.as_raw_fd(), &manifest)?;
            let entries = parse_inventory(&bytes)?;
            validate_inventory_entries(root_file.as_raw_fd(), &manifest, &entries)?;
            Ok(())
        })();
        // Existing state is never an error path that a caller can "repair" by
        // retrying creation. Both valid and malformed state are preserve-only.
        Ok(NamespaceDiagnostic::ManualHandlingRequired)
    }

    fn persist_inventory(&mut self) -> Result<(), NamespaceError> {
        if !self.mutation_valid {
            return Err(NamespaceError::Mismatch);
        }
        let result = self.persist_inventory_inner();
        if result.is_err() {
            // A failed evidence write leaves any unknown on-disk state for
            // manual handling and permanently revokes this mutable handle.
            self.mutation_valid = false;
        }
        result
    }

    fn persist_inventory_inner(&mut self) -> Result<(), NamespaceError> {
        let manifest_name = self
            .entries
            .iter()
            .find(|entry| entry.name.as_c_str().to_bytes() == MANIFEST.as_bytes())
            .ok_or(NamespaceError::Mismatch)?
            .name
            .clone();
        let bytes = render_inventory_bounded(&self.entries)?;
        let mut nonce = [0u8; 16];
        getrandom::fill(&mut nonce).map_err(|_| NamespaceError::Io)?;
        let nonce_text: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
        let temporary = CString::new(format!(".quotabar-c3b1-tmp-{nonce_text}"))
            .map_err(|_| NamespaceError::Io)?;
        self.fail_if(PersistFailpoint::BeforeTempCreate)?;
        let mut file = unsafe {
            file_from_fd(libc::openat(
                self.root.as_raw_fd(),
                temporary.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            ))
        }?;
        self.fail_if(PersistFailpoint::AfterTempCreate)?;
        file.write_all(&bytes).map_err(|_| NamespaceError::Io)?;
        self.fail_if(PersistFailpoint::AfterTempWrite)?;
        file.sync_all().map_err(|_| NamespaceError::Io)?;
        self.fail_if(PersistFailpoint::AfterTempFsync)?;
        if unsafe {
            libc::renameat(
                self.root.as_raw_fd(),
                temporary.as_ptr(),
                self.root.as_raw_fd(),
                manifest_name.as_ptr(),
            )
        } != 0
        {
            return Err(NamespaceError::Io);
        }
        self.fail_if(PersistFailpoint::AfterRename)?;
        self.fail_if(PersistFailpoint::BeforeDirectoryFsync)?;
        if unsafe { libc::fsync(self.root.as_raw_fd()) } != 0 {
            return Err(NamespaceError::Io);
        }
        self.fail_if(PersistFailpoint::AfterDirectoryFsync)?;
        Ok(())
    }

    #[cfg(test)]
    fn fail_persist_at_for_test(&mut self, point: PersistFailpoint) {
        self.persist_failpoint = Some(point);
    }

    fn fail_if(&mut self, point: PersistFailpoint) -> Result<(), NamespaceError> {
        #[cfg(test)]
        if self.persist_failpoint == Some(point) {
            self.persist_failpoint = None;
            return Err(NamespaceError::Io);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PersistFailpoint {
    BeforeTempCreate,
    AfterTempCreate,
    AfterTempWrite,
    AfterTempFsync,
    AfterRename,
    BeforeDirectoryFsync,
    AfterDirectoryFsync,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NamespaceDiagnostic {
    ManualHandlingRequired,
}

impl OwnedEntry {
    fn from_stat(name: CString, stat: &libc::stat, mode: u32) -> Self {
        let kind = match stat.st_mode & libc::S_IFMT {
            libc::S_IFREG => EntryKind::File,
            libc::S_IFDIR => EntryKind::Directory,
            _ => EntryKind::File,
        };
        Self {
            name,
            dev: stat.st_dev as u64,
            ino: stat.st_ino as u64,
            mode,
            kind,
        }
    }

    fn matches(&self, stat: &libc::stat) -> bool {
        stat.st_uid == unsafe { libc::geteuid() }
            && stat.st_dev as u64 == self.dev
            && stat.st_ino as u64 == self.ino
            && (stat.st_mode as u32 & 0o777) == self.mode
    }
}

fn render_inventory_bounded(entries: &[OwnedEntry]) -> Result<Vec<u8>, NamespaceError> {
    // Compute and check the exact byte count before allocating the output.
    let mut required = MANIFEST_HEADER.len() + 1;
    for entry in entries
        .iter()
        .filter(|entry| entry.name.as_c_str().to_bytes() != MANIFEST.as_bytes())
    {
        let kind = match entry.kind { EntryKind::File => "file", EntryKind::Directory => "directory" };
        required = required
            .checked_add(entry.name.as_bytes().len() + 1 + entry.dev.to_string().len() + 1
                + entry.ino.to_string().len() + 1 + entry.mode.to_string().len() + 1 + kind.len() + 1)
            .ok_or(NamespaceError::Mismatch)?;
        if required > MAX_MANIFEST_BYTES { return Err(NamespaceError::Mismatch); }
    }
    let mut text = String::with_capacity(required);
    text.push_str(MANIFEST_HEADER);
    text.push('\n');
    for entry in entries.iter().filter(|entry| entry.name.as_c_str().to_bytes() != MANIFEST.as_bytes()) {
        let kind = match entry.kind { EntryKind::File => "file", EntryKind::Directory => "directory" };
        use std::fmt::Write as _;
        write!(&mut text, "{}\t{}\t{}\t{}\t{}\n", entry.name.to_string_lossy(), entry.dev, entry.ino, entry.mode, kind).map_err(|_| NamespaceError::Io)?;
    }
    Ok(text.into_bytes())
}

fn parse_inventory(bytes: &[u8]) -> Result<Vec<OwnedEntry>, NamespaceError> {
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(NamespaceError::Mismatch);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| NamespaceError::Mismatch)?;
    let mut lines = text.lines();
    if lines.next() != Some(MANIFEST_HEADER) {
        return Err(NamespaceError::Mismatch);
    }
    let mut entries = Vec::new();
    for line in lines {
        if entries.len() == MAX_ENTRIES {
            return Err(NamespaceError::Mismatch);
        }
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
        let mode: u32 = fields
            .next()
            .ok_or(NamespaceError::Mismatch)?
            .parse()
            .map_err(|_| NamespaceError::Mismatch)?;
        let kind = match fields.next() {
            Some("file") => EntryKind::File,
            Some("directory") => EntryKind::Directory,
            _ => return Err(NamespaceError::Mismatch),
        };
        if fields.next().is_some()
            || mode != PRIVATE_MODE
            || entries.iter().any(|entry: &OwnedEntry| entry.name == name)
        {
            return Err(NamespaceError::Mismatch);
        }
        entries.push(OwnedEntry {
            name,
            dev,
            ino,
            mode,
            kind,
        });
    }
    Ok(entries)
}

fn validate_inventory_entries(
    dirfd: i32,
    manifest: &CStr,
    entries: &[OwnedEntry],
) -> Result<(), NamespaceError> {
    for entry in entries {
        let stat = stat_at(dirfd, entry.name.as_c_str())?.ok_or(NamespaceError::Mismatch)?;
        let expected_type = match entry.kind {
            EntryKind::File => libc::S_IFREG,
            EntryKind::Directory => libc::S_IFDIR,
        };
        if (stat.st_mode & libc::S_IFMT) != expected_type || !entry.matches(&stat) {
            return Err(NamespaceError::Mismatch);
        }
    }

    // The descriptor scan is diagnostic-only; it neither removes nor opens
    // unknown names for mutation. Every entry must be declared exactly once.
    let duplicate = unsafe { libc::fcntl(dirfd, libc::F_DUPFD_CLOEXEC, 3) };
    if duplicate < 0 {
        return Err(NamespaceError::Io);
    }
    let directory = unsafe { libc::fdopendir(duplicate) };
    if directory.is_null() {
        unsafe { libc::close(duplicate) };
        return Err(NamespaceError::Io);
    }
    let mut accepted = true;
    loop {
        let entry = unsafe { libc::readdir(directory) };
        if entry.is_null() {
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        let bytes = name.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        if name == manifest || !entries.iter().any(|known| known.name.as_c_str() == name) {
            accepted = false;
            break;
        }
    }
    unsafe { libc::closedir(directory) };
    if accepted {
        Ok(())
    } else {
        Err(NamespaceError::Mismatch)
    }
}

fn c_name(value: &std::ffi::OsStr) -> Result<CString, NamespaceError> {
    CString::new(value.as_encoded_bytes()).map_err(|_| NamespaceError::Collision)
}

fn checked_direct_name(input: &str) -> Result<CString, NamespaceError> {
    if !(1..=128).contains(&input.len()) || input.as_bytes().contains(&0) {
        return Err(NamespaceError::Collision);
    }
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
fn read_file_at_bounded(dirfd: i32, name: &CStr) -> Result<Vec<u8>, NamespaceError> {
    const MAX: usize = MAX_MANIFEST_BYTES;
    let mut file = unsafe {
        file_from_fd(libc::openat(
            dirfd,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        ))
    }?;
    let mut bytes = Vec::with_capacity(MAX.min(4096));
    Read::by_ref(&mut file)
        .take((MAX + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| NamespaceError::Io)?;
    if bytes.len() > MAX {
        return Err(NamespaceError::Mismatch);
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::MetadataExt;
    use uuid::Uuid;

    fn root() -> PathBuf {
        std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("quotabar-c3b1-ns-{}", Uuid::new_v4()))
    }

    #[test]
    fn prepared_inventory_is_preserved_for_manual_handling() {
        let root = root();
        let mut namespace = PreparedValidationNamespace::prepare_new(root.clone()).unwrap();
        namespace.create_private_dir("slots").unwrap();
        namespace.create_private_dir("observer").unwrap();
        assert!(root.join(MANIFEST).exists());
        assert!(root.join(MANIFEST).exists());
        assert!(root.join("slots").exists());
        assert!(root.join("observer").exists());
        let _ = fs::remove_dir(root.join("slots"));
        let _ = fs::remove_dir(root.join("observer"));
        let _ = fs::remove_file(root.join(MANIFEST));
        let _ = fs::remove_dir(root);
    }

    #[test]
    fn collisions_and_substitution_fail_closed() {
        assert_eq!(
            PreparedValidationNamespace::prepare_new(PathBuf::from("/Applications/Claude.app"))
                .unwrap_err(),
            NamespaceError::Collision
        );
        let root = root();
        let mut namespace = PreparedValidationNamespace::prepare_new(root.clone()).unwrap();
        namespace.create_private_dir("slots").unwrap();
        fs::remove_dir(root.join("slots")).unwrap();
        std::os::unix::fs::symlink("/tmp", root.join("slots")).unwrap();
        assert!(root.join(MANIFEST).exists());
        let _ = fs::remove_file(root.join("slots"));
        let _ = fs::remove_file(root.join(MANIFEST));
        let _ = fs::remove_dir(root);
    }

    #[test]
    fn foreign_entries_are_preserved_for_manual_handling() {
        let root = root();
        let mut namespace = PreparedValidationNamespace::prepare_new(root.clone()).unwrap();
        namespace.create_private_dir("slots").unwrap();
        fs::write(root.join("unexpected"), b"x").unwrap();
        assert!(root.join(MANIFEST).exists());
        assert!(root.join(MANIFEST).exists());
        assert!(root.join("slots").exists());
        assert!(root.join("unexpected").exists());
        let _ = fs::remove_file(root.join("unexpected"));
        let _ = fs::remove_dir(root.join("slots"));
        let _ = fs::remove_file(root.join(MANIFEST));
        let _ = fs::remove_dir(root);
    }

    #[test]
    fn altered_manifest_is_preserved_for_manual_handling() {
        let root = root();
        let mut namespace = PreparedValidationNamespace::prepare_new(root.clone()).unwrap();
        namespace.create_private_dir("slots").unwrap();
        fs::write(root.join(MANIFEST), b"altered\n").unwrap();
        assert!(root.join(MANIFEST).exists());
        assert!(root.join(MANIFEST).exists());
        assert!(root.join("slots").exists());
        let _ = fs::remove_file(root.join(MANIFEST));
        let _ = fs::remove_dir(root.join("slots"));
        let _ = fs::remove_dir(root);
    }

    #[test]
    fn replaced_entry_is_preserved_for_manual_handling() {
        let root = root();
        let mut namespace = PreparedValidationNamespace::prepare_new(root.clone()).unwrap();
        namespace.create_private_dir("slots").unwrap();
        namespace.create_private_dir("observer").unwrap();
        fs::remove_dir(root.join("slots")).unwrap();
        fs::create_dir(root.join("slots")).unwrap();
        assert!(root.join(MANIFEST).exists());
        assert!(root.join(MANIFEST).exists());
        assert!(root.join("slots").exists());
        assert!(root.join("observer").exists());
        let _ = fs::remove_dir(root.join("slots"));
        let _ = fs::remove_dir(root.join("observer"));
        let _ = fs::remove_file(root.join(MANIFEST));
        let _ = fs::remove_dir(root);
    }

    #[test]
    fn existing_inventory_is_diagnostic_only() {
        let root = root();
        let mut namespace = PreparedValidationNamespace::prepare_new(root.clone()).unwrap();
        namespace.create_private_dir("slots").unwrap();
        namespace.create_private_dir("observer").unwrap();
        drop(namespace);
        fs::remove_dir(root.join("slots")).unwrap();
        fs::create_dir(root.join("slots")).unwrap();
        assert_eq!(
            PreparedValidationNamespace::inspect_existing(root.clone()).unwrap(),
            NamespaceDiagnostic::ManualHandlingRequired
        );
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
            PreparedValidationNamespace::prepare_new(linked.join("child")),
            Err(NamespaceError::Io | NamespaceError::Collision | NamespaceError::Mismatch)
        ));
        let _ = fs::remove_file(&linked);
        let _ = fs::remove_dir(&redirected);
        let _ = fs::remove_dir(&base);
    }

    #[test]
    fn inventory_bounds_names_and_duplicates_fail_closed() {
        assert!(parse_inventory(format!("{MANIFEST_HEADER}\n").as_bytes()).is_ok());
        let mut seventeen = format!("{MANIFEST_HEADER}\n");
        for index in 0..17 {
            seventeen.push_str(&format!("entry-{index}\t1\t1\t448\tdirectory\n"));
        }
        assert_eq!(
            parse_inventory(seventeen.as_bytes()),
            Err(NamespaceError::Mismatch)
        );
        assert_eq!(checked_direct_name(""), Err(NamespaceError::Collision));
        assert_eq!(
            checked_direct_name(&"a".repeat(129)),
            Err(NamespaceError::Collision)
        );
        assert_eq!(checked_direct_name("."), Err(NamespaceError::Collision));
        assert_eq!(
            checked_direct_name("../child"),
            Err(NamespaceError::Collision)
        );
        let duplicate =
            format!("{MANIFEST_HEADER}\nslot\t1\t1\t448\tdirectory\nslot\t1\t2\t448\tdirectory\n");
        assert_eq!(
            parse_inventory(duplicate.as_bytes()),
            Err(NamespaceError::Mismatch)
        );
    }

    #[test]
    fn seventeenth_inventory_entry_is_rejected_before_mkdir() {
        let root = root();
        let mut namespace = PreparedValidationNamespace::prepare_new(root.clone()).unwrap();
        for index in 0..15 {
            namespace.create_private_dir(&format!("entry-{index}")).unwrap();
        }
        let before = fs::read(root.join(MANIFEST)).unwrap();
        assert_eq!(namespace.create_private_dir("seventeenth"), Err(NamespaceError::Mismatch));
        assert!(!root.join("seventeenth").exists());
        assert_eq!(fs::read(root.join(MANIFEST)).unwrap(), before);
        for index in 0..15 { let _ = fs::remove_dir(root.join(format!("entry-{index}"))); }
        let _ = fs::remove_file(root.join(MANIFEST));
        let _ = fs::remove_dir(root);
    }

    #[test]
    fn bounded_manifest_reader_distinguishes_65536_and_65537_bytes() {
        let root = root();
        fs::create_dir(&root).unwrap();
        let file = root.join("manifest");
        fs::write(&file, vec![b'x'; 64 * 1024]).unwrap();
        let directory = open_dir(&root).unwrap();
        let name = CString::new("manifest").unwrap();
        assert_eq!(
            read_file_at_bounded(directory.as_raw_fd(), &name)
                .unwrap()
                .len(),
            64 * 1024
        );
        fs::write(&file, vec![b'x'; 64 * 1024 + 1]).unwrap();
        assert_eq!(
            read_file_at_bounded(directory.as_raw_fd(), &name),
            Err(NamespaceError::Mismatch)
        );
        drop(directory);
        let _ = fs::remove_file(file);
        let _ = fs::remove_dir(root);
    }

    #[test]
    fn failed_persist_permanently_revokes_mutation_capability() {
        let root = root();
        let mut namespace = PreparedValidationNamespace::prepare_new(root.clone()).unwrap();
        namespace.fail_persist_at_for_test(PersistFailpoint::BeforeTempCreate);
        assert!(namespace.create_private_dir("first").is_err());
        assert_eq!(
            namespace.create_private_dir("second"),
            Err(NamespaceError::Mismatch)
        );
        assert!(!root.join("second").exists());
        let _ = fs::remove_dir(root.join("first"));
        let _ = fs::remove_file(root.join(MANIFEST));
        let _ = fs::remove_dir(root);
    }

    #[test]
    fn every_persist_failpoint_leaves_a_complete_old_or_new_manifest_and_revokes_mutation() {
        for point in [
            PersistFailpoint::BeforeTempCreate,
            PersistFailpoint::AfterTempCreate,
            PersistFailpoint::AfterTempWrite,
            PersistFailpoint::AfterTempFsync,
            PersistFailpoint::AfterRename,
            PersistFailpoint::BeforeDirectoryFsync,
            PersistFailpoint::AfterDirectoryFsync,
        ] {
            let root = root();
            let mut namespace = PreparedValidationNamespace::prepare_new(root.clone()).unwrap();
            let old = fs::read(root.join(MANIFEST)).unwrap();
            namespace.fail_persist_at_for_test(point);
            assert_eq!(namespace.create_private_dir("slot"), Err(NamespaceError::Io));
            let observed = fs::read(root.join(MANIFEST)).unwrap();
            let new = format!("{MANIFEST_HEADER}\nslot\t{}\t{}\t448\tdirectory\n",
                fs::metadata(root.join("slot")).unwrap().dev(),
                fs::metadata(root.join("slot")).unwrap().ino()).into_bytes();
            assert!(observed == old || observed == new, "{point:?} produced torn manifest");
            assert_eq!(namespace.create_private_dir("again"), Err(NamespaceError::Mismatch));
            let _ = fs::remove_dir(root.join("slot"));
            for entry in fs::read_dir(&root).unwrap() {
                let entry = entry.unwrap();
                if entry.file_name().to_string_lossy().starts_with(".quotabar-c3b1-tmp-") {
                    let _ = fs::remove_file(entry.path());
                }
            }
            let _ = fs::remove_file(root.join(MANIFEST));
            let _ = fs::remove_dir(root);
        }
    }

    #[test]
    fn malformed_existing_state_is_manual_handling_and_never_mutable() {
        let root = root();
        let mut namespace = PreparedValidationNamespace::prepare_new(root.clone()).unwrap();
        namespace.create_private_dir("slots").unwrap();
        drop(namespace);
        fs::write(root.join(MANIFEST), b"forged\n").unwrap();
        assert_eq!(
            PreparedValidationNamespace::inspect_existing(root.clone()).unwrap(),
            NamespaceDiagnostic::ManualHandlingRequired
        );
        assert!(root.join(MANIFEST).exists());
        assert!(root.join("slots").exists());
        let _ = fs::remove_dir(root.join("slots"));
        let _ = fs::remove_file(root.join(MANIFEST));
        let _ = fs::remove_dir(root);
    }
}
