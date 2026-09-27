//! Descriptor-pinned namespace primitives for synthetic C3-B1A validation.
//!
//! Every mutable operation below is relative to an already-open directory
//! descriptor.  Paths are accepted only at the outer boundary and are never
//! followed again after the descriptor identity has been recorded.

use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::Write;
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::path::{Component, Path, PathBuf};

const MANIFEST: &str = ".quotabar-c3b1-owner";
const PRIVATE_MODE: u32 = 0o700;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NamespaceError {
    Collision,
    Mismatch,
    Io,
}

#[derive(Clone, Debug)]
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
        manifest_file
            .write_all(b"c3-b1a-owner-v1\n")
            .map_err(|_| NamespaceError::Io)?;
        let manifest_stat =
            stat_at(root_file.as_raw_fd(), &manifest)?.ok_or(NamespaceError::Mismatch)?;
        validate_file_stat(&manifest_stat, 0o600)?;
        Ok(Self {
            parent,
            root: root_file,
            root_name,
            root_entry,
            entries: vec![OwnedEntry::from_stat(manifest, &manifest_stat, 0o600)],
        })
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
        Ok(())
    }

    /// Exact bottom-up cleanup.  A descriptor/stat mismatch stops before any
    /// unknown object is removed; the final root removal is `unlinkat` against
    /// the pinned parent descriptor rather than a path-based recursive API.
    pub(crate) fn cleanup(self) -> Result<(), NamespaceError> {
        let root_stat = stat_fd(&self.root)?;
        if !self.root_entry.matches(&root_stat) {
            return Err(NamespaceError::Mismatch);
        }
        for entry in self.entries.iter().rev() {
            let stat =
                stat_at(self.root.as_raw_fd(), &entry.name)?.ok_or(NamespaceError::Mismatch)?;
            if !entry.matches(&stat) {
                return Err(NamespaceError::Mismatch);
            }
            let is_manifest = entry.name.as_c_str().to_bytes() == MANIFEST.as_bytes();
            unlink_at(
                self.root.as_raw_fd(),
                &entry.name,
                if is_manifest { 0 } else { libc::AT_REMOVEDIR },
            )?;
        }
        let parent_root =
            stat_at(self.parent.as_raw_fd(), &self.root_name)?.ok_or(NamespaceError::Mismatch)?;
        if !self.root_entry.matches(&parent_root) {
            return Err(NamespaceError::Mismatch);
        }
        unlink_at(self.parent.as_raw_fd(), &self.root_name, libc::AT_REMOVEDIR)
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
    let path = c_name(path.as_os_str())?;
    unsafe { file_from_fd(libc::open(path.as_ptr(), dir_flags())) }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use uuid::Uuid;

    fn root() -> PathBuf {
        std::env::temp_dir().join(format!("quotabar-c3b1-ns-{}", Uuid::new_v4()))
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
}
