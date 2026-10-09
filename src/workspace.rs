//! Workspace file operations for the HTTP and ACP `fs/*` surfaces. A request names a path that
//! passes lexical validation (no NUL, `..`, or absolute prefix) and becomes a path relative to an
//! [`Anchor`]; every operation then walks from that anchor following links, and refuses a path
//! whose resolved form leaves the anchor. Callers run these synchronous primitives as
//! [`crate::workload_fs::Executor`] jobs, so a workload identity's credentials decide what a link
//! inside the root may reach.

use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::error::{Result, StackError};
use crate::runtime::sandbox::SandboxProfile;
use crate::workload_fs::{self, Anchor, LinkPolicy, WriteOptions};

// === CONSTANTS ===

const HARD_LINK_REASON: &str = "target has more than one hard link";
const NOT_REGULAR_REASON: &str = "target is not a regular file";
const SYMLINK_LOOP_REASON: &str = "symlinks form a loop";
const OWNER_MISMATCH_REASON: &str = "target is owned by another user";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathIntent {
    ReadExisting,
    WriteOrCreate,
}

/// Validate a workspace-relative `requested` path and return it relative to the workspace root.
pub fn workspace_relative_path(requested: &str, intent: PathIntent) -> Result<PathBuf> {
    let invalid = |reason: &str| StackError::WorkspacePathInvalid {
        reason: reason.to_owned(),
        requested: requested.to_owned(),
    };
    if requested.contains('\0') {
        return Err(invalid("contains NUL byte"));
    }
    let mut relative = PathBuf::new();
    for component in Path::new(requested).components() {
        match component {
            Component::ParentDir => return Err(invalid("contains `..` segment")),
            Component::Prefix(_) | Component::RootDir => {
                return Err(invalid("must be a workspace-relative path"));
            }
            Component::CurDir => {}
            Component::Normal(name) => relative.push(name),
        }
    }
    // A path naming the root itself has no entry to write, and `Path` collapses a trailing `.`
    // (`subdir/.`) to `subdir`, which would retarget the write at the directory.
    if intent == PathIntent::WriteOrCreate {
        if relative.as_os_str().is_empty() {
            return Err(invalid("must name a specific file inside the workspace"));
        }
        let trimmed = requested.trim_end_matches('/');
        if trimmed == "." || trimmed.ends_with("/.") {
            return Err(invalid("path must end with a file name, not `.`"));
        }
    }
    Ok(relative)
}

/// Relative form of an absolute ACP `requested` path below `anchor`. Both the anchor's canonical
/// path and `configured_root`, the spelling the anchor was opened from, are accepted because agents
/// echo back whichever cwd they were given. Only the remainder matters: the walk starts from the
/// anchor's descriptor, so neither spelling can redirect it.
pub fn anchored_relative_path(
    anchor: &Anchor,
    configured_root: &Path,
    requested: &Path,
    intent: PathIntent,
) -> Result<PathBuf> {
    let invalid = |reason: &str| StackError::WorkspacePathInvalid {
        reason: reason.to_owned(),
        requested: requested.to_string_lossy().into_owned(),
    };
    if !requested.is_absolute() {
        return Err(invalid("must be an absolute path"));
    }
    let relative = requested
        .strip_prefix(anchor.path())
        .or_else(|_| requested.strip_prefix(configured_root))
        .map_err(|_| invalid("outside the session workspace"))?;
    let relative = relative
        .to_str()
        .ok_or_else(|| invalid("not valid UTF-8"))?;
    workspace_relative_path(relative, intent)
}

/// Open the anchor a request on `root` walks from. A missing root reads as a missing path whatever
/// the operation.
pub fn open_root(root: &Path, requested: &str) -> Result<Anchor> {
    Anchor::open_with(root, LinkPolicy::Follow { contained: true })
        .map_err(|error| workspace_error(error, requested, PathIntent::ReadExisting))
}

