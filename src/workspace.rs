//! Workspace file operations under `workspace.root`: path resolution rejecting traversal, NUL
//! bytes, absolute prefixes, and symlink escapes, plus the sync list/read/write/delete primitives
//! the HTTP handlers run in `spawn_blocking`. Residual TOCTOU is accepted: a local actor with
//! write access to the root can swap entries between resolve and the following syscall.

use std::fs::Metadata;
use std::io::{ErrorKind, Read};
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::error::{Result, StackError};
use crate::fs_util::atomic_write_owner_only;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathIntent {
    ReadExisting,
    WriteOrCreate,
}

/// Resolve a workspace-relative `requested` path to an absolute filesystem path inside `root`,
/// which must already exist.
pub fn resolve_workspace_path(root: &Path, requested: &str, intent: PathIntent) -> Result<PathBuf> {
    if requested.contains('\0') {
        return Err(StackError::WorkspacePathInvalid {
            reason: "contains NUL byte".to_owned(),
            requested: requested.to_owned(),
        });
    }

    let requested_path = Path::new(requested);
    let mut normal_count = 0usize;
    for component in requested_path.components() {
        match component {
            Component::ParentDir => {
                return Err(StackError::WorkspacePathInvalid {
                    reason: "contains `..` segment".to_owned(),
                    requested: requested.to_owned(),
                });
            }
            Component::Prefix(_) | Component::RootDir => {
                return Err(StackError::WorkspacePathInvalid {
                    reason: "must be a workspace-relative path".to_owned(),
                    requested: requested.to_owned(),
                });
            }
            Component::CurDir => {}
            Component::Normal(_) => normal_count += 1,
        }
    }
    // Rust's `Path` API silently retargets both of these: a path normalizing to the root itself
    // resolves to a sibling of the root, and a trailing `.` (`subdir/.`) collapses to `subdir`,
    // so the write would land somewhere other than the path the caller named.
    if matches!(intent, PathIntent::WriteOrCreate) {
        if normal_count == 0 {
            return Err(StackError::WorkspacePathInvalid {
                reason: "must name a specific file inside the workspace".to_owned(),
                requested: requested.to_owned(),
            });
        }
        let trimmed = requested.trim_end_matches('/');
        if trimmed == "." || trimmed.ends_with("/.") {
            return Err(StackError::WorkspacePathInvalid {
                reason: "path must end with a file name, not `.`".to_owned(),
                requested: requested.to_owned(),
            });
        }
    }

    let canonical_root = root.canonicalize().map_err(|source| {
        if source.kind() == ErrorKind::NotFound {
            StackError::WorkspaceNotFound {
                requested: requested.to_owned(),
            }
        } else {
            StackError::WorkspaceIo {
                requested: requested.to_owned(),
                source,
            }
        }
    })?;
    let joined = canonical_root.join(requested_path);

    match intent {
        PathIntent::ReadExisting => {
            let canonical = canonicalize_or_translate(&joined, requested, intent)?;
            if !canonical.starts_with(&canonical_root) {
                return Err(StackError::WorkspaceSymlinkEscape {
                    requested: requested.to_owned(),
                });
            }
            Ok(canonical)
        }
        PathIntent::WriteOrCreate => {
            let parent = joined
                .parent()
                .ok_or_else(|| StackError::WorkspacePathInvalid {
                    reason: "has no parent directory".to_owned(),
                    requested: requested.to_owned(),
                })?;
            let canonical_parent = canonicalize_or_translate(parent, requested, intent)?;
            if !canonical_parent.starts_with(&canonical_root) {
                return Err(StackError::WorkspaceSymlinkEscape {
                    requested: requested.to_owned(),
                });
            }
            // `canonicalize` resolves through a regular file, so the parent can canonicalize and
            // still not be a directory.
            let parent_metadata =
                std::fs::metadata(&canonical_parent).map_err(|source| StackError::WorkspaceIo {
                    requested: requested.to_owned(),
                    source,
                })?;
            if !parent_metadata.is_dir() {
                return Err(StackError::WorkspacePathInvalid {
                    reason: "intermediate component is not a directory".to_owned(),
                    requested: requested.to_owned(),
                });
            }
            let final_name =
                joined
                    .file_name()
                    .ok_or_else(|| StackError::WorkspacePathInvalid {
                        reason: "has no file name".to_owned(),
                        requested: requested.to_owned(),
                    })?;
            let resolved = canonical_parent.join(final_name);
            // Refuse to overwrite an existing symlink; `symlink_metadata` sees the link itself.
            if let Ok(metadata) = std::fs::symlink_metadata(&resolved)
                && metadata.file_type().is_symlink()
            {
                return Err(StackError::WorkspaceSymlinkEscape {
                    requested: requested.to_owned(),
                });
            }
            Ok(resolved)
        }
    }
}

