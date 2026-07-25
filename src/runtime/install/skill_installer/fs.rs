//! Filesystem primitives for skill installs and ports.
//!
//! Everything here is safety-critical: skill trees arrive from downloaded
//! archives or another agent's home directory, so each traversal refuses
//! symlinks and special files rather than following them, and directory
//! swaps are staged in a sibling temporary directory so a failed install
//! never leaves a half-written skill in place.

use super::*;

pub(super) fn ensure_no_installed_skill_ancestor(
    destination_root: &Path,
    skill_name: &str,
) -> Result<()> {
    let mut ancestor = destination_root.to_path_buf();
    let mut components = skill_name.split('/').peekable();
    while let Some(component) = components.next() {
        if components.peek().is_none() {
            break;
        }
        ancestor.push(component);
        let descriptor = ancestor.join(SKILL_DESCRIPTOR);
        match std::fs::symlink_metadata(&descriptor) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                return Err(StackError::SkillInstallTargetConflict {
                    path: ancestor,
                    reason: "nested target would modify an already-installed skill".to_owned(),
                });
            }
            Ok(_) => {
                return Err(StackError::SkillInstallTargetConflict {
                    path: descriptor,
                    reason: "ancestor SKILL.md is not a regular file".to_owned(),
                });
            }
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(StackError::SkillInstallFailed {
                    reason: format!("stat skill ancestor `{}`: {source}", descriptor.display()),
                });
            }
        }
    }
    Ok(())
}

pub(super) fn existing_target_state(target_dir: &Path) -> Result<ExistingTargetState> {
    let metadata = match std::fs::symlink_metadata(target_dir) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ExistingTargetState::Missing);
        }
        Err(source) => {
            return Err(StackError::SkillInstallFailed {
                reason: format!("stat skill target `{}`: {source}", target_dir.display()),
            });
        }
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(StackError::SkillInstallTargetConflict {
            path: target_dir.to_path_buf(),
            reason: "target exists but is not a directory".to_owned(),
        });
    }
    let descriptor = target_dir.join(SKILL_DESCRIPTOR);
    let descriptor_metadata = match std::fs::symlink_metadata(&descriptor) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Err(StackError::SkillInstallTargetConflict {
                path: target_dir.to_path_buf(),
                reason: "target directory exists without SKILL.md".to_owned(),
            });
        }
        Err(source) => {
            return Err(StackError::SkillInstallFailed {
                reason: format!(
                    "stat skill target descriptor `{}`: {source}",
                    descriptor.display()
                ),
            });
        }
    };
    if descriptor_metadata.file_type().is_symlink() || !descriptor_metadata.is_file() {
        return Err(StackError::SkillInstallTargetConflict {
            path: target_dir.to_path_buf(),
            reason: "target SKILL.md is not a regular file".to_owned(),
        });
    }
    Ok(ExistingTargetState::AlreadyInstalled)
}

pub(super) fn copy_skill_dir_atomically(
    source_dir: &Path,
    target_dir: &Path,
    skill_name: &str,
) -> Result<()> {
    let parent = target_dir
        .parent()
        .ok_or_else(|| StackError::SkillInstallFailed {
            reason: format!("skill target `{}` has no parent", target_dir.display()),
        })?;
    let tempdir = tempfile::Builder::new()
        .prefix(&format!(".{}.", skill_temp_prefix(skill_name)))
        .tempdir_in(parent)
        .map_err(|source| StackError::SkillInstallFailed {
            reason: format!(
                "create temporary skill target in `{}`: {source}",
                parent.display()
            ),
        })?;
    copy_dir_recursive(source_dir, tempdir.path())?;
    std::fs::rename(tempdir.path(), target_dir).map_err(|source| {
        StackError::SkillInstallFailed {
            reason: format!(
                "move installed skill to `{}`: {source}",
                target_dir.display()
            ),
        }
    })?;
    std::mem::forget(tempdir);
    Ok(())
}

