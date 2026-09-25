//! Shared filesystem helpers for landing files under the runtime user's home with owner-only
//! permissions (0700 / 0600 on Unix, a no-op elsewhere).

use crate::error::{Result, StackError};
use std::fs::File;
use std::fs::Permissions;
use std::io::Write as _;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const AGENT_CONFIG_MUTATION_LOCK_FILE_NAME: &str = ".agent-config.lock";

/// Cross-process advisory `flock`, released when dropped. The lock file keeps a stable inode, so
/// atomic replacements of the files it guards do not invalidate another `acps` process's lock.
pub struct ExclusiveFileLock {
    _file: File,
}

/// Process-wide lock for read/modify/write of the Agent config.
pub fn acquire_agent_config_mutation_file_lock(config_path: &Path) -> Result<ExclusiveFileLock> {
    let parent = parent_dir(config_path)?;
    create_dir_owner_only(parent)?;
    acquire_exclusive_lock_file(&parent.join(AGENT_CONFIG_MUTATION_LOCK_FILE_NAME))
}

/// Block until the exclusive lock on `lock_path` is held; the parent directory must exist.
pub fn acquire_exclusive_lock_file(lock_path: &Path) -> Result<ExclusiveFileLock> {
    let file = open_lock_file(lock_path)?;
    match flock_exclusive(&file, false) {
        Ok(()) => Ok(ExclusiveFileLock { _file: file }),
        Err(source) => Err(StackError::FileCreate {
            path: lock_path.to_path_buf(),
            source,
        }),
    }
}

/// Take the exclusive lock on `lock_path` without waiting; `None` means another holder has it.
pub fn try_acquire_exclusive_lock_file(lock_path: &Path) -> Result<Option<ExclusiveFileLock>> {
    let file = open_lock_file(lock_path)?;
    match flock_exclusive(&file, true) {
        Ok(()) => Ok(Some(ExclusiveFileLock { _file: file })),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        Err(source) => Err(StackError::FileCreate {
            path: lock_path.to_path_buf(),
            source,
        }),
    }
}

fn open_lock_file(lock_path: &Path) -> Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    let file = options
        .open(lock_path)
        .map_err(|source| StackError::FileCreate {
            path: lock_path.to_path_buf(),
            source,
        })?;
    let metadata = file.metadata().map_err(|source| StackError::FileCreate {
        path: lock_path.to_path_buf(),
        source,
    })?;
    if !metadata.is_file()
        || !metadata_owned_by_current_user(&metadata)
        || metadata_has_multiple_links(&metadata)
        || !metadata_is_owner_only_file(&metadata)
    {
        return Err(StackError::FileCreate {
            path: lock_path.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "lock file must be a current-user-owned, single-link regular file with mode 0600",
            ),
        });
    }
    Ok(file)
}

#[cfg(unix)]
fn flock_exclusive(file: &File, non_blocking: bool) -> std::io::Result<()> {
    use std::os::fd::AsRawFd as _;
    let operation = if non_blocking {
        libc::LOCK_EX | libc::LOCK_NB
    } else {
        libc::LOCK_EX
    };
    // SAFETY: flock only operates on the descriptor this function borrows for the call.
    if unsafe { libc::flock(file.as_raw_fd(), operation) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(unix))]
fn flock_exclusive(_file: &File, _non_blocking: bool) -> std::io::Result<()> {
    Ok(())
}

