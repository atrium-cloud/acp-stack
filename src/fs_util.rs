//! Shared filesystem helpers used by `cli`, `secrets`, and any future module
//! that needs to land files under the runtime user's home with owner-only
//! permissions. The pattern is owner-only (0700 / 0600) on Unix and a no-op
//! on other platforms; the runtime is Linux-targeted but the no-op keeps
//! tests on macOS dev machines honest.

use crate::error::{Result, StackError};
use std::fs::File;
use std::fs::Permissions;
use std::io::Write as _;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const AGENT_CONFIG_MUTATION_LOCK_FILE_NAME: &str = ".agent-config.lock";

/// Process-wide advisory lock for read/modify/write operations that touch the
/// canonical Agent config or a harness's generated global config. The lock
/// file has a stable inode so atomic config replacements do not invalidate the
/// lock held by another `acps` process.
pub struct AgentConfigMutationFileLock {
    _file: File,
}

pub fn acquire_agent_config_mutation_file_lock(
    config_path: &Path,
) -> Result<AgentConfigMutationFileLock> {
    let parent = parent_dir(config_path)?;
    create_dir_owner_only(parent)?;
    let lock_path = parent.join(AGENT_CONFIG_MUTATION_LOCK_FILE_NAME);
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
        .open(&lock_path)
        .map_err(|source| StackError::FileCreate {
            path: lock_path.clone(),
            source,
        })?;
    let metadata = file.metadata().map_err(|source| StackError::FileCreate {
        path: lock_path.clone(),
        source,
    })?;
    if !metadata.is_file()
        || !metadata_owned_by_current_user(&metadata)
        || metadata_has_multiple_links(&metadata)
        || !metadata_is_owner_only_file(&metadata)
    {
        return Err(StackError::FileCreate {
            path: lock_path,
            source: std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "Agent config mutation lock must be a current-user-owned, single-link regular file with mode 0600",
            ),
        });
    }
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd as _;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(StackError::FileCreate {
                path: lock_path,
                source: std::io::Error::last_os_error(),
            });
        }
    }
    Ok(AgentConfigMutationFileLock { _file: file })
}

pub fn home_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .ok_or(StackError::HomeNotSet)?;
    Ok(PathBuf::from(home))
}

pub fn parent_dir(path: &Path) -> Result<&Path> {
    path.parent().ok_or_else(|| StackError::MissingParentDir {
        path: path.to_path_buf(),
    })
}

pub fn create_dir_owner_only(path: &Path) -> Result<()> {
    if path.exists() {
        // Use `symlink_metadata` so a symlink at this path is treated as a
        // non-directory, rather than transparently followed. Allowing a
        // symlink to substitute for the runtime's owned directories would
        // route file creation outside the security-managed tree.
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

/// Atomically replace `path` with `content`, writing through a sibling temp
/// file and renaming. Sets owner-only mode (0600) on both the temp and the
/// final file on Unix. The directory containing `path` must already exist.
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

/// Validate and prepare an allowlisted file target below `home` without
/// following symlinked path components. Existing targets must be regular,
/// single-link files owned by the current user. Missing directories are
/// created one component at a time with owner-only permissions.
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

/// Validate an existing owner-only runtime file without changing its mode or
/// following a symlink. Used by preflight paths that must remain read-only
/// until a queued mutation is safe to apply.
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
            // Repair the mode before any caller opens the file, so writes never land
            // while the file is still group/world-readable from an older binary.
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
