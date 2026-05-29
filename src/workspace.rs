//! Workspace file operations under `workspace.root`.
//!
//! Three responsibilities:
//!   * Resolve incoming request paths to absolute filesystem paths inside the
//!     workspace root, rejecting traversal, NUL bytes, absolute prefixes, and
//!     symlink escapes. See `resolve_workspace_path`.
//!   * Provide synchronous list/read/write/delete primitives that the HTTP
//!     handlers run inside `tokio::task::spawn_blocking`.
//!   * Stay agnostic to HTTP framing — the API layer translates `StackError`
//!     into envelope responses.
//!
//! Symlink policy: for reads we canonicalize the full path and check that it
//! stays inside the canonicalized root, which transparently rejects escapes
//! while permitting symlinks that happen to point back inside the workspace.
//! For writes we additionally refuse to overwrite an existing symlink at the
//! target, because writing through a symlink (even one that points inside the
//! workspace) is rarely what an API client intends and makes intent hard to
//! audit from the request line alone. `read_file` also opens the final path
//! with `O_NOFOLLOW` on Unix so a symlink swap between resolve and open is
//! refused at the kernel level rather than silently followed.
//!
//! Residual TOCTOU: a local actor with write access to `workspace.root` can
//! swap entries between `resolve_workspace_path` and the subsequent
//! list/write/delete syscall, defeating intermediate-directory containment.
//! The runtime accepts this in single-tenant deployments where the agent and
//! daemon run as the same user inside a VM. Strict mitigation would require
//! `openat` with `O_NOFOLLOW` at every path segment; that is intentionally
//! out of scope for 0.0.1.

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

/// Resolve a workspace-relative `requested` path to an absolute filesystem
/// path inside `root`. `root` must already exist (it is canonicalized on every
/// call so that callers do not need to cache the canonical form).
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
    // For writes/deletes the request must point at a specific file. Two
    // edge cases:
    //   * paths that normalize to "the root itself" (`""`, `.`, `./`, …) —
    //     Rust's `Path::file_name()` returns the workspace root's basename
    //     for these, so they'd resolve to a sibling of the root.
    //   * paths whose final string segment is `.` (e.g. `subdir/.`) — Rust
    //     collapses the trailing `.` away, so the resolver would silently
    //     act on `subdir` even though the response echoes `subdir/.`. The
    //     write would target a different path than the caller named.
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
            // `canonicalize` happily resolves through a regular file, so the
            // parent could canonicalize successfully and still not be a
            // directory. Re-stat explicitly to surface this as a 400 instead
            // of letting atomic_write_owner_only fail with a 500 ENOTDIR.
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
            // Refuse to overwrite an existing symlink. `symlink_metadata` does
            // not follow the link, so we see the link itself rather than its
            // target.
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

/// List the entries of `absolute_path` (which must be inside the workspace
/// root; the caller resolves first). Entries are sorted directories-first,
/// then by name ascending. Symlinks are reported as `EntryKind::Symlink` and
/// are not traversed: their `size` field reflects the link target's metadata
/// only when the metadata call succeeds; otherwise it is `None`.
pub fn list_directory(absolute_path: &Path) -> Result<WorkspaceListing> {
    // Targeting a regular file with the list endpoint is a client error, not
    // an internal fault: surface a workspace.path_invalid 400 rather than
    // leaking the platform's NotADirectory I/O text through workspace.io_failed.
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

/// Read at most `max_bytes` of an existing regular file. Returns
/// `WorkspaceTooLarge` if the file's reported size exceeds `max_bytes` or the
/// stream produces more bytes than that (which should not happen given the
/// initial metadata check, but we re-check to defend against a concurrent
/// writer that grows the file after we opened it).
pub fn read_file(absolute_path: &Path, max_bytes: u64) -> Result<FileRead> {
    // Stat first, open second. Opening a FIFO/socket for read can block
    // indefinitely on Unix, which would tie up a tokio blocking thread for
    // every request to such a path. Stat-then-open keeps the open call
    // confined to entries we've already verified are regular files.
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
        // ELOOP from O_NOFOLLOW means a symlink appeared at the final
        // component between our metadata check and the open. Surface that as
        // the same 400 symlink-escape we'd return at resolve time rather
        // than an opaque 500.
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

/// Atomically write `content` to `absolute_path` using a temp file alongside
/// the target. Returns the post-write file size and mtime. Refuses to write
/// when the target is an existing directory — that's a 400-shaped client
/// error rather than the 500 that atomic-write would produce on the rename.
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

/// Remove a regular file at `absolute_path`. Refuses directories (the API
/// surface does not expose recursive directory removal in 0.0.1) and refuses
/// symlinks (whose delete semantics are ambiguous over the wire).
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
    // Refuse non-regular entries explicitly: the API contract says this
    // endpoint deletes "a regular file", and silently removing FIFOs, sockets,
    // or device nodes via remove_file would be a surprise.
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

/// Canonicalize a path and translate `std::io` errors into the workspace
/// domain so client-shaped errors (missing path, intermediate non-directory)
/// surface as 4xx instead of falling through to generic 500 `WorkspaceIo`.
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

/// Open a file with `O_NOFOLLOW | O_NONBLOCK` so that two final-component
/// type swaps that could happen between our resolve-time metadata check and
/// this open are caught at open time rather than after the syscall has
/// blocked indefinitely or followed a link:
///
///   * a symlink swapped in → kernel returns ELOOP, surfaced as a 400.
///   * a FIFO/socket swapped in → `O_NONBLOCK` makes the open return
///     immediately (with ENXIO for a writer-less FIFO, or success); the
///     post-open `fstat` then rejects the non-regular file before any read.
///
/// On non-Unix hosts this falls back to the platform-default open.
fn open_no_follow(absolute_path: &Path) -> std::io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(absolute_path)?;
        // Re-stat through the open handle so the type we read from is the
        // type we already accepted in `read_file`'s metadata check. Any
        // race-substituted non-regular file is rejected here.
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

/// `atomic_write_owner_only` returns `FileCreate`/`PermissionSet`/... errors;
/// translate them into workspace-domain errors so handlers can respond with
/// `workspace.*` codes consistently.
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
        // `subdir/.` is normalized to `subdir` by Rust's Path API, so the
        // resolver accepts the request, but the write primitive must catch
        // the directory-as-target case and surface a 400-shaped path-invalid
        // error rather than the 500 that the atomic rename would produce.
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