/// Point `link` at `target` by renaming a sibling temp symlink over it, so readers never observe a
/// missing link. Replaces an existing symlink; the parent directory must exist.
#[cfg(unix)]
pub fn replace_symlink_atomically(target: &Path, link: &Path) -> Result<()> {
    let parent = parent_dir(link)?;
    let file_name = link
        .file_name()
        .ok_or_else(|| StackError::MissingParentDir {
            path: link.to_path_buf(),
        })?;
    let mut temp_name = std::ffi::OsString::from(".");
    temp_name.push(file_name);
    temp_name.push(format!(".tmp-{}", std::process::id()));
    let temp_link = parent.join(temp_name);
    match std::fs::remove_file(&temp_link) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(StackError::FileCreate {
                path: temp_link,
                source,
            });
        }
    }
    std::os::unix::fs::symlink(target, &temp_link).map_err(|source| StackError::FileCreate {
        path: temp_link.clone(),
        source,
    })?;
    if let Err(source) = std::fs::rename(&temp_link, link) {
        if let Err(cleanup_error) = std::fs::remove_file(&temp_link) {
            tracing::warn!(error = %cleanup_error, path = %temp_link.display(), "failed to remove temp symlink after a failed swap");
        }
        return Err(StackError::FileCreate {
            path: link.to_path_buf(),
            source,
        });
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn replace_symlink_atomically(_target: &Path, link: &Path) -> Result<()> {
    Err(StackError::FileCreate {
        path: link.to_path_buf(),
        source: std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "runtime-managed symlinks require a Unix host",
        ),
    })
}

/// Remove every release directory under `releases` except `keep` and `previous`, the one it
/// replaced, which processes started before the swap may still be running from. Failures are
/// logged; stale releases are harmless.
pub fn prune_release_dirs(
    releases: &Path,
    keep: &std::ffi::OsStr,
    previous: Option<&std::ffi::OsStr>,
) {
    let entries = match std::fs::read_dir(releases) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!(%error, path = %releases.display(), "could not list releases for pruning");
            return;
        }
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.as_os_str() == keep || Some(name.as_os_str()) == previous {
            continue;
        }
        let path = entry.path();
        if let Err(error) = std::fs::remove_dir_all(&path) {
            tracing::warn!(%error, path = %path.display(), "could not prune a stale release");
        }
    }
}

pub fn home_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .ok_or(StackError::HomeNotSet)?;
    let home = PathBuf::from(home);
    ensure_home_isolated(&home)?;
    Ok(home)
}

/// Fixture builds run inside the developer's test suite: a HOME outside the temp dir would let a
/// test rewrite the developer's real agent configs, so refuse it unless the host is disposable.
#[cfg(feature = "test-fixtures")]
fn ensure_home_isolated(home: &Path) -> Result<()> {
    if crate::dev_gates::disposable_host_enabled() || path_is_under_temp_dir(home) {
        return Ok(());
    }
    Err(StackError::HomeNotIsolated {
        path: home.to_path_buf(),
    })
}

#[cfg(not(feature = "test-fixtures"))]
fn ensure_home_isolated(_home: &Path) -> Result<()> {
    Ok(())
}

/// Canonicalize both sides: macOS reports `$TMPDIR` under `/var` while the real path is
/// `/private/var`, and tempfile hands out whichever form the caller resolved.
#[cfg(feature = "test-fixtures")]
fn path_is_under_temp_dir(path: &Path) -> bool {
    let canonical = |candidate: &Path| candidate.canonicalize().ok();
    match (canonical(&std::env::temp_dir()), canonical(path)) {
        (Some(temp), Some(target)) => target.starts_with(&temp),
        _ => false,
    }
}

pub fn parent_dir(path: &Path) -> Result<&Path> {
    path.parent().ok_or_else(|| StackError::MissingParentDir {
        path: path.to_path_buf(),
    })
}

pub fn create_dir_owner_only(path: &Path) -> Result<()> {
    if path.exists() {
        // `symlink_metadata` so a symlink here reads as a non-directory instead of being followed;
        // a substituted symlink would route file creation outside the security-managed tree.
        let metadata =
            std::fs::symlink_metadata(path).map_err(|source| StackError::DirectoryCreate {
                path: path.to_path_buf(),
                source,
            })?;
        if !metadata.is_dir() {
            return Err(StackError::DirectoryCreate {
                path: path.to_path_buf(),
                source: std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "path exists but is not a directory; refusing to chmod a non-directory \
                     under runtime-managed paths",
                ),
            });
        }
        return set_owner_only_dir(path);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| StackError::DirectoryCreate {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(path)
            .map_err(|source| StackError::DirectoryCreate {
                path: path.to_path_buf(),
                source,
            })
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(path).map_err(|source| StackError::DirectoryCreate {
            path: path.to_path_buf(),
            source,
        })
    }
}

