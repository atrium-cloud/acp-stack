//! Shared filesystem helpers used by `cli`, `secrets`, and any future module
//! that needs to land files under the runtime user's home with owner-only
//! permissions. The pattern is owner-only (0700 / 0600) on Unix and a no-op
//! on other platforms; the runtime is Linux-targeted but the no-op keeps
//! tests on macOS dev machines honest.

use crate::error::{Result, StackError};
use std::fs::Permissions;
use std::io::Write as _;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

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
}
