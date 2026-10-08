//! File operations below a trusted [`Anchor`]. Each path component is opened relative to its
//! parent directory descriptor; under [`LinkPolicy::Refuse`] that open uses `O_NOFOLLOW`, so a
//! symlink anywhere below the anchor is refused instead of traversed. One portable `openat`-family
//! implementation serves Linux and macOS.

use super::*;

use std::ffi::{CStr, OsStr, OsString};
use std::fs::File;
use std::io::Read;
use std::os::fd::{AsFd as _, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStringExt as _;
use std::time::{SystemTime, UNIX_EPOCH};

use rand::RngExt as _;
use rustix::fs::{AtFlags, CWD, Dir, FileType, Mode, OFlags, RawMode, Stat};
use rustix::io::Errno;

// === CONSTANTS ===

const PERMISSION_BITS: u32 = 0o7777;
const TEMP_FILE_PREFIX: &str = ".acps-";
const TEMP_FILE_SUFFIX: &str = ".tmp";
const TEMP_FILE_RANDOM_BYTES: usize = 8;
const CURRENT_DIRECTORY: &CStr = c".";
const PARENT_DIRECTORY: &CStr = c"..";
/// Ancestors a tree removal holds open; deeper ones are reopened through `..`.
const REMOVE_TREE_HELD_LEVELS: usize = 32;
/// O_NONBLOCK keeps a FIFO swapped in for a directory from blocking the open before O_DIRECTORY
/// refuses it.
const DIRECTORY_OPEN_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK);
const READ_OPEN_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK);
const CREATE_OPEN_FLAGS: OFlags = OFlags::WRONLY
    .union(OFlags::CREATE)
    .union(OFlags::EXCL)
    .union(OFlags::NOFOLLOW);

/// How a walk treats links below its [`Anchor`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkPolicy {
    /// Refuse every symlink and hard-linked target, as walks for a workload identity must.
    Refuse,
    /// Follow symlinks and accept hard links like a plain path walk; without a workload identity
    /// there is no boundary to hold. `contained` refuses a path that resolves outside the anchor.
    Follow { contained: bool },
}

/// An open, trusted directory every walker operation starts from.
#[derive(Debug)]
pub struct Anchor {
    directory: OwnedFd,
    path: PathBuf,
    links: LinkPolicy,
}

impl Anchor {
    /// Canonicalize `root` once and open it. The root is operator-trusted and may sit behind
    /// legitimate symlinks such as macOS `/var -> /private/var`; everything below it is walked
    /// without following symlinks.
    pub fn open(root: &Path) -> Result<Anchor> {
        Self::open_with(root, LinkPolicy::Refuse)
    }

    /// [`Anchor::open`] with the walk below it following `links`.
    pub fn open_with(root: &Path, links: LinkPolicy) -> Result<Anchor> {
        let path =
            std::fs::canonicalize(root).map_err(|source| failure(root, "canonicalize", source))?;
        let c_path = path_cstring(&path)?;
        let directory = open_at(CWD, &c_path, DIRECTORY_OPEN_FLAGS, 0)
            .map_err(|source| failure(&path, "open", source))?;
        Ok(Anchor {
            directory,
            path,
            links,
        })
    }

    fn follows(&self) -> bool {
        matches!(self.links, LinkPolicy::Follow { .. })
    }

    /// Under `Follow { contained: true }`, refuse a path whose existing part resolves outside the
    /// anchor. Without a workload identity nothing else can swap the path in between.
    fn require_contained(&self, path: &Path) -> Result<()> {
        if self.links != (LinkPolicy::Follow { contained: true }) {
            return Ok(());
        }
        let mut existing = path;
        let resolved = loop {
            match std::fs::canonicalize(existing) {
                Ok(resolved) => break resolved,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    match existing.parent() {
                        Some(parent) => existing = parent,
                        None => return Ok(()),
                    }
                }
                Err(error) => return Err(failure(existing, "canonicalize", error)),
            }
        };
        if resolved.starts_with(&self.path) {
            Ok(())
        } else {
            Err(StackError::WorkloadFsSymlinkRefused {
                path: path.to_path_buf(),
            })
        }
    }

    /// Canonical path of the anchor directory.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Modes and policy for walker writes. Without a workload identity callers pass the owner-only
/// modes; with one they pass the umask modes so results follow the process umask.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WriteOptions {
    /// Create missing parent directories with `dir_mode`.
    pub create_parents: bool,
    pub file_mode: u32,
    pub dir_mode: u32,
    /// Refuse to replace an existing target owned by a different uid.
    pub require_existing_owner: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    Other,
}

/// `lstat` view of one entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryInfo {
    pub kind: EntryKind,
    pub uid: u32,
    pub gid: u32,
    /// Permission bits, including setuid, setgid, and sticky.
    pub mode: u32,
    pub nlink: u64,
    pub size: u64,
    pub modified: SystemTime,
}

/// Read a regular, single-link file of at most `max_bytes`.
pub fn read_file(anchor: &Anchor, relative: &Path, max_bytes: u64) -> Result<Vec<u8>> {
    read_file_with_info(anchor, relative, max_bytes).map(|(content, _)| content)
}

/// [`read_file`], also returning the status of the file the content came from.
pub fn read_file_with_info(
    anchor: &Anchor,
    relative: &Path,
    max_bytes: u64,
) -> Result<(Vec<u8>, EntryInfo)> {
    let (descriptor, status, display) = open_readable(anchor, relative, max_bytes)?;
    let mut content = Vec::new();
    File::from(descriptor)
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut content)
        .map_err(|source| failure(&display, "read", source))?;
    if content.len() as u64 > max_bytes {
        return Err(StackError::WorkspaceTooLarge { limit: max_bytes });
    }
    Ok((content, status))
}

/// Stream the file at `from` into a new file at `to`, keeping its permission bits within
/// `options.file_mode`. The source is vetted as [`read_file`] vets it, with no size bound.
pub fn copy_file(anchor: &Anchor, from: &Path, to: &Path, options: &WriteOptions) -> Result<()> {
    let (descriptor, status, _) = open_readable(anchor, from, u64::MAX)?;
    let file_options = WriteOptions {
        file_mode: status.mode & options.file_mode,
        ..*options
    };
    write_new_from_reader(anchor, to, &mut File::from(descriptor), &file_options)
}