pub fn write_new_file_owner_only(path: &Path, content: &[u8]) -> Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let mut file = opts.open(path).map_err(|source| StackError::FileCreate {
        path: path.to_path_buf(),
        source,
    })?;
    file.write_all(content)
        .map_err(|source| StackError::FileCreate {
            path: path.to_path_buf(),
            source,
        })?;
    sync_file(path, &file)?;
    sync_parent_dir(path)
}

/// Atomically replace `path` via a sibling temp file, owner-only on both. The parent must exist.
pub fn atomic_write_owner_only(path: &Path, content: &[u8]) -> Result<()> {
    let parent = parent_dir(path)?;
    let mut temp =
        tempfile::NamedTempFile::new_in(parent).map_err(|source| StackError::FileCreate {
            path: parent.to_path_buf(),
            source,
        })?;
    let temp_path = temp.path().to_path_buf();
    set_owner_only_file(&temp_path)?;
    temp.as_file_mut()
        .write_all(content)
        .map_err(|source| StackError::FileCreate {
            path: temp_path.clone(),
            source,
        })?;
    sync_file(&temp_path, temp.as_file_mut())?;
    temp.persist(path).map_err(|error| StackError::FileCreate {
        path: path.to_path_buf(),
        source: error.error,
    })?;
    set_owner_only_file(path)?;
    let final_file = std::fs::File::open(path).map_err(|source| StackError::FileCreate {
        path: path.to_path_buf(),
        source,
    })?;
    sync_file(path, &final_file)?;
    sync_parent_dir(path)
}

/// Validate and prepare a file target below `home` without following symlinked path components;
/// an existing target must be a current-user-owned, single-link regular file.
pub fn prepare_owner_managed_file_path(home: &Path, path: &Path) -> Result<()> {
    let relative = path
        .strip_prefix(home)
        .map_err(|_| StackError::FileCreate {
            path: path.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "managed file target is outside the runtime home",
            ),
        })?;
    let parent = parent_dir(relative)?;
    let mut current = home.to_path_buf();
    for component in parent.components() {
        use std::path::Component;
        let Component::Normal(component) = component else {
            return Err(StackError::FileCreate {
                path: path.to_path_buf(),
                source: std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "managed file target contains a non-normal path component",
                ),
            });
        };
        current.push(component);
        if current.exists() {
            let metadata = std::fs::symlink_metadata(&current).map_err(|source| {
                StackError::DirectoryCreate {
                    path: current.clone(),
                    source,
                }
            })?;
            if !metadata.is_dir() || !metadata_owned_by_current_user(&metadata) {
                return Err(StackError::DirectoryCreate {
                    path: current.clone(),
                    source: std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "managed directory is not a current-user-owned real directory",
                    ),
                });
            }
            set_owner_only_dir(&current)?;
        } else {
            create_dir_owner_only(&current)?;
        }
    }

    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_file()
                || !metadata_owned_by_current_user(&metadata)
                || metadata_has_multiple_links(&metadata)
            {
                return Err(StackError::FileCreate {
                    path: path.to_path_buf(),
                    source: std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "managed file target must be a current-user-owned regular file with one link",
                    ),
                });
            }
            set_owner_only_file(path)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(StackError::FileCreate {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Validate an existing owner-only runtime file without changing its mode or following a symlink.
pub fn validate_owner_only_regular_file(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path).map_err(|source| StackError::FileCreate {
        path: path.to_path_buf(),
        source,
    })?;
    let valid = metadata.is_file()
        && metadata_owned_by_current_user(&metadata)
        && !metadata_has_multiple_links(&metadata)
        && metadata_is_owner_only_file(&metadata);
    if !valid {
        return Err(StackError::FileCreate {
            path: path.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "runtime file must be a current-user-owned, single-link regular file with mode 0600",
            ),
        });
    }
    Ok(())
}

#[cfg(unix)]
fn metadata_owned_by_current_user(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    metadata.uid() == unsafe { libc::geteuid() }
}

#[cfg(not(unix))]
fn metadata_owned_by_current_user(_metadata: &std::fs::Metadata) -> bool {
    true
}