/// Resolve an ABSOLUTE path (as ACP `fs/*` methods send) to a verified path inside `root`. The
/// remainder MUST route through `resolve_workspace_path`: this entry point must never be weaker.
pub fn resolve_workspace_abs_path(
    root: &Path,
    requested: &Path,
    intent: PathIntent,
) -> Result<PathBuf> {
    let display = requested.to_string_lossy().into_owned();
    if !requested.is_absolute() {
        return Err(StackError::WorkspacePathInvalid {
            reason: "must be an absolute path".to_owned(),
            requested: display,
        });
    }
    let canonical_root = root.canonicalize().map_err(|source| {
        if source.kind() == ErrorKind::NotFound {
            StackError::WorkspaceNotFound {
                requested: display.clone(),
            }
        } else {
            StackError::WorkspaceIo {
                requested: display.clone(),
                source,
            }
        }
    })?;
    // Accept both the canonical root and the configured (possibly symlinked)
    // spelling: agents echo back whatever cwd they were given at session/new.
    let relative = requested
        .strip_prefix(&canonical_root)
        .or_else(|_| requested.strip_prefix(root))
        .map_err(|_| StackError::WorkspacePathInvalid {
            reason: "outside the session workspace".to_owned(),
            requested: display.clone(),
        })?;
    let relative_str = relative
        .to_str()
        .ok_or_else(|| StackError::WorkspacePathInvalid {
            reason: "not valid UTF-8".to_owned(),
            requested: display.clone(),
        })?;
    resolve_workspace_path(root, relative_str, intent)
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceEntry {
    pub name: String,
    pub kind: EntryKind,
    pub size: Option<u64>,
    pub modified: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceListing {
    pub entries: Vec<WorkspaceEntry>,
}

#[derive(Debug, Clone)]
pub struct FileRead {
    pub content: Vec<u8>,
    pub size: u64,
    pub modified: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileMetadata {
    pub size: u64,
    pub modified: DateTime<Utc>,
}

/// List the entries of an already-resolved `absolute_path`, sorted directories-first then by
/// name. Symlinks are reported as `EntryKind::Symlink` and are not traversed.
pub fn list_directory(absolute_path: &Path) -> Result<WorkspaceListing> {
    let metadata = std::fs::metadata(absolute_path).map_err(|source| StackError::WorkspaceIo {
        requested: display_relative(absolute_path),
        source,
    })?;
    if !metadata.is_dir() {
        return Err(StackError::WorkspacePathInvalid {
            reason: "target is not a directory".to_owned(),
            requested: display_relative(absolute_path),
        });
    }

    let read_dir = std::fs::read_dir(absolute_path).map_err(|source| StackError::WorkspaceIo {
        requested: display_relative(absolute_path),
        source,
    })?;

    let mut entries = Vec::new();
    for raw in read_dir {
        let entry = raw.map_err(|source| StackError::WorkspaceIo {
            requested: display_relative(absolute_path),
            source,
        })?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let symlink_meta =
            entry
                .path()
                .symlink_metadata()
                .map_err(|source| StackError::WorkspaceIo {
                    requested: name.clone(),
                    source,
                })?;
        let kind = classify(&symlink_meta);
        let modified = system_time_to_utc(symlink_meta.modified().map_err(|source| {
            StackError::WorkspaceIo {
                requested: name.clone(),
                source,
            }
        })?);
        let size = match kind {
            EntryKind::File => Some(symlink_meta.len()),
            _ => None,
        };
        entries.push(WorkspaceEntry {
            name,
            kind,
            size,
            modified,
        });
    }
    entries.sort_by_key(sort_key);
    Ok(WorkspaceListing { entries })
}

/// Read at most `max_bytes` of an existing regular file. The post-read size re-check defends
/// against a concurrent writer growing the file after the metadata check.
pub fn read_file(absolute_path: &Path, max_bytes: u64) -> Result<FileRead> {
    // Stat first, open second: opening a FIFO/socket for read blocks indefinitely on Unix and
    // would tie up a tokio blocking thread.
    let metadata = std::fs::metadata(absolute_path).map_err(|source| {
        if source.kind() == ErrorKind::NotFound {
            StackError::WorkspaceNotFound {
                requested: display_relative(absolute_path),
            }
        } else {
            StackError::WorkspaceIo {
                requested: display_relative(absolute_path),
                source,
            }
        }
    })?;
    if !metadata.is_file() {
        return Err(StackError::WorkspacePathInvalid {
            reason: "target is not a regular file".to_owned(),
            requested: display_relative(absolute_path),
        });
    }
    if metadata.len() > max_bytes {
        return Err(StackError::WorkspaceTooLarge { limit: max_bytes });
    }
    let mut file = open_no_follow(absolute_path).map_err(|source| {
        // ELOOP from O_NOFOLLOW means a symlink appeared at the final component between the
        // metadata check and the open.
        if source.raw_os_error() == Some(libc::ELOOP) {
            StackError::WorkspaceSymlinkEscape {
                requested: display_relative(absolute_path),
            }
        } else {
            StackError::WorkspaceIo {
                requested: display_relative(absolute_path),
                source,
            }
        }
    })?;
    let modified =
        system_time_to_utc(
            metadata
                .modified()
                .map_err(|source| StackError::WorkspaceIo {
                    requested: display_relative(absolute_path),
                    source,
                })?,
        );
    let cap = usize::try_from(max_bytes.saturating_add(1)).unwrap_or(usize::MAX);
    let mut buffer = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    file.by_ref()
        .take(cap as u64)
        .read_to_end(&mut buffer)
        .map_err(|source| StackError::WorkspaceIo {
            requested: display_relative(absolute_path),
            source,
        })?;
    if buffer.len() as u64 > max_bytes {
        return Err(StackError::WorkspaceTooLarge { limit: max_bytes });
    }
    let size = buffer.len() as u64;
    Ok(FileRead {
        content: buffer,
        size,
        modified,
    })
}

/// Atomically write `content` to `absolute_path`, returning the post-write size and mtime.
pub fn write_file_atomic(absolute_path: &Path, content: &[u8]) -> Result<FileMetadata> {
    if let Ok(metadata) = std::fs::symlink_metadata(absolute_path)
        && metadata.file_type().is_dir()
    {
        return Err(StackError::WorkspacePathInvalid {
            reason: "target is a directory; refusing to write".to_owned(),
            requested: display_relative(absolute_path),
        });
    }
    if let Some(parent) = absolute_path.parent()
        && !parent.is_dir()
    {
        return Err(StackError::WorkspaceParentNotFound {
            requested: display_relative(absolute_path),
        });
    }
    atomic_write_owner_only(absolute_path, content).map_err(translate_atomic_write_error)?;
    let metadata = std::fs::metadata(absolute_path).map_err(|source| StackError::WorkspaceIo {
        requested: display_relative(absolute_path),
        source,
    })?;
    Ok(FileMetadata {
        size: metadata.len(),
        modified: system_time_to_utc(metadata.modified().map_err(|source| {
            StackError::WorkspaceIo {
                requested: display_relative(absolute_path),
                source,
            }
        })?),
    })
}

/// Remove a regular file at `absolute_path`, refusing directories and symlinks.
pub fn delete_file(absolute_path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(absolute_path).map_err(|source| {
        if source.kind() == ErrorKind::NotFound {
            StackError::WorkspaceNotFound {
                requested: display_relative(absolute_path),
            }
        } else {
            StackError::WorkspaceIo {
                requested: display_relative(absolute_path),
                source,
            }
        }
    })?;
    let file_type = metadata.file_type();
    if file_type.is_dir() {
        return Err(StackError::WorkspacePathInvalid {
            reason: "target is a directory; refusing to remove recursively".to_owned(),
            requested: display_relative(absolute_path),
        });
    }
    if file_type.is_symlink() {
        return Err(StackError::WorkspaceSymlinkEscape {
            requested: display_relative(absolute_path),
        });
    }
    if !file_type.is_file() {
        return Err(StackError::WorkspacePathInvalid {
            reason: "target is not a regular file".to_owned(),
            requested: display_relative(absolute_path),
        });
    }
    std::fs::remove_file(absolute_path).map_err(|source| StackError::WorkspaceIo {
        requested: display_relative(absolute_path),
        source,
    })
}

/// Canonicalize a path, translating client-shaped `std::io` errors into 4xx workspace errors.
fn canonicalize_or_translate(path: &Path, requested: &str, intent: PathIntent) -> Result<PathBuf> {
    match path.canonicalize() {
        Ok(canonical) => Ok(canonical),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if matches!(intent, PathIntent::WriteOrCreate) {
                return Err(StackError::WorkspaceParentNotFound {
                    requested: requested.to_owned(),
                });
            }
            Err(StackError::WorkspaceNotFound {
                requested: requested.to_owned(),
            })
        }
        Err(error)
            if matches!(error.kind(), std::io::ErrorKind::NotADirectory)
                || error.raw_os_error() == Some(libc::ENOTDIR) =>
        {
            Err(StackError::WorkspacePathInvalid {
                reason: "intermediate component is not a directory".to_owned(),
                requested: requested.to_owned(),
            })
        }
        Err(source) => Err(StackError::WorkspaceIo {
            requested: requested.to_owned(),
            source,
        }),
    }
}

/// Open with `O_NOFOLLOW | O_NONBLOCK` so a final-component swap between the resolve-time metadata
/// check and this open is caught here: a symlink returns ELOOP, and a FIFO/socket returns
/// immediately instead of blocking, to be rejected by the post-open `fstat`.
fn open_no_follow(absolute_path: &Path) -> std::io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(absolute_path)?;
        // Re-stat through the open handle so a race-substituted non-regular file is rejected.
        let metadata = file.metadata()?;
        let mode = metadata.mode();
        // libc exposes these constants with target-specific integer types.
        #[allow(clippy::unnecessary_cast)]
        let file_type_mask = libc::S_IFMT as u32;
        #[allow(clippy::unnecessary_cast)]
        let regular_file_mode = libc::S_IFREG as u32;
        if mode & file_type_mask != regular_file_mode {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "workspace open refused non-regular file after race-check",
            ));
        }
        Ok(file)
    }
    #[cfg(not(unix))]
    {
        std::fs::File::open(absolute_path)
    }
}