pub(super) fn replace_skill_dir_atomically(
    source_dir: &Path,
    target_dir: &Path,
    skill_name: &str,
) -> Result<()> {
    let parent = target_dir
        .parent()
        .ok_or_else(|| StackError::SkillInstallFailed {
            reason: format!("skill target `{}` has no parent", target_dir.display()),
        })?;
    let tempdir = tempfile::Builder::new()
        .prefix(&format!(".{}.", skill_temp_prefix(skill_name)))
        .tempdir_in(parent)
        .map_err(|source| StackError::SkillInstallFailed {
            reason: format!(
                "create temporary skill target in `{}`: {source}",
                parent.display()
            ),
        })?;
    copy_dir_recursive(source_dir, tempdir.path())?;

    let backup = tempfile::Builder::new()
        .prefix(&format!(".{}.backup.", skill_temp_prefix(skill_name)))
        .tempdir_in(parent)
        .map_err(|source| StackError::SkillInstallFailed {
            reason: format!(
                "create temporary skill backup in `{}`: {source}",
                parent.display()
            ),
        })?;
    let backup_path = backup.path().to_path_buf();
    std::fs::remove_dir(&backup_path).map_err(|source| StackError::SkillInstallFailed {
        reason: format!("prepare skill backup `{}`: {source}", backup_path.display()),
    })?;
    std::fs::rename(target_dir, &backup_path).map_err(|source| StackError::SkillInstallFailed {
        reason: format!(
            "move existing skill `{}` to backup `{}`: {source}",
            target_dir.display(),
            backup_path.display()
        ),
    })?;
    if let Err(source) = std::fs::rename(tempdir.path(), target_dir) {
        let restore = std::fs::rename(&backup_path, target_dir);
        let restore_message = restore
            .err()
            .map(|err| format!("; restore failed: {err}"))
            .unwrap_or_default();
        return Err(StackError::SkillInstallFailed {
            reason: format!(
                "replace installed skill at `{}`: {source}{restore_message}",
                target_dir.display()
            ),
        });
    }
    std::mem::forget(tempdir);
    Ok(())
}

fn skill_temp_prefix(skill_name: &str) -> &str {
    skill_name.rsplit('/').next().unwrap_or("skill")
}

pub(super) fn copy_dir_recursive(source_dir: &Path, target_dir: &Path) -> Result<()> {
    let metadata =
        std::fs::symlink_metadata(source_dir).map_err(|source| StackError::SkillInstallFailed {
            reason: format!("stat source `{}`: {source}", source_dir.display()),
        })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(StackError::SkillInstallFailed {
            reason: format!("source `{}` is not a directory", source_dir.display()),
        });
    }
    create_dir_owner_only(target_dir)?;
    for entry in std::fs::read_dir(source_dir).map_err(|source| StackError::SkillInstallFailed {
        reason: format!("read source directory `{}`: {source}", source_dir.display()),
    })? {
        let entry = entry.map_err(|source| StackError::SkillInstallFailed {
            reason: format!(
                "read source directory entry `{}`: {source}",
                source_dir.display()
            ),
        })?;
        let entry_path = entry.path();
        let entry_name = entry.file_name();
        let target_path = target_dir.join(entry_name);
        let entry_metadata = std::fs::symlink_metadata(&entry_path).map_err(|source| {
            StackError::SkillInstallFailed {
                reason: format!("stat source entry `{}`: {source}", entry_path.display()),
            }
        })?;
        if entry_metadata.file_type().is_symlink() {
            return Err(StackError::SkillInstallFailed {
                reason: format!("refusing to install symlink `{}`", entry_path.display()),
            });
        }
        if entry_metadata.is_dir() {
            copy_dir_recursive(&entry_path, &target_path)?;
        } else if entry_metadata.is_file() {
            std::fs::copy(&entry_path, &target_path).map_err(|source| {
                StackError::SkillInstallFailed {
                    reason: format!(
                        "copy skill file `{}` -> `{}`: {source}",
                        entry_path.display(),
                        target_path.display()
                    ),
                }
            })?;
            set_owner_only_file(&target_path)?;
        } else {
            return Err(StackError::SkillInstallFailed {
                reason: format!(
                    "refusing to install special file `{}`",
                    entry_path.display()
                ),
            });
        }
    }
    set_owner_only_dir(target_dir)
}