#[cfg(unix)]
fn metadata_has_multiple_links(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    metadata.nlink() > 1
}

#[cfg(unix)]
fn metadata_is_owner_only_file(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    metadata.mode() & 0o777 == 0o600
}

#[cfg(not(unix))]
fn metadata_is_owner_only_file(_metadata: &std::fs::Metadata) -> bool {
    true
}

#[cfg(not(unix))]
fn metadata_has_multiple_links(_metadata: &std::fs::Metadata) -> bool {
    false
}

pub fn pre_create_owner_only(path: &Path) -> Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    match opts.open(path) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            // Repair the mode BEFORE any caller opens the file, so writes never land while it is
            // still group/world-readable from an older binary.
            set_owner_only_file(path)
        }
        Err(source) => Err(StackError::FileCreate {
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[cfg(unix)]
pub fn set_owner_only_dir(path: &Path) -> Result<()> {
    set_permissions(path, 0o700)
}

#[cfg(not(unix))]
pub fn set_owner_only_dir(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
pub fn set_owner_only_file(path: &Path) -> Result<()> {
    set_permissions(path, 0o600)
}

#[cfg(not(unix))]
pub fn set_owner_only_file(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_permissions(path: &Path, mode: u32) -> Result<()> {
    std::fs::set_permissions(path, Permissions::from_mode(mode)).map_err(|source| {
        StackError::PermissionSet {
            path: path.to_path_buf(),
            source,
        }
    })
}

#[cfg(unix)]
fn sync_parent_dir(path: &Path) -> Result<()> {
    let parent = parent_dir(path)?;
    let directory = std::fs::File::open(parent).map_err(|source| StackError::FileCreate {
        path: parent.to_path_buf(),
        source,
    })?;
    directory
        .sync_all()
        .map_err(|source| StackError::FileCreate {
            path: parent.to_path_buf(),
            source,
        })
}

#[cfg(not(unix))]
fn sync_parent_dir(_path: &Path) -> Result<()> {
    Ok(())
}

fn sync_file(path: &Path, file: &std::fs::File) -> Result<()> {
    file.sync_all().map_err(|source| StackError::FileCreate {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "test-fixtures")]
    #[test]
    fn temp_home_passes_isolation_check() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        assert!(path_is_under_temp_dir(tempdir.path()));
    }

    #[cfg(feature = "test-fixtures")]
    #[test]
    fn non_temp_home_fails_isolation_check() {
        // The crate root exists but is nowhere near the temp dir, like a developer's real HOME.
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(!path_is_under_temp_dir(repo_root));
        assert!(!path_is_under_temp_dir(Path::new(
            "/definitely/missing/home"
        )));
    }

    #[test]
    fn prune_release_dirs_keeps_the_new_and_previous_release() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        for name in ["old", "previous", "new"] {
            std::fs::create_dir(tempdir.path().join(name)).expect("release dir");
        }

        prune_release_dirs(
            tempdir.path(),
            std::ffi::OsStr::new("new"),
            Some(std::ffi::OsStr::new("previous")),
        );

        let mut remaining: Vec<String> = std::fs::read_dir(tempdir.path())
            .expect("read releases")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        remaining.sort();
        assert_eq!(remaining, ["new", "previous"]);
    }

    #[test]
    fn write_new_file_owner_only_persists_content() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = tempdir.path().join("secret.txt");

        write_new_file_owner_only(&path, b"secret").expect("write");

        assert_eq!(std::fs::read(&path).expect("read"), b"secret");
    }

    #[test]
    fn atomic_write_owner_only_replaces_content() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = tempdir.path().join("secret.txt");
        write_new_file_owner_only(&path, b"old").expect("write old");

        atomic_write_owner_only(&path, b"new").expect("replace");

        assert_eq!(std::fs::read(&path).expect("read"), b"new");
    }

    #[cfg(unix)]
    #[test]
    fn owner_only_writes_use_file_mode_0600() {
        use std::os::unix::fs::PermissionsExt as _;

        let tempdir = tempfile::tempdir().expect("tempdir");
        let created = tempdir.path().join("created.txt");
        let replaced = tempdir.path().join("replaced.txt");

        write_new_file_owner_only(&created, b"created").expect("write created");
        write_new_file_owner_only(&replaced, b"old").expect("write old");
        atomic_write_owner_only(&replaced, b"new").expect("replace");

        let created_mode = std::fs::metadata(&created)
            .expect("created metadata")
            .permissions()
            .mode()
            & 0o777;
        let replaced_mode = std::fs::metadata(&replaced)
            .expect("replaced metadata")
            .permissions()
            .mode()
            & 0o777;

        assert_eq!(created_mode, 0o600);
        assert_eq!(replaced_mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn managed_file_path_rejects_symlinked_parent() {
        use std::os::unix::fs::symlink;

        let home = tempfile::tempdir().expect("home");
        let outside = tempfile::tempdir().expect("outside");
        symlink(outside.path(), home.path().join(".codex")).expect("symlink");

        let result = prepare_owner_managed_file_path(
            home.path(),
            &home.path().join(".codex").join("config.toml"),
        );
        assert!(result.is_err());
        assert!(!outside.path().join("config.toml").exists());
    }

    #[cfg(unix)]
    #[test]
    fn managed_file_path_rejects_hard_linked_target() {
        let home = tempfile::tempdir().expect("home");
        let directory = home.path().join(".codex");
        create_dir_owner_only(&directory).expect("directory");
        let target = directory.join("config.toml");
        write_new_file_owner_only(&target, b"model = 'one'\n").expect("target");
        std::fs::hard_link(&target, directory.join("second-link.toml")).expect("hard link");

        let result = prepare_owner_managed_file_path(home.path(), &target);
        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn read_only_validation_does_not_repair_permissive_mode() {
        use std::os::unix::fs::PermissionsExt as _;

        let home = tempfile::tempdir().expect("home");
        let target = home.path().join("secret");
        std::fs::write(&target, b"secret").expect("write");
        std::fs::set_permissions(&target, Permissions::from_mode(0o644)).expect("chmod");

        assert!(validate_owner_only_regular_file(&target).is_err());
        assert_eq!(
            std::fs::metadata(&target)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            0o644
        );
    }

    #[cfg(unix)]
    #[test]
    fn agent_config_mutation_lock_is_owner_only_and_serializes_process_handles() {
        use std::os::unix::fs::PermissionsExt as _;
        use std::sync::mpsc;
        use std::time::Duration;

        let tempdir = tempfile::tempdir().expect("tempdir");
        let config_path = tempdir.path().join("acps-config.toml");
        let first = acquire_agent_config_mutation_file_lock(&config_path).expect("first lock");
        let lock_path = tempdir.path().join(AGENT_CONFIG_MUTATION_LOCK_FILE_NAME);
        assert_eq!(
            std::fs::metadata(&lock_path)
                .expect("lock metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        let (started_tx, started_rx) = mpsc::channel();
        let (acquired_tx, acquired_rx) = mpsc::channel();
        let config_path_for_thread = config_path.clone();
        let waiter = std::thread::spawn(move || {
            started_tx.send(()).expect("started");
            let second = acquire_agent_config_mutation_file_lock(&config_path_for_thread)
                .expect("second lock");
            acquired_tx.send(()).expect("acquired");
            drop(second);
        });
        started_rx.recv().expect("waiter started");
        assert!(
            acquired_rx
                .recv_timeout(Duration::from_millis(100))
                .is_err()
        );
        drop(first);
        acquired_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("second handle acquires after release");
        waiter.join().expect("waiter joins");
    }

    #[cfg(unix)]
    #[test]
    fn agent_config_mutation_lock_rejects_hard_links() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let config_path = tempdir.path().join("acps-config.toml");
        drop(acquire_agent_config_mutation_file_lock(&config_path).expect("create lock"));
        let lock_path = tempdir.path().join(AGENT_CONFIG_MUTATION_LOCK_FILE_NAME);
        std::fs::hard_link(&lock_path, tempdir.path().join("second-lock-link")).expect("hard link");

        assert!(acquire_agent_config_mutation_file_lock(&config_path).is_err());
    }
}