fn classify(metadata: &Metadata) -> EntryKind {
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        EntryKind::Symlink
    } else if file_type.is_dir() {
        EntryKind::Directory
    } else if file_type.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    }
}

fn sort_key(entry: &WorkspaceEntry) -> (u8, String) {
    let bucket = match entry.kind {
        EntryKind::Directory => 0,
        EntryKind::File => 1,
        EntryKind::Symlink => 2,
        EntryKind::Other => 3,
    };
    (bucket, entry.name.clone())
}

fn system_time_to_utc(time: SystemTime) -> DateTime<Utc> {
    DateTime::<Utc>::from(time)
}

fn display_relative(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Translate `atomic_write_owner_only` errors into workspace-domain `workspace.*` errors.
fn translate_atomic_write_error(error: StackError) -> StackError {
    let requested = "<workspace target>".to_owned();
    match error {
        StackError::FileCreate { source, .. } | StackError::PermissionSet { source, .. } => {
            StackError::WorkspaceIo { requested, source }
        }
        StackError::MissingParentDir { .. } => StackError::WorkspaceNotFound { requested },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn workspace_root() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn rejects_parent_traversal() {
        let root = workspace_root();
        let error = resolve_workspace_path(root.path(), "../etc/passwd", PathIntent::ReadExisting)
            .expect_err("traversal should be rejected");
        assert!(matches!(
            error,
            StackError::WorkspacePathInvalid { reason, .. } if reason.contains("..")
        ));
    }

    #[test]
    fn rejects_nul_byte_in_path() {
        let root = workspace_root();
        let error = resolve_workspace_path(root.path(), "a\0b", PathIntent::ReadExisting)
            .expect_err("NUL byte should be rejected");
        assert!(matches!(
            error,
            StackError::WorkspacePathInvalid { reason, .. } if reason.contains("NUL")
        ));
    }

    #[test]
    fn rejects_curdir_for_writes() {
        let root = workspace_root();
        let error = resolve_workspace_path(root.path(), ".", PathIntent::WriteOrCreate)
            .expect_err("`.` for writes should be rejected");
        assert!(matches!(
            error,
            StackError::WorkspacePathInvalid { reason, .. } if reason.contains("specific file")
        ));
    }

    #[test]
    fn write_atomic_refuses_directory_target() {
        let root = workspace_root();
        std::fs::create_dir(root.path().join("subdir")).expect("mkdir");
        let target = root.path().join("subdir");
        let error = write_file_atomic(&target, b"oops").expect_err("should refuse directory");
        assert!(matches!(
            error,
            StackError::WorkspacePathInvalid { reason, .. }
                if reason.contains("directory")
        ));
    }

    #[test]
    fn allows_curdir_for_reads_of_root() {
        let root = workspace_root();
        let resolved = resolve_workspace_path(root.path(), ".", PathIntent::ReadExisting)
            .expect("listing the root via `.` should work");
        assert_eq!(
            resolved,
            fs::canonicalize(root.path()).expect("canonicalize")
        );
    }

    #[test]
    fn rejects_absolute_paths() {
        let root = workspace_root();
        let error = resolve_workspace_path(root.path(), "/etc/passwd", PathIntent::ReadExisting)
            .expect_err("absolute path should be rejected");
        assert!(matches!(
            error,
            StackError::WorkspacePathInvalid { reason, .. }
                if reason.contains("workspace-relative")
        ));
    }

    #[test]
    fn read_existing_returns_canonical_path() {
        let root = workspace_root();
        let file = root.path().join("hello.txt");
        fs::write(&file, b"hi").expect("write");

        let resolved = resolve_workspace_path(root.path(), "hello.txt", PathIntent::ReadExisting)
            .expect("resolve");
        assert_eq!(resolved, fs::canonicalize(&file).expect("canonicalize"));
    }

    #[test]
    fn read_existing_with_file_as_intermediate_returns_path_invalid() {
        let root = workspace_root();
        fs::write(root.path().join("plain.txt"), b"data").expect("write");
        let error =
            resolve_workspace_path(root.path(), "plain.txt/child", PathIntent::ReadExisting)
                .expect_err("intermediate file should not be treated as a directory");
        assert!(matches!(
            error,
            StackError::WorkspacePathInvalid { reason, .. }
                if reason.contains("not a directory")
        ));
    }

    #[test]
    fn write_or_create_with_file_as_intermediate_returns_path_invalid() {
        let root = workspace_root();
        fs::write(root.path().join("plain.txt"), b"data").expect("write");
        let error =
            resolve_workspace_path(root.path(), "plain.txt/child", PathIntent::WriteOrCreate)
                .expect_err("intermediate file should not be treated as a directory");
        assert!(matches!(
            error,
            StackError::WorkspacePathInvalid { reason, .. }
                if reason.contains("not a directory")
        ));
    }

    #[test]
    fn write_or_create_rejects_trailing_dot_segment() {
        let root = workspace_root();
        fs::create_dir(root.path().join("subdir")).expect("mkdir");
        let error = resolve_workspace_path(root.path(), "subdir/.", PathIntent::WriteOrCreate)
            .expect_err("`subdir/.` should be rejected for writes");
        assert!(matches!(
            error,
            StackError::WorkspacePathInvalid { reason, .. } if reason.contains("file name")
        ));
    }

    #[test]
    fn read_existing_missing_returns_not_found() {
        let root = workspace_root();
        let error = resolve_workspace_path(root.path(), "missing.txt", PathIntent::ReadExisting)
            .expect_err("missing file should 404");
        assert!(matches!(error, StackError::WorkspaceNotFound { .. }));
    }

    #[test]
    fn write_or_create_requires_existing_parent() {
        let root = workspace_root();
        let error =
            resolve_workspace_path(root.path(), "nested/new.txt", PathIntent::WriteOrCreate)
                .expect_err("missing parent should 404");
        assert!(matches!(error, StackError::WorkspaceParentNotFound { .. }));
    }

    #[test]
    fn write_or_create_accepts_new_file_in_existing_dir() {
        let root = workspace_root();
        let resolved = resolve_workspace_path(root.path(), "new.txt", PathIntent::WriteOrCreate)
            .expect("resolve");
        assert_eq!(
            resolved.parent().expect("parent"),
            fs::canonicalize(root.path()).expect("canonicalize")
        );
        assert_eq!(resolved.file_name().expect("file_name"), "new.txt");
    }

    #[cfg(unix)]
    #[test]
    fn read_existing_rejects_symlink_that_escapes_root() {
        use std::os::unix::fs::symlink;
        let root = workspace_root();
        let outside = tempfile::tempdir().expect("outside tempdir");
        let outside_target = outside.path().join("target");
        fs::write(&outside_target, b"leak").expect("write outside");
        symlink(&outside_target, root.path().join("link")).expect("symlink");

        let error = resolve_workspace_path(root.path(), "link", PathIntent::ReadExisting)
            .expect_err("escape should be rejected");
        assert!(matches!(error, StackError::WorkspaceSymlinkEscape { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn write_or_create_refuses_existing_symlink_at_target() {
        use std::os::unix::fs::symlink;
        let root = workspace_root();
        let outside = tempfile::tempdir().expect("outside tempdir");
        let outside_target = outside.path().join("target");
        fs::write(&outside_target, b"leak").expect("write outside");
        symlink(&outside_target, root.path().join("link")).expect("symlink");

        let error = resolve_workspace_path(root.path(), "link", PathIntent::WriteOrCreate)
            .expect_err("symlink overwrite should be rejected");
        assert!(matches!(error, StackError::WorkspaceSymlinkEscape { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn read_existing_allows_symlink_that_stays_inside_root() {
        use std::os::unix::fs::symlink;
        let root = workspace_root();
        let target = root.path().join("real.txt");
        fs::write(&target, b"ok").expect("write target");
        symlink(&target, root.path().join("inner-link")).expect("symlink");

        let resolved = resolve_workspace_path(root.path(), "inner-link", PathIntent::ReadExisting)
            .expect("resolve");
        assert_eq!(resolved, fs::canonicalize(&target).expect("canonicalize"));
    }

    #[test]
    fn list_directory_sorts_directories_before_files_then_by_name() {
        let root = workspace_root();
        fs::write(root.path().join("zzz.txt"), b"").expect("write");
        fs::write(root.path().join("aaa.txt"), b"").expect("write");
        fs::create_dir(root.path().join("zdir")).expect("mkdir z");
        fs::create_dir(root.path().join("adir")).expect("mkdir a");

        let listing = list_directory(root.path()).expect("list");
        let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["adir", "zdir", "aaa.txt", "zzz.txt"]);
    }

    #[test]
    fn list_directory_reports_file_sizes_but_not_directory_sizes() {
        let root = workspace_root();
        fs::write(root.path().join("a.bin"), b"hello").expect("write");
        fs::create_dir(root.path().join("sub")).expect("mkdir");
        let listing = list_directory(root.path()).expect("list");

        let dir = listing
            .entries
            .iter()
            .find(|e| e.name == "sub")
            .expect("sub");
        assert_eq!(dir.kind, EntryKind::Directory);
        assert!(dir.size.is_none());

        let file = listing
            .entries
            .iter()
            .find(|e| e.name == "a.bin")
            .expect("a.bin");
        assert_eq!(file.kind, EntryKind::File);
        assert_eq!(file.size, Some(5));
    }

    #[cfg(unix)]
    #[test]
    fn list_directory_reports_symlinks_without_following_them() {
        use std::os::unix::fs::symlink;
        let root = workspace_root();
        fs::write(root.path().join("real"), b"x").expect("write");
        symlink(root.path().join("real"), root.path().join("alias")).expect("symlink");

        let listing = list_directory(root.path()).expect("list");
        let alias = listing
            .entries
            .iter()
            .find(|e| e.name == "alias")
            .expect("alias");
        assert_eq!(alias.kind, EntryKind::Symlink);
    }

    #[test]
    fn read_file_returns_content_and_size() {
        let root = workspace_root();
        let path = root.path().join("greeting.txt");
        fs::write(&path, b"hello world").expect("write");

        let result = read_file(&path, 1024).expect("read");
        assert_eq!(result.content, b"hello world");
        assert_eq!(result.size, 11);
    }

    #[test]
    fn read_file_returns_too_large_when_metadata_exceeds_limit() {
        let root = workspace_root();
        let path = root.path().join("big.bin");
        fs::write(&path, vec![0u8; 100]).expect("write");

        let error = read_file(&path, 50).expect_err("over limit");
        assert!(matches!(error, StackError::WorkspaceTooLarge { limit: 50 }));
    }

    #[test]
    fn read_file_returns_not_found_for_missing_path() {
        let root = workspace_root();
        let error = read_file(&root.path().join("absent"), 1024).expect_err("missing");
        assert!(matches!(error, StackError::WorkspaceNotFound { .. }));
    }

    #[test]
    fn write_file_atomic_creates_and_overwrites_without_leaving_tempfiles() {
        let root = workspace_root();
        let target = root.path().join("note.md");

        let first = write_file_atomic(&target, b"hello").expect("write 1");
        assert_eq!(first.size, 5);
        assert_eq!(fs::read(&target).expect("read"), b"hello");

        let second = write_file_atomic(&target, b"updated content").expect("write 2");
        assert_eq!(second.size, 15);
        assert_eq!(fs::read(&target).expect("read"), b"updated content");

        let leftover: Vec<_> = fs::read_dir(root.path())
            .expect("read_dir")
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name() != "note.md")
            .collect();
        assert!(leftover.is_empty(), "leftover entries: {leftover:?}");
    }

    #[test]
    fn write_file_atomic_reports_missing_parent() {
        let root = workspace_root();
        let target = root.path().join("missing").join("note.md");
        let error = write_file_atomic(&target, b"hello").expect_err("missing parent");
        assert!(matches!(
            error,
            StackError::WorkspaceParentNotFound { requested } if requested == "note.md"
        ));
    }

    #[test]
    fn resolve_workspace_path_reports_missing_root_as_not_found() {
        let root = tempfile::tempdir().expect("tempdir");
        let missing_root = root.path().join("missing-root");
        let error = resolve_workspace_path(&missing_root, "notes/x.txt", PathIntent::WriteOrCreate)
            .expect_err("missing root");
        assert!(matches!(
            error,
            StackError::WorkspaceNotFound { requested } if requested == "notes/x.txt"
        ));
    }

    #[test]
    fn list_directory_returns_path_invalid_for_regular_file() {
        let root = workspace_root();
        let file = root.path().join("plain.txt");
        fs::write(&file, b"data").expect("write");

        let error = list_directory(&file).expect_err("should refuse listing a file");
        assert!(matches!(
            error,
            StackError::WorkspacePathInvalid { reason, .. }
                if reason.contains("not a directory")
        ));
    }

    #[test]
    fn delete_file_removes_regular_file() {
        let root = workspace_root();
        let path = root.path().join("scratch.txt");
        fs::write(&path, b"bye").expect("write");

        delete_file(&path).expect("delete");
        assert!(!path.exists(), "file should be gone");
    }

    #[test]
    fn delete_file_refuses_directory() {
        let root = workspace_root();
        let dir = root.path().join("subdir");
        fs::create_dir(&dir).expect("mkdir");

        let error = delete_file(&dir).expect_err("should refuse directory");
        assert!(matches!(
            error,
            StackError::WorkspacePathInvalid { reason, .. } if reason.contains("directory")
        ));
    }

    #[test]
    fn delete_file_returns_not_found_for_missing_path() {
        let root = workspace_root();
        let error = delete_file(&root.path().join("absent")).expect_err("missing");
        assert!(matches!(error, StackError::WorkspaceNotFound { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn delete_file_refuses_symlink_at_target() {
        use std::os::unix::fs::symlink;
        let root = workspace_root();
        fs::write(root.path().join("real"), b"x").expect("write");
        symlink(root.path().join("real"), root.path().join("link")).expect("symlink");

        let error = delete_file(&root.path().join("link")).expect_err("should refuse symlink");
        assert!(matches!(error, StackError::WorkspaceSymlinkEscape { .. }));
    }
}