/// Walker options for a write made on behalf of the workload: umask modes and an owner check
/// against the identity when one is declared, otherwise the runtime's owner-only modes.
pub fn workload_write_options(profile: &SandboxProfile) -> WriteOptions {
    match &profile.identity {
        Some(identity) => WriteOptions {
            create_parents: false,
            file_mode: workload_fs::UMASK_FILE_MODE,
            dir_mode: workload_fs::UMASK_DIR_MODE,
            require_existing_owner: Some(identity.uid),
        },
        None => WriteOptions {
            create_parents: false,
            file_mode: workload_fs::OWNER_ONLY_FILE_MODE,
            dir_mode: workload_fs::OWNER_ONLY_DIR_MODE,
            require_existing_owner: None,
        },
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

#[derive(Debug)]
pub struct FileOpen {
    pub file: std::fs::File,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileMetadata {
    pub size: u64,
    pub modified: DateTime<Utc>,
}

/// List the directory at `relative`, sorted directories-first then by name. Symlinks are reported
/// as `EntryKind::Symlink` and never traversed.
pub fn list_directory(
    anchor: &Anchor,
    relative: &Path,
    requested: &str,
) -> Result<WorkspaceListing> {
    let mut entries: Vec<WorkspaceEntry> = workload_fs::list_dir(anchor, relative)
        .map_err(|error| workspace_error(error, requested, PathIntent::ReadExisting))?
        .into_iter()
        .map(|(name, info)| {
            let kind = entry_kind(info.kind);
            WorkspaceEntry {
                name: name.to_string_lossy().into_owned(),
                size: (kind == EntryKind::File).then_some(info.size),
                kind,
                modified: system_time_to_utc(info.modified),
            }
        })
        .collect();
    entries.sort_by_key(sort_key);
    Ok(WorkspaceListing { entries })
}

/// Read the regular file at `relative`, at most `max_bytes` long.
pub fn read_file(
    anchor: &Anchor,
    relative: &Path,
    requested: &str,
    max_bytes: u64,
) -> Result<FileRead> {
    // The opened file's own status, so a followed symlink reports its target's mtime.
    let (content, info) = workload_fs::read_file_with_info(anchor, relative, max_bytes)
        .map_err(|error| workspace_error(error, requested, PathIntent::ReadExisting))?;
    Ok(FileRead {
        size: content.len() as u64,
        content,
        modified: system_time_to_utc(info.modified),
    })
}

/// Open the regular file at `relative` for streaming, with no size bound. The size is the opened
/// file's own, so it describes exactly what the handle reads.
pub fn open_file(anchor: &Anchor, relative: &Path, requested: &str) -> Result<FileOpen> {
    let (file, info) = workload_fs::open_file(anchor, relative)
        .map_err(|error| workspace_error(error, requested, PathIntent::ReadExisting))?;
    Ok(FileOpen {
        file,
        size: info.size,
    })
}

/// Atomically replace or create the file at `relative`, or the file a symlink there leads to,
/// returning its post-write size and mtime.
pub fn write_file(
    anchor: &Anchor,
    relative: &Path,
    requested: &str,
    content: &[u8],
    options: &WriteOptions,
) -> Result<FileMetadata> {
    workload_fs::write_file_atomic(anchor, relative, content, options)
        .map_err(|error| workspace_error(error, requested, PathIntent::WriteOrCreate))?;
    file_metadata(anchor, relative, requested)
}

/// Remove the regular file or symlink at `relative`; a symlink is removed as a link and its target
/// is kept. Directories are refused.
pub fn delete_file(anchor: &Anchor, relative: &Path, requested: &str) -> Result<()> {
    workload_fs::remove_file(anchor, relative)
        .map_err(|error| workspace_error(error, requested, PathIntent::ReadExisting))
}

fn file_metadata(anchor: &Anchor, relative: &Path, requested: &str) -> Result<FileMetadata> {
    let info = workload_fs::stat_followed(anchor, relative)
        .map_err(|error| workspace_error(error, requested, PathIntent::ReadExisting))?
        .ok_or_else(|| StackError::WorkspaceNotFound {
            requested: requested.to_owned(),
        })?;
    Ok(FileMetadata {
        size: info.size,
        modified: system_time_to_utc(info.modified),
    })
}

/// Translate walker errors into the `workspace.*` domain the HTTP and ACP callers report, keyed by
/// the caller's own `requested` path rather than a host path.
fn workspace_error(error: StackError, requested: &str, intent: PathIntent) -> StackError {
    let requested = requested.to_owned();
    let invalid = |reason: &str, requested: String| StackError::WorkspacePathInvalid {
        reason: reason.to_owned(),
        requested,
    };
    match error {
        StackError::WorkloadFsInvalidPath { reason, .. } => invalid(reason, requested),
        StackError::WorkloadFsSymlinkRefused { .. } => {
            StackError::WorkspaceSymlinkEscape { requested }
        }
        StackError::WorkloadFsHardLinkRefused { .. } => invalid(HARD_LINK_REASON, requested),
        StackError::WorkloadFsSymlinkLoop { .. } => invalid(SYMLINK_LOOP_REASON, requested),
        StackError::WorkloadFsNotRegular { .. } => invalid(NOT_REGULAR_REASON, requested),
        StackError::WorkloadFsOwnerMismatch { .. } => invalid(OWNER_MISMATCH_REASON, requested),
        // A write's own target may be missing, so a missing component is a missing parent.
        StackError::WorkloadFsNotFound { .. } => match intent {
            PathIntent::ReadExisting => StackError::WorkspaceNotFound { requested },
            PathIntent::WriteOrCreate => StackError::WorkspaceParentNotFound { requested },
        },
        StackError::WorkloadFsIo { source, .. }
            if source.kind() == std::io::ErrorKind::PermissionDenied =>
        {
            StackError::WorkspacePermissionDenied { requested, source }
        }
        StackError::WorkloadFsIo { source, .. } => StackError::WorkspaceIo { requested, source },
        other => other,
    }
}

fn entry_kind(kind: workload_fs::EntryKind) -> EntryKind {
    match kind {
        workload_fs::EntryKind::File => EntryKind::File,
        workload_fs::EntryKind::Dir => EntryKind::Directory,
        workload_fs::EntryKind::Symlink => EntryKind::Symlink,
        workload_fs::EntryKind::Other => EntryKind::Other,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    const MAX_READ: u64 = 1024;

    fn workspace() -> (tempfile::TempDir, Anchor) {
        let root = tempfile::tempdir().expect("tempdir");
        let anchor = open_root(root.path(), ".").expect("anchor");
        (root, anchor)
    }

    fn owner_only() -> WriteOptions {
        workload_write_options(&SandboxProfile::default())
    }

    fn assert_invalid(error: StackError, fragment: &str) {
        assert!(
            matches!(
                &error,
                StackError::WorkspacePathInvalid { reason, .. } if reason.contains(fragment)
            ),
            "expected a path_invalid containing `{fragment}`, got {error:?}"
        );
    }

    #[test]
    fn lexical_validation_refuses_traversal_nul_and_absolute_paths() {
        for intent in [PathIntent::ReadExisting, PathIntent::WriteOrCreate] {
            assert_invalid(
                workspace_relative_path("../etc/passwd", intent).expect_err("traversal"),
                "..",
            );
            assert_invalid(
                workspace_relative_path("a\0b", intent).expect_err("NUL"),
                "NUL",
            );
            assert_invalid(
                workspace_relative_path("/etc/passwd", intent).expect_err("absolute"),
                "workspace-relative",
            );
        }
    }

    #[test]
    fn lexical_validation_refuses_writes_that_name_no_file() {
        assert_invalid(
            workspace_relative_path(".", PathIntent::WriteOrCreate).expect_err("root"),
            "specific file",
        );
        assert_invalid(
            workspace_relative_path("subdir/.", PathIntent::WriteOrCreate).expect_err("dot"),
            "file name",
        );
        assert_eq!(
            workspace_relative_path(".", PathIntent::ReadExisting).expect("root listing"),
            PathBuf::new()
        );
        assert_eq!(
            workspace_relative_path("./a//b/", PathIntent::WriteOrCreate).expect("normalized"),
            PathBuf::from("a/b")
        );
    }

    #[test]
    fn anchored_paths_accept_canonical_and_configured_spellings_only() {
        let (root, anchor) = workspace();
        let canonical = anchor.path().join("notes/a.txt");
        let configured = root.path().join("notes/a.txt");
        for requested in [&canonical, &configured] {
            assert_eq!(
                anchored_relative_path(&anchor, root.path(), requested, PathIntent::WriteOrCreate)
                    .expect("inside"),
                PathBuf::from("notes/a.txt")
            );
        }
        assert_invalid(
            anchored_relative_path(
                &anchor,
                root.path(),
                Path::new("/etc/passwd"),
                PathIntent::ReadExisting,
            )
            .expect_err("outside"),
            "outside the session workspace",
        );
        assert_invalid(
            anchored_relative_path(
                &anchor,
                root.path(),
                Path::new("relative.txt"),
                PathIntent::ReadExisting,
            )
            .expect_err("relative"),
            "absolute",
        );
        assert_invalid(
            anchored_relative_path(
                &anchor,
                root.path(),
                &anchor.path().join("../escape"),
                PathIntent::ReadExisting,
            )
            .expect_err("traversal"),
            "..",
        );
    }

    #[test]
    fn open_root_reports_a_missing_root_as_not_found() {
        let parent = tempfile::tempdir().expect("tempdir");
        let error = open_root(&parent.path().join("missing-root"), "notes/x.txt")
            .expect_err("missing root");
        assert!(matches!(
            error,
            StackError::WorkspaceNotFound { requested } if requested == "notes/x.txt"
        ));
    }

    #[test]
    fn list_directory_sorts_directories_before_files_and_reports_symlinks() {
        let (root, anchor) = workspace();
        fs::write(root.path().join("zzz.txt"), b"").expect("write");
        fs::write(root.path().join("aaa.txt"), b"hello").expect("write");
        fs::create_dir(root.path().join("zdir")).expect("mkdir z");
        fs::create_dir(root.path().join("adir")).expect("mkdir a");
        symlink(root.path().join("aaa.txt"), root.path().join("alias")).expect("symlink");

        let listing = list_directory(&anchor, Path::new(""), ".").expect("list");
        let summary: Vec<(&str, &EntryKind, Option<u64>)> = listing
            .entries
            .iter()
            .map(|entry| (entry.name.as_str(), &entry.kind, entry.size))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("adir", &EntryKind::Directory, None),
                ("zdir", &EntryKind::Directory, None),
                ("aaa.txt", &EntryKind::File, Some(5)),
                ("zzz.txt", &EntryKind::File, Some(0)),
                ("alias", &EntryKind::Symlink, None),
            ]
        );
    }

    #[test]
    fn list_directory_refuses_a_file_and_a_symlinked_directory() {
        let (root, anchor) = workspace();
        let outside = tempfile::tempdir().expect("outside");
        fs::write(root.path().join("plain.txt"), b"data").expect("write");
        symlink(outside.path(), root.path().join("linked")).expect("symlink");

        assert_invalid(
            list_directory(&anchor, Path::new("plain.txt"), "plain.txt").expect_err("file"),
            "not a directory",
        );
        assert!(matches!(
            list_directory(&anchor, Path::new("linked"), "linked"),
            Err(StackError::WorkspaceSymlinkEscape { .. })
        ));
    }

    #[test]
    fn read_file_returns_content_size_and_enforces_the_limit() {
        let (root, anchor) = workspace();
        fs::write(root.path().join("greeting.txt"), b"hello world").expect("write");

        let read =
            read_file(&anchor, Path::new("greeting.txt"), "greeting.txt", MAX_READ).expect("read");
        assert_eq!(read.content, b"hello world");
        assert_eq!(read.size, 11);
        assert!(matches!(
            read_file(&anchor, Path::new("greeting.txt"), "greeting.txt", 5),
            Err(StackError::WorkspaceTooLarge { limit: 5 })
        ));
        assert!(matches!(
            read_file(&anchor, Path::new("absent"), "absent", MAX_READ),
            Err(StackError::WorkspaceNotFound { .. })
        ));
    }

    #[test]
    fn read_file_refuses_links_that_resolve_outside_the_root() {
        let (root, anchor) = workspace();
        let outside = tempfile::tempdir().expect("outside");
        fs::write(outside.path().join("secret"), b"leak").expect("secret");
        symlink(outside.path().join("secret"), root.path().join("escape")).expect("escape");
        symlink(outside.path(), root.path().join("dir")).expect("dir");

        for relative in ["escape", "dir/secret"] {
            assert!(
                matches!(
                    read_file(&anchor, Path::new(relative), relative, MAX_READ),
                    Err(StackError::WorkspaceSymlinkEscape { .. })
                ),
                "{relative} must be refused"
            );
        }
    }

    #[test]
    fn links_inside_the_root_are_followed() {
        let (root, anchor) = workspace();
        let outside = tempfile::tempdir().expect("outside");
        fs::write(outside.path().join("secret"), b"leak").expect("secret");
        fs::create_dir(root.path().join("real")).expect("real dir");
        fs::write(root.path().join("AGENTS.md"), b"rules").expect("agents");
        symlink(root.path().join("AGENTS.md"), root.path().join("CLAUDE.md")).expect("inner");
        symlink(root.path().join("real"), root.path().join("linked")).expect("dir link");
        fs::hard_link(root.path().join("AGENTS.md"), root.path().join("pnpm")).expect("hard");
        symlink(outside.path().join("secret"), root.path().join("escape")).expect("escape");
        let target_modified = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        fs::File::options()
            .write(true)
            .open(root.path().join("AGENTS.md"))
            .and_then(|file| file.set_modified(target_modified))
            .expect("set target mtime");

        for relative in ["CLAUDE.md", "pnpm"] {
            let read = read_file(&anchor, Path::new(relative), relative, MAX_READ).expect(relative);
            assert_eq!(read.content, b"rules");
            assert_eq!(read.modified, system_time_to_utc(target_modified));
        }
        write_file(
            &anchor,
            Path::new("linked/new.txt"),
            "linked/new.txt",
            b"x",
            &owner_only(),
        )
        .expect("write through a linked dir");
        assert!(root.path().join("real/new.txt").exists());
        assert!(matches!(
            read_file(&anchor, Path::new("escape"), "escape", MAX_READ),
            Err(StackError::WorkspaceSymlinkEscape { .. })
        ));
    }

    #[test]
    fn write_file_writes_through_a_link_and_keeps_it() {
        let (root, anchor) = workspace();
        fs::write(root.path().join("real.txt"), b"old").expect("real");
        symlink("real.txt", root.path().join("relative")).expect("relative link");
        symlink(root.path().join("relative"), root.path().join("chained")).expect("chained link");
        symlink("made.txt", root.path().join("dangling")).expect("dangling link");

        let written = write_file(
            &anchor,
            Path::new("chained"),
            "chained",
            b"through",
            &owner_only(),
        )
        .expect("write through a chain");
        assert_eq!(written.size, 7);
        assert_eq!(
            fs::read(root.path().join("real.txt")).expect("real"),
            b"through"
        );
        for link in ["relative", "chained"] {
            assert!(
                fs::symlink_metadata(root.path().join(link))
                    .expect("link kept")
                    .file_type()
                    .is_symlink()
            );
        }

        write_file(
            &anchor,
            Path::new("dangling"),
            "dangling",
            b"made",
            &owner_only(),
        )
        .expect("write through a dangling link");
        assert_eq!(
            fs::read(root.path().join("made.txt")).expect("made"),
            b"made"
        );
    }

    #[test]
    fn symlink_loops_are_reported_as_invalid_paths() {
        let (root, anchor) = workspace();
        symlink("second", root.path().join("first")).expect("first");
        symlink("first", root.path().join("second")).expect("second");

        assert_invalid(
            read_file(&anchor, Path::new("first"), "first", MAX_READ).expect_err("read loop"),
            SYMLINK_LOOP_REASON,
        );
        assert_invalid(
            write_file(&anchor, Path::new("first"), "first", b"x", &owner_only())
                .expect_err("write loop"),
            SYMLINK_LOOP_REASON,
        );
    }

    #[test]
    fn write_file_creates_and_overwrites_owner_only_without_tempfiles() {
        let (root, anchor) = workspace();
        let first = write_file(
            &anchor,
            Path::new("note.md"),
            "note.md",
            b"hello",
            &owner_only(),
        )
        .expect("write 1");
        assert_eq!(first.size, 5);
        let second = write_file(
            &anchor,
            Path::new("note.md"),
            "note.md",
            b"updated content",
            &owner_only(),
        )
        .expect("write 2");
        assert_eq!(second.size, 15);
        assert_eq!(
            fs::read(root.path().join("note.md")).expect("read"),
            b"updated content"
        );
        let mode = fs::metadata(root.path().join("note.md"))
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, workload_fs::OWNER_ONLY_FILE_MODE);
        let names: Vec<_> = fs::read_dir(root.path())
            .expect("read_dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("note.md")]);
    }

    #[test]
    fn write_file_refuses_directories_symlinks_and_missing_parents() {
        let (root, anchor) = workspace();
        let outside = tempfile::tempdir().expect("outside");
        let secret = outside.path().join("secret");
        fs::write(&secret, b"outside").expect("secret");
        fs::create_dir(root.path().join("subdir")).expect("mkdir");
        fs::write(root.path().join("plain.txt"), b"data").expect("plain");
        symlink(&secret, root.path().join("link")).expect("link");
        symlink(outside.path(), root.path().join("dir")).expect("dir link");

        assert_invalid(
            write_file(&anchor, Path::new("subdir"), "subdir", b"x", &owner_only())
                .expect_err("directory"),
            "regular file",
        );
        assert_invalid(
            write_file(
                &anchor,
                Path::new("plain.txt/child"),
                "plain.txt/child",
                b"x",
                &owner_only(),
            )
            .expect_err("file as parent"),
            "not a directory",
        );
        for relative in ["link", "dir/secret", "dir/new"] {
            assert!(
                matches!(
                    write_file(&anchor, Path::new(relative), relative, b"x", &owner_only()),
                    Err(StackError::WorkspaceSymlinkEscape { .. })
                ),
                "{relative} must be refused"
            );
        }
        assert!(matches!(
            write_file(
                &anchor,
                Path::new("missing/note.md"),
                "missing/note.md",
                b"x",
                &owner_only()
            ),
            Err(StackError::WorkspaceParentNotFound { requested }) if requested == "missing/note.md"
        ));
        assert_eq!(fs::read(&secret).expect("secret"), b"outside");
        assert!(!outside.path().join("new").exists());
    }

    #[test]
    fn delete_file_removes_files_and_links_and_refuses_directories_escapes_and_missing() {
        let (root, anchor) = workspace();
        let outside = tempfile::tempdir().expect("outside");
        let secret = outside.path().join("secret");
        fs::write(&secret, b"outside").expect("secret");
        fs::write(root.path().join("scratch.txt"), b"bye").expect("scratch");
        fs::create_dir(root.path().join("subdir")).expect("mkdir");
        symlink(&secret, root.path().join("link")).expect("link");
        symlink(outside.path(), root.path().join("dir")).expect("dir link");

        delete_file(&anchor, Path::new("scratch.txt"), "scratch.txt").expect("delete");
        assert!(!root.path().join("scratch.txt").exists());
        delete_file(&anchor, Path::new("link"), "link").expect("delete the link");
        assert!(fs::symlink_metadata(root.path().join("link")).is_err());
        assert_invalid(
            delete_file(&anchor, Path::new("subdir"), "subdir").expect_err("directory"),
            "regular file",
        );
        assert!(matches!(
            delete_file(&anchor, Path::new("dir/secret"), "dir/secret"),
            Err(StackError::WorkspaceSymlinkEscape { .. })
        ));
        assert!(matches!(
            delete_file(&anchor, Path::new("absent"), "absent"),
            Err(StackError::WorkspaceNotFound { .. })
        ));
        assert_eq!(fs::read(&secret).expect("secret"), b"outside");
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires CAP_SETUID/CAP_SETGID and ACPS_TEST_WORKLOAD_USER"]
    fn workload_write_lands_owned_by_the_identity() {
        use std::os::unix::fs::MetadataExt as _;

        let user = std::env::var("ACPS_TEST_WORKLOAD_USER").expect("ACPS_TEST_WORKLOAD_USER");
        let entry = crate::ownership::lookup_user(&user)
            .expect("lookup")
            .expect("workload user exists");
        let profile = SandboxProfile {
            config: Default::default(),
            identity: Some(crate::runtime::sandbox::WorkloadIdentity {
                name: user,
                uid: entry.uid,
                gid: entry.gid,
                home: entry.home,
            }),
        };
        let root = tempfile::tempdir().expect("tempdir");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o777))
            .expect("open the root to the workload user");
        let options = workload_write_options(&profile);
        let root_path = root.path().to_path_buf();

        let written = profile
            .executor()
            .run(workload_fs::DEFAULT_JOB_TIMEOUT, move || {
                let anchor = open_root(&root_path, "note.md")?;
                write_file(&anchor, Path::new("note.md"), "note.md", b"hello", &options)
            })
            .expect("workload write");

        assert_eq!(written.size, 5);
        let metadata = fs::symlink_metadata(root.path().join("note.md")).expect("metadata");
        assert_eq!((metadata.uid(), metadata.gid()), (entry.uid, entry.gid));
    }

    #[test]
    fn open_file_reports_the_opened_size_with_no_limit() {
        let (root, anchor) = workspace();
        let content = vec![7u8; MAX_READ as usize * 4];
        fs::write(root.path().join("large.bin"), &content).expect("write");

        let opened = open_file(&anchor, Path::new("large.bin"), "large.bin").expect("open");
        assert_eq!(opened.size, content.len() as u64);
        assert!(matches!(
            open_file(&anchor, Path::new("absent"), "absent"),
            Err(StackError::WorkspaceNotFound { .. })
        ));
    }

    #[test]
    fn open_file_follows_links_inside_the_root_only() {
        let (root, anchor) = workspace();
        let outside = tempfile::tempdir().expect("outside");
        fs::write(outside.path().join("secret"), b"leak").expect("secret");
        fs::write(root.path().join("real.txt"), b"ok").expect("real");
        symlink(outside.path().join("secret"), root.path().join("escape")).expect("escape");
        symlink(root.path().join("real.txt"), root.path().join("inner")).expect("inner");
        symlink(outside.path(), root.path().join("dir")).expect("dir");
        fs::hard_link(root.path().join("real.txt"), root.path().join("linked")).expect("hard");

        for relative in ["inner", "linked"] {
            let opened = open_file(&anchor, Path::new(relative), relative).expect(relative);
            assert_eq!(opened.size, 2);
        }
        for relative in ["escape", "dir/secret"] {
            assert!(
                matches!(
                    open_file(&anchor, Path::new(relative), relative),
                    Err(StackError::WorkspaceSymlinkEscape { .. })
                ),
                "{relative} must be refused"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires CAP_SETUID/CAP_SETGID and ACPS_TEST_WORKLOAD_USER"]
    fn workload_open_refuses_a_file_the_identity_cannot_read() {
        let user = std::env::var("ACPS_TEST_WORKLOAD_USER").expect("ACPS_TEST_WORKLOAD_USER");
        let entry = crate::ownership::lookup_user(&user)
            .expect("lookup")
            .expect("workload user exists");
        let profile = SandboxProfile {
            config: Default::default(),
            identity: Some(crate::runtime::sandbox::WorkloadIdentity {
                name: user,
                uid: entry.uid,
                gid: entry.gid,
                home: entry.home,
            }),
        };
        let root = tempfile::tempdir().expect("tempdir");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o777))
            .expect("open the root to the workload user");
        let private = root.path().join("private.bin");
        fs::write(&private, b"runtime only").expect("write");
        fs::set_permissions(&private, fs::Permissions::from_mode(0o600)).expect("owner only");
        let root_path = root.path().to_path_buf();

        let outcome = profile
            .executor()
            .run(workload_fs::DEFAULT_JOB_TIMEOUT, move || {
                let anchor = open_root(&root_path, "private.bin")?;
                open_file(&anchor, Path::new("private.bin"), "private.bin")
            });

        assert!(
            matches!(&outcome, Err(StackError::WorkspacePermissionDenied { .. })),
            "expected a permission refusal, got {outcome:?}"
        );
    }
}