/// Open the regular file at `relative` for reading, with its status and display path.
fn open_readable(
    anchor: &Anchor,
    relative: &Path,
    max_bytes: u64,
) -> Result<(OwnedFd, EntryInfo, PathBuf)> {
    let (parents, name) = entry_components(&anchor.path, relative)?;
    let display = joined(&anchor.path, &parents).join(OsStr::from_bytes(name.as_bytes()));
    let directory = open_directory_chain(anchor, &parents, None)?;
    anchor.require_contained(&display)?;
    // Vet the entry before opening it so a device or FIFO is never opened.
    let status = stat_at(directory.as_fd(), &name, anchor.follows())
        .map_err(|source| failure(&display, "fstatat", source))?
        .ok_or_else(|| StackError::WorkloadFsNotFound {
            path: display.clone(),
        })?;
    vet_readable(anchor, &status, &display, max_bytes)?;
    let open_flags = if anchor.follows() {
        READ_OPEN_FLAGS.difference(OFlags::NOFOLLOW)
    } else {
        READ_OPEN_FLAGS
    };
    let descriptor = open_at(directory.as_fd(), &name, open_flags, 0)
        .map_err(|source| failure(&display, "openat", source))?;
    let status =
        fstat_entry(descriptor.as_fd()).map_err(|source| failure(&display, "fstat", source))?;
    vet_readable(anchor, &status, &display, max_bytes)?;
    Ok((descriptor, status, display))
}

/// Atomically replace (or create) a regular file through a sibling temp file. An existing target
/// must be a single-link regular file, owned by `require_existing_owner` when set.
pub fn write_file_atomic(
    anchor: &Anchor,
    relative: &Path,
    content: &[u8],
    options: &WriteOptions,
) -> Result<()> {
    let (parents, name) = entry_components(&anchor.path, relative)?;
    let parent_display = joined(&anchor.path, &parents);
    let display = child_path(&parent_display, &name);
    let directory = open_directory_chain(
        anchor,
        &parents,
        options.create_parents.then_some(options.dir_mode),
    )?;
    if let Some(status) =
        lstat_at(directory.as_fd(), &name).map_err(|source| failure(&display, "fstatat", source))?
    {
        vet_regular_file(&status, &display, anchor.follows())?;
        if let Some(expected_uid) = options.require_existing_owner
            && status.uid != expected_uid
        {
            return Err(StackError::WorkloadFsOwnerMismatch {
                path: display,
                expected_uid,
                actual_uid: status.uid,
            });
        }
    }
    let temp_name = temp_file_name(&parent_display)?;
    let temp_display = child_path(&parent_display, &temp_name);
    let descriptor = open_at(
        directory.as_fd(),
        &temp_name,
        CREATE_OPEN_FLAGS,
        options.file_mode,
    )
    .map_err(|source| failure(&temp_display, "openat", source))?;
    let published = fill_file(descriptor, &mut &content[..], &temp_display).and_then(|()| {
        rustix::fs::renameat(&directory, &temp_name, &directory, &name)
            .map_err(|source| failure(&display, "renameat", source))
    });
    if let Err(error) = published {
        discard_entry(directory.as_fd(), &temp_name, &temp_display);
        return Err(error);
    }
    rustix::fs::fsync(&directory).map_err(|source| failure(&parent_display, "fsync", source))
}

/// Create a new regular file; any existing entry at the target, symlinks included, is refused.
pub fn write_file_new(
    anchor: &Anchor,
    relative: &Path,
    content: &[u8],
    options: &WriteOptions,
) -> Result<()> {
    write_new_from_reader(anchor, relative, &mut &content[..], options)
}

/// Remove a regular file; directories, symlinks, and special files are refused.
pub fn remove_file(anchor: &Anchor, relative: &Path) -> Result<()> {
    let (parents, name) = entry_components(&anchor.path, relative)?;
    let display = joined(&anchor.path, &parents).join(OsStr::from_bytes(name.as_bytes()));
    let directory = open_directory_chain(anchor, &parents, None)?;
    let status = lstat_at(directory.as_fd(), &name)
        .map_err(|source| failure(&display, "fstatat", source))?
        .ok_or_else(|| StackError::WorkloadFsNotFound {
            path: display.clone(),
        })?;
    match status.kind {
        EntryKind::File => {}
        EntryKind::Symlink => return Err(StackError::WorkloadFsSymlinkRefused { path: display }),
        EntryKind::Dir | EntryKind::Other => {
            return Err(StackError::WorkloadFsNotRegular { path: display });
        }
    }
    rustix::fs::unlinkat(&directory, &name, AtFlags::empty())
        .map_err(|source| failure(&display, "unlinkat", source))
}

/// Remove an entry and, for a directory, everything below it. Symlinks are unlinked as links and
/// never followed, so their targets survive.
pub fn remove_tree(anchor: &Anchor, relative: &Path) -> Result<()> {
    let (parents, name) = entry_components(&anchor.path, relative)?;
    let display = joined(&anchor.path, &parents).join(OsStr::from_bytes(name.as_bytes()));
    let directory = open_directory_chain(anchor, &parents, None)?;
    let status = lstat_at(directory.as_fd(), &name)
        .map_err(|source| failure(&display, "fstatat", source))?
        .ok_or_else(|| StackError::WorkloadFsNotFound {
            path: display.clone(),
        })?;
    if status.kind != EntryKind::Dir {
        return rustix::fs::unlinkat(&directory, &name, AtFlags::empty())
            .map_err(|source| failure(&display, "unlinkat", source));
    }
    remove_directory_tree(directory, name, display)
}

/// Remove an empty directory; a non-empty one or a symlink is refused by the kernel.
pub fn remove_empty_dir(anchor: &Anchor, relative: &Path) -> Result<()> {
    let (parents, name) = entry_components(&anchor.path, relative)?;
    let display = joined(&anchor.path, &parents).join(OsStr::from_bytes(name.as_bytes()));
    let directory = open_directory_chain(anchor, &parents, None)?;
    rustix::fs::unlinkat(&directory, &name, AtFlags::REMOVEDIR)
        .map_err(|source| failure(&display, "unlinkat", source))
}

/// Create `relative` and any missing ancestors with `dir_mode`; existing directories are kept.
pub fn create_dir_all(anchor: &Anchor, relative: &Path, dir_mode: u32) -> Result<()> {
    let components = components_of(&anchor.path, relative)?;
    open_directory_chain(anchor, &components, Some(dir_mode)).map(drop)
}

/// Create a symlink at `relative` pointing at `target` verbatim.
pub fn symlink(anchor: &Anchor, relative: &Path, target: &Path) -> Result<()> {
    let (parents, name) = entry_components(&anchor.path, relative)?;
    let display = joined(&anchor.path, &parents).join(OsStr::from_bytes(name.as_bytes()));
    let target_bytes = target.as_os_str().as_bytes();
    if target_bytes.is_empty() {
        return Err(StackError::WorkloadFsInvalidPath {
            path: display,
            reason: "symlink target is empty",
        });
    }
    let c_target = CString::new(target_bytes).map_err(|_| StackError::WorkloadFsInvalidPath {
        path: display.clone(),
        reason: "symlink target contains a NUL byte",
    })?;
    let directory = open_directory_chain(anchor, &parents, None)?;
    rustix::fs::symlinkat(&c_target, &directory, &name)
        .map_err(|source| failure(&display, "symlinkat", source))
}