pub(super) fn validate_skill_dir_for_port(source_dir: &Path) -> Result<()> {
    let metadata =
        std::fs::symlink_metadata(source_dir).map_err(|source| StackError::SkillInstallFailed {
            reason: format!("stat source `{}`: {source}", source_dir.display()),
        })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(StackError::SkillInstallFailed {
            reason: format!("source `{}` is not a directory", source_dir.display()),
        });
    }
    for entry in std::fs::read_dir(source_dir).map_err(|source| StackError::SkillInstallFailed {
        reason: format!("read source directory `{}`: {source}", source_dir.display()),
    })? {
        let entry = entry.map_err(|source| StackError::SkillInstallFailed {
            reason: format!(
                "read source directory entry `{}`: {source}",
                source_dir.display()
            ),
        })?;
        let entry_path = entry.path();
        let entry_metadata = std::fs::symlink_metadata(&entry_path).map_err(|source| {
            StackError::SkillInstallFailed {
                reason: format!("stat source entry `{}`: {source}", entry_path.display()),
            }
        })?;
        if entry_metadata.file_type().is_symlink() {
            return Err(StackError::SkillInstallFailed {
                reason: format!("refusing to port symlink `{}`", entry_path.display()),
            });
        }
        if entry_metadata.is_dir() {
            validate_skill_dir_for_port(&entry_path)?;
        } else if !entry_metadata.is_file() {
            return Err(StackError::SkillInstallFailed {
                reason: format!("refusing to port special file `{}`", entry_path.display()),
            });
        }
    }
    Ok(())
}

pub(super) fn ensure_directory_no_symlink_ancestors(
    path: &Path,
    create_missing: bool,
) -> Result<()> {
    let mut current = PathBuf::new();
    let mut normal_components = 0usize;
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            Component::RootDir => current.push(component.as_os_str()),
            Component::Normal(part) => {
                normal_components += 1;
                current.push(part);
            }
            Component::CurDir | Component::ParentDir => {
                return Err(StackError::SkillInstallFailed {
                    reason: format!(
                        "skill install directory `{}` contains an unsafe path segment",
                        path.display()
                    ),
                });
            }
        }
        if current.as_os_str().is_empty() || matches!(component, Component::RootDir) {
            continue;
        }
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(StackError::SkillInstallTargetConflict {
                        path: current.clone(),
                        reason: "install directory path segment is not a real directory".to_owned(),
                    });
                }
            }
            Err(source) if source.kind() == std::io::ErrorKind::NotFound && create_missing => {
                create_single_owner_only_dir(&current)?;
            }
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Err(StackError::SkillInstallFailed {
                    reason: format!("skill install directory `{}` is missing", current.display()),
                });
            }
            Err(source) => {
                return Err(StackError::SkillInstallFailed {
                    reason: format!(
                        "stat skill install directory `{}`: {source}",
                        current.display()
                    ),
                });
            }
        }
    }
    if normal_components == 0 {
        return Err(StackError::SkillInstallFailed {
            reason: format!("skill install directory `{}` is not valid", path.display()),
        });
    }
    set_owner_only_dir(path)
}

pub(super) fn source_root_exists_without_symlink_ancestors(path: &Path) -> Result<bool> {
    let mut current = PathBuf::new();
    let mut normal_components = 0usize;
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            Component::RootDir => current.push(component.as_os_str()),
            Component::Normal(part) => {
                normal_components += 1;
                current.push(part);
            }
            Component::CurDir | Component::ParentDir => {
                return Err(StackError::SkillInstallFailed {
                    reason: format!(
                        "skill source directory `{}` contains an unsafe path segment",
                        path.display()
                    ),
                });
            }
        }
        if current.as_os_str().is_empty() || matches!(component, Component::RootDir) {
            continue;
        }
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(StackError::SkillInstallTargetConflict {
                        path: current.clone(),
                        reason: "source skills path segment is not a real directory".to_owned(),
                    });
                }
            }
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(source) => {
                return Err(StackError::SkillInstallFailed {
                    reason: format!(
                        "stat skill source directory `{}`: {source}",
                        current.display()
                    ),
                });
            }
        }
    }
    if normal_components == 0 {
        return Err(StackError::SkillInstallFailed {
            reason: format!("skill source directory `{}` is not valid", path.display()),
        });
    }
    Ok(true)
}

fn create_single_owner_only_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
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
        std::fs::DirBuilder::new()
            .create(path)
            .map_err(|source| StackError::DirectoryCreate {
                path: path.to_path_buf(),
                source,
            })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ExistingTargetState {
    Missing,
    AlreadyInstalled,
}