/// Target of the symlink at `relative`.
pub fn read_link(anchor: &Anchor, relative: &Path) -> Result<PathBuf> {
    let (parents, name) = entry_components(&anchor.path, relative)?;
    let display = joined(&anchor.path, &parents).join(OsStr::from_bytes(name.as_bytes()));
    let directory = open_directory_chain(anchor, &parents, None)?;
    read_link_at(directory.as_fd(), &name).map_err(|source| {
        if source == Errno::INVAL {
            StackError::WorkloadFsInvalidPath {
                path: display,
                reason: "target is not a symlink",
            }
        } else {
            failure(&display, "readlinkat", source)
        }
    })
}

/// `lstat` of `relative` (the anchor itself when empty); `None` when it or a parent is missing.
pub fn stat(anchor: &Anchor, relative: &Path) -> Result<Option<EntryInfo>> {
    let mut components = components_of(&anchor.path, relative)?;
    let Some(name) = components.pop() else {
        return fstat_entry(anchor.directory.as_fd())
            .map(Some)
            .map_err(|source| failure(&anchor.path, "fstat", source));
    };
    let display = joined(&anchor.path, &components).join(OsStr::from_bytes(name.as_bytes()));
    let directory = match open_directory_chain(anchor, &components, None) {
        Ok(directory) => directory,
        Err(StackError::WorkloadFsNotFound { .. }) => return Ok(None),
        Err(error) => return Err(error),
    };
    lstat_at(directory.as_fd(), &name).map_err(|source| failure(&display, "fstatat", source))
}

/// Entries of the directory at `relative` (the anchor itself when empty), sorted by name.
pub fn list_dir(anchor: &Anchor, relative: &Path) -> Result<Vec<(OsString, EntryInfo)>> {
    let components = components_of(&anchor.path, relative)?;
    let display = joined(&anchor.path, &components);
    let directory = open_directory_chain(anchor, &components, None)?;
    let mut entries = Vec::new();
    for name in read_names(directory.as_fd(), &display)? {
        let entry_display = child_path(&display, &name);
        // An entry removed between readdir and lstat is simply gone.
        if let Some(status) = lstat_at(directory.as_fd(), &name)
            .map_err(|source| failure(&entry_display, "fstatat", source))?
        {
            entries.push((OsString::from_vec(name.into_bytes()), status));
        }
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(entries)
}

/// Rename `from` to `to` without following a symlink at either end or along either parent chain.
pub fn rename(anchor: &Anchor, from: &Path, to: &Path) -> Result<()> {
    let (from_parents, from_name) = entry_components(&anchor.path, from)?;
    let (to_parents, to_name) = entry_components(&anchor.path, to)?;
    let from_display =
        joined(&anchor.path, &from_parents).join(OsStr::from_bytes(from_name.as_bytes()));
    let from_directory = open_directory_chain(anchor, &from_parents, None)?;
    let to_directory = open_directory_chain(anchor, &to_parents, None)?;
    rustix::fs::renameat(&from_directory, &from_name, &to_directory, &to_name)
        .map_err(|source| failure(&from_display, "renameat", source))
}

/// Create one new directory; its parent must exist and the name must be free.
pub(super) fn create_directory(anchor: &Anchor, relative: &Path, dir_mode: u32) -> Result<()> {
    let (parents, name) = entry_components(&anchor.path, relative)?;
    let display = joined(&anchor.path, &parents).join(OsStr::from_bytes(name.as_bytes()));
    let directory = open_directory_chain(anchor, &parents, None)?;
    rustix::fs::mkdirat(&directory, &name, mode_of(dir_mode))
        .map_err(|source| failure(&display, "mkdirat", source))
}

/// Create a new regular file filled from `source`; an existing entry of any kind is refused.
pub(super) fn write_new_from_reader(
    anchor: &Anchor,
    relative: &Path,
    source: &mut dyn Read,
    options: &WriteOptions,
) -> Result<()> {
    let (parents, name) = entry_components(&anchor.path, relative)?;
    let parent_display = joined(&anchor.path, &parents);
    let display = child_path(&parent_display, &name);
    let directory = open_directory_chain(
        anchor,
        &parents,
        options.create_parents.then_some(options.dir_mode),
    )?;
    // O_CREAT|O_EXCL never follows a symlink at the target.
    let descriptor = open_at(
        directory.as_fd(),
        &name,
        CREATE_OPEN_FLAGS,
        options.file_mode,
    )
    .map_err(|source| failure(&display, "openat", source))?;
    if let Err(error) = fill_file(descriptor, source, &display) {
        discard_entry(directory.as_fd(), &name, &display);
        return Err(error);
    }
    rustix::fs::fsync(&directory).map_err(|source| failure(&parent_display, "fsync", source))
}

/// Open a regular file for reading, single-link unless `allow_hard_links`. The caller has already
/// vetted the entry by lstat; the descriptor's own status is authoritative if the entry was swapped
/// in between.
pub(super) fn open_regular_file(
    directory: BorrowedFd<'_>,
    name: &CStr,
    display: &Path,
    allow_hard_links: bool,
) -> Result<(OwnedFd, EntryInfo)> {
    let descriptor = open_at(directory, name, READ_OPEN_FLAGS, 0)
        .map_err(|source| failure(display, "openat", source))?;
    let status =
        fstat_entry(descriptor.as_fd()).map_err(|source| failure(display, "fstat", source))?;
    vet_regular_file(&status, display, allow_hard_links)?;
    Ok((descriptor, status))
}

fn fill_file(descriptor: OwnedFd, source: &mut dyn Read, display: &Path) -> Result<()> {
    let mut file = File::from(descriptor);
    std::io::copy(source, &mut file).map_err(|error| failure(display, "write", error))?;
    file.sync_all()
        .map_err(|error| failure(display, "fsync", error))
}

/// Remove an entry this call created before failing; the caller reports the original error.
fn discard_entry(directory: BorrowedFd<'_>, name: &CStr, entry_path: &Path) {
    if let Err(error) = rustix::fs::unlinkat(directory, name, AtFlags::empty()) {
        tracing::warn!(
            %error,
            path = %entry_path.display(),
            "failed to remove a partially written workload file"
        );
    }
}

struct OpenDirectory {
    descriptor: OwnedFd,
    name: CString,
    display: PathBuf,
}

/// An ancestor of the directory being emptied: held open near the top of the tree, otherwise
/// released and recognised by device and inode when reopened through `..`.
enum AncestorDescriptor {
    Held(OwnedFd),
    Released { identity: (u64, u64) },
}

struct PendingAncestor {
    descriptor: AncestorDescriptor,
    name: CString,
    display: PathBuf,
}

/// Post-order removal with an explicit stack so a deep tree cannot exhaust the thread's stack, and
/// at most [`REMOVE_TREE_HELD_LEVELS`] ancestors held open so it cannot exhaust descriptors either.
fn remove_directory_tree(parent: OwnedFd, name: CString, display: PathBuf) -> Result<()> {
    let descriptor = open_directory_at(parent.as_fd(), &name)
        .map_err(|error| directory_open_failure(parent.as_fd(), &name, &display, error))?;
    let mut current = OpenDirectory {
        descriptor,
        name,
        display,
    };
    let mut ancestors: Vec<PendingAncestor> = Vec::new();
    loop {
        let directory = current.descriptor.as_fd();
        match remove_non_directories(directory, &current.display)? {
            Some(subdirectory) => {
                let subdirectory_display = child_path(&current.display, &subdirectory);
                let descriptor = open_directory_at(directory, &subdirectory).map_err(|error| {
                    directory_open_failure(directory, &subdirectory, &subdirectory_display, error)
                })?;
                let finished_level = std::mem::replace(
                    &mut current,
                    OpenDirectory {
                        descriptor,
                        name: subdirectory,
                        display: subdirectory_display,
                    },
                );
                let descriptor = if ancestors.len() < REMOVE_TREE_HELD_LEVELS {
                    AncestorDescriptor::Held(finished_level.descriptor)
                } else {
                    let identity = directory_identity(finished_level.descriptor.as_fd())
                        .map_err(|source| failure(&finished_level.display, "fstat", source))?;
                    AncestorDescriptor::Released { identity }
                };
                ancestors.push(PendingAncestor {
                    descriptor,
                    name: finished_level.name,
                    display: finished_level.display,
                });
            }
            None => {
                let Some(ancestor) = ancestors.pop() else {
                    drop(current.descriptor);
                    return rustix::fs::unlinkat(&parent, &current.name, AtFlags::REMOVEDIR)
                        .map_err(|source| failure(&current.display, "unlinkat", source));
                };
                let ancestor_descriptor = match ancestor.descriptor {
                    AncestorDescriptor::Held(descriptor) => descriptor,
                    AncestorDescriptor::Released { identity } => {
                        reopen_parent(directory, identity, &ancestor.display)?
                    }
                };
                drop(current.descriptor);
                rustix::fs::unlinkat(&ancestor_descriptor, &current.name, AtFlags::REMOVEDIR)
                    .map_err(|source| failure(&current.display, "unlinkat", source))?;
                current = OpenDirectory {
                    descriptor: ancestor_descriptor,
                    name: ancestor.name,
                    display: ancestor.display,
                };
            }
        }
    }
}

/// Reopen the parent of `child` through `..`, refusing it unless it is still the directory that
/// was released, so a subtree moved mid-removal cannot redirect it.
fn reopen_parent(child: BorrowedFd<'_>, identity: (u64, u64), display: &Path) -> Result<OwnedFd> {
    let parent = open_directory_at(child, PARENT_DIRECTORY)
        .map_err(|source| failure(display, "openat", source))?;
    let reopened =
        directory_identity(parent.as_fd()).map_err(|source| failure(display, "fstat", source))?;
    if reopened != identity {
        return Err(StackError::WorkloadFsInvalidPath {
            path: display.to_path_buf(),
            reason: "the directory moved during removal",
        });
    }
    Ok(parent)
}

// `dev_t` is i32 on macOS; the pair is only compared for equality.
#[allow(clippy::unnecessary_cast)]
fn directory_identity(descriptor: BorrowedFd<'_>) -> rustix::io::Result<(u64, u64)> {
    rustix::fs::fstat(descriptor).map(|status| (status.st_dev as u64, status.st_ino as u64))
}

/// Unlink every non-directory entry and return the first subdirectory still to be emptied.
fn remove_non_directories(directory: BorrowedFd<'_>, display: &Path) -> Result<Option<CString>> {
    let mut subdirectory = None;
    for name in read_names(directory, display)? {
        let entry_display = child_path(display, &name);
        match lstat_at(directory, &name)
            .map_err(|source| failure(&entry_display, "fstatat", source))?
        {
            None => {}
            Some(status) if status.kind == EntryKind::Dir => {
                if subdirectory.is_none() {
                    subdirectory = Some(name);
                }
            }
            Some(_) => rustix::fs::unlinkat(directory, &name, AtFlags::empty())
                .map_err(|source| failure(&entry_display, "unlinkat", source))?,
        }
    }
    Ok(subdirectory)
}

pub(super) fn failure(
    path: &Path,
    operation: &'static str,
    source: impl Into<std::io::Error>,
) -> StackError {
    let source = source.into();
    let path = path.to_path_buf();
    match source.raw_os_error() {
        Some(libc::ENOENT) => StackError::WorkloadFsNotFound { path },
        Some(libc::ELOOP) => StackError::WorkloadFsSymlinkRefused { path },
        Some(libc::EEXIST) => StackError::WorkloadFsAlreadyExists { path },
        _ => StackError::WorkloadFsIo {
            path,
            operation,
            source,
        },
    }
}

fn invalid_path(anchor_path: &Path, relative: &Path, reason: &'static str) -> StackError {
    StackError::WorkloadFsInvalidPath {
        path: anchor_path.join(relative),
        reason,
    }
}

/// Split `relative` into entry names, dropping empty and `.` segments. Absolute paths, `..`, and
/// NUL bytes are refused.
pub(super) fn components_of(anchor_path: &Path, relative: &Path) -> Result<Vec<CString>> {
    let bytes = relative.as_os_str().as_bytes();
    if bytes.contains(&0) {
        return Err(invalid_path(anchor_path, relative, "contains a NUL byte"));
    }
    if bytes.first() == Some(&b'/') {
        return Err(invalid_path(
            anchor_path,
            relative,
            "must be relative to the anchor",
        ));
    }
    let mut components = Vec::new();
    for segment in bytes.split(|byte| *byte == b'/') {
        match segment {
            b"" | b"." => {}
            b".." => {
                return Err(invalid_path(
                    anchor_path,
                    relative,
                    "contains a `..` component",
                ));
            }
            name => components.push(
                CString::new(name)
                    .map_err(|_| invalid_path(anchor_path, relative, "contains a NUL byte"))?,
            ),
        }
    }
    Ok(components)
}

/// [`components_of`] for operations on a named entry. The path must end in a name because `Path`
/// would collapse `dir/.` to `dir` and retarget the call.
fn entry_components(anchor_path: &Path, relative: &Path) -> Result<(Vec<CString>, CString)> {
    let bytes = relative.as_os_str().as_bytes();
    let mut end = bytes.len();
    while end > 0 && bytes[end - 1] == b'/' {
        end -= 1;
    }
    let trimmed = &bytes[..end];
    if trimmed == b"." || trimmed.ends_with(b"/.") {
        return Err(invalid_path(
            anchor_path,
            relative,
            "must end with a name, not `.`",
        ));
    }
    let mut components = components_of(anchor_path, relative)?;
    match components.pop() {
        Some(name) => Ok((components, name)),
        None => Err(invalid_path(
            anchor_path,
            relative,
            "must name an entry below the anchor",
        )),
    }
}

fn joined(base: &Path, components: &[CString]) -> PathBuf {
    let mut path = base.to_path_buf();
    for component in components {
        path.push(OsStr::from_bytes(component.as_bytes()));
    }
    path
}

fn child_path(base: &Path, name: &CStr) -> PathBuf {
    base.join(OsStr::from_bytes(name.to_bytes()))
}

/// Open every directory in `components` below `anchor`, following symlinks only as its
/// [`LinkPolicy`] allows, and creating missing ones with `create_mode` when set.
pub(super) fn open_directory_chain(
    anchor: &Anchor,
    components: &[CString],
    create_mode: Option<u32>,
) -> Result<OwnedFd> {
    anchor.require_contained(&joined(&anchor.path, components))?;
    let flags = if anchor.follows() {
        DIRECTORY_OPEN_FLAGS.difference(OFlags::NOFOLLOW)
    } else {
        DIRECTORY_OPEN_FLAGS
    };
    let mut current = anchor
        .directory
        .try_clone()
        .map_err(|source| failure(&anchor.path, "dup", source))?;
    for (index, name) in components.iter().enumerate() {
        let display = joined(&anchor.path, &components[..=index]);
        current = open_child_directory(current.as_fd(), name, flags, create_mode, &display)?;
    }
    Ok(current)
}

fn open_child_directory(
    parent: BorrowedFd<'_>,
    name: &CStr,
    flags: OFlags,
    create_mode: Option<u32>,
    display: &Path,
) -> Result<OwnedFd> {
    let open = || open_at(parent, name, flags, 0);
    match open() {
        Ok(directory) => Ok(directory),
        Err(Errno::NOENT) => {
            let Some(mode) = create_mode else {
                return Err(StackError::WorkloadFsNotFound {
                    path: display.to_path_buf(),
                });
            };
            match rustix::fs::mkdirat(parent, name, mode_of(mode)) {
                // A concurrent creator won; the no-follow reopen below still vets what it made.
                Ok(()) | Err(Errno::EXIST) => {}
                Err(error) => return Err(failure(display, "mkdirat", error)),
            }
            open().map_err(|error| directory_open_failure(parent, name, display, error))
        }
        Err(error) => Err(directory_open_failure(parent, name, display, error)),
    }
}

/// Linux reports ENOTDIR and macOS ELOOP for a no-follow directory open of a symlink, so lstat the
/// entry to tell a symlink from a plain non-directory.
fn directory_open_failure(
    parent: BorrowedFd<'_>,
    name: &CStr,
    display: &Path,
    error: Errno,
) -> StackError {
    match error {
        Errno::NOTDIR | Errno::LOOP => match lstat_at(parent, name) {
            Ok(Some(status)) if status.kind == EntryKind::Symlink => {
                StackError::WorkloadFsSymlinkRefused {
                    path: display.to_path_buf(),
                }
            }
            Ok(Some(_)) => StackError::WorkloadFsInvalidPath {
                path: display.to_path_buf(),
                reason: "a path component is not a directory",
            },
            Ok(None) => StackError::WorkloadFsNotFound {
                path: display.to_path_buf(),
            },
            Err(stat_error) => failure(display, "fstatat", stat_error),
        },
        _ => failure(display, "openat", error),
    }
}

fn vet_readable(anchor: &Anchor, status: &EntryInfo, display: &Path, max_bytes: u64) -> Result<()> {
    vet_regular_file(status, display, anchor.follows())?;
    if status.size > max_bytes {
        return Err(StackError::WorkspaceTooLarge { limit: max_bytes });
    }
    Ok(())
}

pub(super) fn vet_regular_file(
    status: &EntryInfo,
    display: &Path,
    allow_hard_links: bool,
) -> Result<()> {
    let path = || display.to_path_buf();
    match status.kind {
        EntryKind::File => {}
        EntryKind::Symlink => return Err(StackError::WorkloadFsSymlinkRefused { path: path() }),
        EntryKind::Dir | EntryKind::Other => {
            return Err(StackError::WorkloadFsNotRegular { path: path() });
        }
    }
    if !allow_hard_links && status.nlink > 1 {
        return Err(StackError::WorkloadFsHardLinkRefused { path: path() });
    }
    Ok(())
}

fn temp_file_name(parent_display: &Path) -> Result<CString> {
    let mut random = [0u8; TEMP_FILE_RANDOM_BYTES];
    rand::rng().fill(&mut random);
    let hex: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    CString::new(format!("{TEMP_FILE_PREFIX}{hex}{TEMP_FILE_SUFFIX}")).map_err(|_| {
        StackError::WorkloadFsInvalidPath {
            path: parent_display.to_path_buf(),
            reason: "temporary file name contains a NUL byte",
        }
    })
}

// `Stat` field widths differ across targets and rustix backends, so these conversions are lossless
// widenings on some targets and identities on others.
#[allow(clippy::useless_conversion, clippy::unnecessary_fallible_conversions)]
fn entry_info(status: &Stat) -> EntryInfo {
    EntryInfo {
        kind: match FileType::from_raw_mode(status.st_mode) {
            FileType::RegularFile => EntryKind::File,
            FileType::Directory => EntryKind::Dir,
            FileType::Symlink => EntryKind::Symlink,
            _ => EntryKind::Other,
        },
        uid: status.st_uid,
        gid: status.st_gid,
        mode: u32::from(status.st_mode) & PERMISSION_BITS,
        nlink: u64::try_from(status.st_nlink).unwrap_or(u64::MAX),
        size: u64::try_from(status.st_size).unwrap_or(0),
        modified: modification_time(
            i64::try_from(status.st_mtime).unwrap_or(0),
            i64::try_from(status.st_mtime_nsec).unwrap_or(0),
        ),
    }
}

fn modification_time(seconds: i64, nanoseconds: i64) -> SystemTime {
    let offset = Duration::from_secs(seconds.unsigned_abs());
    let whole_seconds = if seconds >= 0 {
        UNIX_EPOCH.checked_add(offset)
    } else {
        UNIX_EPOCH.checked_sub(offset)
    };
    whole_seconds
        .and_then(|time| {
            time.checked_add(Duration::from_nanos(
                u64::try_from(nanoseconds).unwrap_or(0),
            ))
        })
        .unwrap_or(UNIX_EPOCH)
}

/// Names in the directory, excluding `.` and `..`.
pub(super) fn read_names(directory: BorrowedFd<'_>, display: &Path) -> Result<Vec<CString>> {
    // Dir reads through its own open file description, so listings never share an offset.
    let listing =
        Dir::read_from(directory).map_err(|source| failure(display, "opendir", source))?;
    let mut names = Vec::new();
    for entry in listing {
        let entry = entry.map_err(|source| failure(display, "readdir", source))?;
        let name = entry.file_name();
        if name != CURRENT_DIRECTORY && name != PARENT_DIRECTORY {
            names.push(name.to_owned());
        }
    }
    Ok(names)
}

/// `openat` with `O_CLOEXEC`, retried on EINTR.
pub(super) fn open_at(
    directory: BorrowedFd<'_>,
    name: &CStr,
    flags: OFlags,
    mode: u32,
) -> rustix::io::Result<OwnedFd> {
    rustix::io::retry_on_intr(|| {
        rustix::fs::openat(directory, name, flags | OFlags::CLOEXEC, mode_of(mode))
    })
}

pub(super) fn open_directory_at(
    directory: BorrowedFd<'_>,
    name: &CStr,
) -> rustix::io::Result<OwnedFd> {
    open_at(directory, name, DIRECTORY_OPEN_FLAGS, 0)
}

// `mode_t` is u16 on macOS; masking to the permission bits first makes the narrowing lossless.
#[allow(clippy::unnecessary_cast)]
fn mode_of(bits: u32) -> Mode {
    Mode::from_raw_mode((bits & PERMISSION_BITS) as RawMode)
}

/// `lstat` relative to `directory`; `None` when the entry does not exist.
pub(super) fn lstat_at(
    directory: BorrowedFd<'_>,
    name: &CStr,
) -> rustix::io::Result<Option<EntryInfo>> {
    stat_at(directory, name, false)
}

/// `stat` relative to `directory`, following a symlink at `name` when `follow` is set; `None` when
/// the entry does not exist.
fn stat_at(
    directory: BorrowedFd<'_>,
    name: &CStr,
    follow: bool,
) -> rustix::io::Result<Option<EntryInfo>> {
    let flags = if follow {
        AtFlags::empty()
    } else {
        AtFlags::SYMLINK_NOFOLLOW
    };
    match rustix::fs::statat(directory, name, flags) {
        Ok(status) => Ok(Some(entry_info(&status))),
        Err(Errno::NOENT) => Ok(None),
        Err(error) => Err(error),
    }
}

fn fstat_entry(descriptor: BorrowedFd<'_>) -> rustix::io::Result<EntryInfo> {
    rustix::fs::fstat(descriptor).map(|status| entry_info(&status))
}

pub(super) fn read_link_at(directory: BorrowedFd<'_>, name: &CStr) -> rustix::io::Result<PathBuf> {
    rustix::fs::readlinkat(directory, name, Vec::new())
        .map(|target| PathBuf::from(OsString::from_vec(target.into_bytes())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    const MAX_READ: u64 = 1024;

    fn owner_only(create_parents: bool) -> WriteOptions {
        WriteOptions {
            create_parents,
            file_mode: OWNER_ONLY_FILE_MODE,
            dir_mode: OWNER_ONLY_DIR_MODE,
            require_existing_owner: None,
        }
    }

    fn fixture() -> (tempfile::TempDir, Anchor) {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let anchor = Anchor::open(tempdir.path()).expect("anchor");
        (tempdir, anchor)
    }

    fn make_fifo(path: &Path) {
        let c_path = CString::new(path.as_os_str().as_bytes()).expect("path");
        // SAFETY: `c_path` is NUL-terminated.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0, "mkfifo");
    }

    #[test]
    fn lexical_validation_refuses_unsafe_paths() {
        let (_tempdir, anchor) = fixture();
        let with_nul = Path::new(OsStr::from_bytes(b"a\0b"));
        for relative in [
            Path::new("/etc/passwd"),
            Path::new(""),
            Path::new("."),
            Path::new("a/../b"),
            Path::new(".."),
            Path::new("dir/."),
            Path::new("dir/./"),
            with_nul,
        ] {
            let error = write_file_atomic(&anchor, relative, b"x", &owner_only(true))
                .expect_err("unsafe path");
            assert!(
                matches!(error, StackError::WorkloadFsInvalidPath { .. }),
                "{relative:?}: {error:?}"
            );
        }
        assert!(matches!(
            list_dir(&anchor, Path::new("../outside")),
            Err(StackError::WorkloadFsInvalidPath { .. })
        ));
        assert!(
            list_dir(&anchor, Path::new(""))
                .expect("anchor listing")
                .is_empty()
        );
        assert_eq!(
            components_of(anchor.path(), Path::new("./a//b/./c/")).expect("components"),
            vec![c"a".to_owned(), c"b".to_owned(), c"c".to_owned()]
        );
    }

    #[test]
    fn symlink_at_the_target_is_refused_for_read_write_and_remove() {
        let (tempdir, anchor) = fixture();
        let outside = tempfile::tempdir().expect("outside");
        let secret = outside.path().join("secret");
        std::fs::write(&secret, b"outside").expect("secret");
        std::os::unix::fs::symlink(&secret, tempdir.path().join("link")).expect("link");

        assert!(matches!(
            read_file(&anchor, Path::new("link"), MAX_READ),
            Err(StackError::WorkloadFsSymlinkRefused { .. })
        ));
        assert!(matches!(
            write_file_atomic(&anchor, Path::new("link"), b"changed", &owner_only(false)),
            Err(StackError::WorkloadFsSymlinkRefused { .. })
        ));
        assert!(matches!(
            write_file_new(&anchor, Path::new("link"), b"changed", &owner_only(false)),
            Err(StackError::WorkloadFsAlreadyExists { .. })
        ));
        assert!(matches!(
            remove_file(&anchor, Path::new("link")),
            Err(StackError::WorkloadFsSymlinkRefused { .. })
        ));
        assert_eq!(std::fs::read(&secret).expect("secret"), b"outside");
        assert!(
            std::fs::symlink_metadata(tempdir.path().join("link"))
                .expect("link kept")
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn symlink_at_a_parent_component_is_refused() {
        let (tempdir, anchor) = fixture();
        let outside = tempfile::tempdir().expect("outside");
        let secret = outside.path().join("secret");
        std::fs::write(&secret, b"outside").expect("secret");
        std::os::unix::fs::symlink(outside.path(), tempdir.path().join("dir")).expect("link");

        assert!(matches!(
            read_file(&anchor, Path::new("dir/secret"), MAX_READ),
            Err(StackError::WorkloadFsSymlinkRefused { .. })
        ));
        assert!(matches!(
            write_file_atomic(
                &anchor,
                Path::new("dir/secret"),
                b"changed",
                &owner_only(true)
            ),
            Err(StackError::WorkloadFsSymlinkRefused { .. })
        ));
        assert!(matches!(
            write_file_new(&anchor, Path::new("dir/new"), b"changed", &owner_only(true)),
            Err(StackError::WorkloadFsSymlinkRefused { .. })
        ));
        assert!(matches!(
            remove_tree(&anchor, Path::new("dir/secret")),
            Err(StackError::WorkloadFsSymlinkRefused { .. })
        ));
        assert!(matches!(
            list_dir(&anchor, Path::new("dir")),
            Err(StackError::WorkloadFsSymlinkRefused { .. })
        ));
        assert!(matches!(
            stat(&anchor, Path::new("dir/secret")),
            Err(StackError::WorkloadFsSymlinkRefused { .. })
        ));
        assert_eq!(std::fs::read(&secret).expect("secret"), b"outside");
        assert!(!outside.path().join("new").exists());
    }

    #[test]
    fn hard_linked_target_is_refused_for_read_and_write() {
        let (tempdir, anchor) = fixture();
        let target = tempdir.path().join("target");
        std::fs::write(&target, b"original").expect("target");
        std::fs::hard_link(&target, tempdir.path().join("second")).expect("hard link");

        assert!(matches!(
            read_file(&anchor, Path::new("target"), MAX_READ),
            Err(StackError::WorkloadFsHardLinkRefused { .. })
        ));
        assert!(matches!(
            write_file_atomic(&anchor, Path::new("target"), b"changed", &owner_only(false)),
            Err(StackError::WorkloadFsHardLinkRefused { .. })
        ));
        assert_eq!(std::fs::read(&target).expect("target"), b"original");
    }

    #[test]
    fn non_regular_targets_are_refused() {
        let (tempdir, anchor) = fixture();
        make_fifo(&tempdir.path().join("fifo"));
        std::fs::create_dir(tempdir.path().join("dir")).expect("dir");

        assert!(matches!(
            read_file(&anchor, Path::new("fifo"), MAX_READ),
            Err(StackError::WorkloadFsNotRegular { .. })
        ));
        assert!(matches!(
            write_file_atomic(&anchor, Path::new("fifo"), b"x", &owner_only(false)),
            Err(StackError::WorkloadFsNotRegular { .. })
        ));
        assert!(matches!(
            read_file(&anchor, Path::new("dir"), MAX_READ),
            Err(StackError::WorkloadFsNotRegular { .. })
        ));
        assert!(matches!(
            remove_file(&anchor, Path::new("dir")),
            Err(StackError::WorkloadFsNotRegular { .. })
        ));
        assert!(matches!(
            write_file_atomic(&anchor, Path::new("fifo/child"), b"x", &owner_only(false)),
            Err(StackError::WorkloadFsInvalidPath { .. })
        ));
    }

    #[test]
    fn read_enforces_the_size_limit() {
        let (tempdir, anchor) = fixture();
        std::fs::write(tempdir.path().join("big"), vec![b'x'; 32]).expect("big");

        assert!(matches!(
            read_file(&anchor, Path::new("big"), 16),
            Err(StackError::WorkspaceTooLarge { limit: 16 })
        ));
        assert_eq!(
            read_file(&anchor, Path::new("big"), 32)
                .expect("fits")
                .len(),
            32
        );
        assert!(matches!(
            read_file(&anchor, Path::new("missing"), MAX_READ),
            Err(StackError::WorkloadFsNotFound { .. })
        ));
    }

    #[test]
    fn atomic_write_replaces_content_and_leaves_no_temp_files() {
        let (tempdir, anchor) = fixture();
        let options = owner_only(false);
        write_file_atomic(&anchor, Path::new("file"), b"first", &options).expect("create");
        write_file_atomic(&anchor, Path::new("file"), b"second", &options).expect("replace");

        assert_eq!(
            read_file(&anchor, Path::new("file"), MAX_READ).expect("read"),
            b"second"
        );
        let names: Vec<OsString> = list_dir(&anchor, Path::new(""))
            .expect("list")
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, vec![OsString::from("file")]);
        let mode = std::fs::metadata(tempdir.path().join("file"))
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, OWNER_ONLY_FILE_MODE);
    }

    #[test]
    fn atomic_write_checks_the_existing_owner() {
        let (tempdir, anchor) = fixture();
        std::fs::write(tempdir.path().join("file"), b"original").expect("file");
        let current = crate::ownership::process_euid();
        let mut options = owner_only(false);

        options.require_existing_owner = Some(current.wrapping_add(1));
        assert!(matches!(
            write_file_atomic(&anchor, Path::new("file"), b"changed", &options),
            Err(StackError::WorkloadFsOwnerMismatch { .. })
        ));
        options.require_existing_owner = Some(current);
        write_file_atomic(&anchor, Path::new("file"), b"changed", &options).expect("owner ok");
        assert_eq!(
            std::fs::read(tempdir.path().join("file")).expect("read"),
            b"changed"
        );
    }

    #[test]
    fn create_parents_builds_missing_directories() {
        let (tempdir, anchor) = fixture();
        assert!(matches!(
            write_file_atomic(&anchor, Path::new("a/b/file"), b"x", &owner_only(false)),
            Err(StackError::WorkloadFsNotFound { .. })
        ));
        write_file_atomic(&anchor, Path::new("a/b/file"), b"x", &owner_only(true))
            .expect("create parents");

        let directory = std::fs::symlink_metadata(tempdir.path().join("a/b")).expect("dir");
        assert!(directory.is_dir());
        assert_eq!(directory.mode() & 0o777, OWNER_ONLY_DIR_MODE);
        assert_eq!(
            std::fs::read(tempdir.path().join("a/b/file")).expect("file"),
            b"x"
        );

        create_dir_all(&anchor, Path::new("a/c/d"), OWNER_ONLY_DIR_MODE).expect("dir all");
        create_dir_all(&anchor, Path::new("a/c/d"), OWNER_ONLY_DIR_MODE).expect("idempotent");
        assert!(tempdir.path().join("a/c/d").is_dir());
    }

    #[test]
    fn write_file_new_refuses_an_existing_target() {
        let (tempdir, anchor) = fixture();
        write_file_new(&anchor, Path::new("backup"), b"one", &owner_only(false)).expect("new");
        assert!(matches!(
            write_file_new(&anchor, Path::new("backup"), b"two", &owner_only(false)),
            Err(StackError::WorkloadFsAlreadyExists { .. })
        ));
        assert_eq!(
            std::fs::read(tempdir.path().join("backup")).expect("read"),
            b"one"
        );
    }

    #[test]
    fn remove_tree_unlinks_symlinks_without_following_them() {
        let (tempdir, anchor) = fixture();
        let outside = tempfile::tempdir().expect("outside");
        let kept = outside.path().join("kept");
        std::fs::write(&kept, b"keep me").expect("kept");
        let tree = tempdir.path().join("tree");
        std::fs::create_dir_all(tree.join("nested/deeper")).expect("tree");
        std::fs::write(tree.join("file"), b"x").expect("file");
        std::fs::write(tree.join("nested/deeper/file"), b"x").expect("deep file");
        std::os::unix::fs::symlink(outside.path(), tree.join("nested/to-outside")).expect("link");
        std::os::unix::fs::symlink(&kept, tree.join("to-file")).expect("file link");

        remove_tree(&anchor, Path::new("tree")).expect("remove tree");

        assert!(!tree.exists());
        assert_eq!(std::fs::read(&kept).expect("kept"), b"keep me");
        assert!(matches!(
            remove_tree(&anchor, Path::new("tree")),
            Err(StackError::WorkloadFsNotFound { .. })
        ));
    }

    #[test]
    fn copy_file_streams_content_and_masks_the_source_mode() {
        let (tempdir, anchor) = fixture();
        let content = vec![b'x'; 3 * 1024 * 1024];
        std::fs::write(tempdir.path().join("source"), &content).expect("source");
        std::fs::set_permissions(
            tempdir.path().join("source"),
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("chmod source");
        let options = WriteOptions {
            create_parents: false,
            file_mode: 0o750,
            dir_mode: 0o700,
            require_existing_owner: None,
        };

        copy_file(&anchor, Path::new("source"), Path::new("copy"), &options).expect("copy");

        let copy = tempdir.path().join("copy");
        assert_eq!(std::fs::read(&copy).expect("copy content"), content);
        assert_eq!(
            std::fs::metadata(&copy).expect("copy metadata").mode() & 0o777,
            0o750
        );
        assert!(matches!(
            copy_file(&anchor, Path::new("source"), Path::new("copy"), &options),
            Err(StackError::WorkloadFsAlreadyExists { .. })
        ));
    }

    #[test]
    fn remove_tree_climbs_past_the_held_levels_and_removes_siblings_below_them() {
        let (_tempdir, anchor) = fixture();
        let options = WriteOptions {
            create_parents: false,
            file_mode: 0o600,
            dir_mode: 0o700,
            require_existing_owner: None,
        };
        let depth = REMOVE_TREE_HELD_LEVELS * 3 + 5;
        let mut level = PathBuf::from("tree");
        for index in 0..depth {
            level.push("d");
            create_dir_all(&anchor, &level, options.dir_mode).expect("level");
            write_file_new(&anchor, &level.join("file"), b"x", &options).expect("file");
            // A second subdirectory past the held levels is only reached by climbing back up.
            if index % REMOVE_TREE_HELD_LEVELS == REMOVE_TREE_HELD_LEVELS - 1 {
                create_dir_all(&anchor, &level.join("sibling/inner"), options.dir_mode)
                    .expect("sibling");
            }
        }

        remove_tree(&anchor, Path::new("tree")).expect("remove deep tree");

        assert_eq!(stat(&anchor, Path::new("tree")).expect("stat"), None);
    }

    #[test]
    fn symlink_read_link_stat_and_rename_never_follow_links() {
        let (tempdir, anchor) = fixture();
        symlink(&anchor, Path::new("link"), Path::new("../outside/target")).expect("symlink");
        assert_eq!(
            read_link(&anchor, Path::new("link")).expect("read link"),
            PathBuf::from("../outside/target")
        );
        assert!(matches!(
            symlink(&anchor, Path::new("link"), Path::new("other")),
            Err(StackError::WorkloadFsAlreadyExists { .. })
        ));
        let info = stat(&anchor, Path::new("link"))
            .expect("stat")
            .expect("exists");
        assert_eq!(info.kind, EntryKind::Symlink);

        std::fs::write(tempdir.path().join("file"), b"x").expect("file");
        assert!(matches!(
            read_link(&anchor, Path::new("file")),
            Err(StackError::WorkloadFsInvalidPath { .. })
        ));
        rename(&anchor, Path::new("link"), Path::new("moved")).expect("rename");
        assert!(
            std::fs::symlink_metadata(tempdir.path().join("moved"))
                .expect("moved")
                .file_type()
                .is_symlink()
        );

        assert_eq!(
            stat(&anchor, Path::new("missing/deeper")).expect("stat"),
            None
        );
        let root = stat(&anchor, Path::new("")).expect("stat").expect("anchor");
        assert_eq!(root.kind, EntryKind::Dir);
        let file = stat(&anchor, Path::new("file"))
            .expect("stat")
            .expect("file");
        assert_eq!((file.kind, file.size, file.nlink), (EntryKind::File, 1, 1));
        assert_eq!(file.uid, crate::ownership::process_euid());
    }

    #[test]
    fn list_dir_reports_entry_kinds_sorted_by_name() {
        let (tempdir, anchor) = fixture();
        std::fs::create_dir(tempdir.path().join("b-dir")).expect("dir");
        std::fs::write(tempdir.path().join("a-file"), b"abc").expect("file");
        std::os::unix::fs::symlink("a-file", tempdir.path().join("c-link")).expect("link");

        let entries = list_dir(&anchor, Path::new(".")).expect("list");
        let summary: Vec<(OsString, EntryKind)> = entries
            .into_iter()
            .map(|(name, info)| (name, info.kind))
            .collect();
        assert_eq!(
            summary,
            vec![
                (OsString::from("a-file"), EntryKind::File),
                (OsString::from("b-dir"), EntryKind::Dir),
                (OsString::from("c-link"), EntryKind::Symlink),
            ]
        );
        assert!(matches!(
            list_dir(&anchor, Path::new("a-file")),
            Err(StackError::WorkloadFsInvalidPath { .. })
        ));
    }
}
