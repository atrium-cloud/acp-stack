//! Filesystem primitives for skill installs and ports. Installed skills live below the workload
//! home and every access there goes through [`WorkloadHome`]. Skill trees arrive untrusted, so
//! every copy refuses symlinks and special files rather than following them, and directory swaps
//! stage in a sibling directory.

use super::*;

use rand::RngExt as _;

use std::time::Duration;

use crate::workload_fs::{
    Anchor, HandoffOptions, LinkPolicy, SymlinkPolicy, WriteOptions, copy_file, create_dir_all,
    handoff_tree, list_dir, remove_tree, rename, stat_followed,
};

// === CONSTANTS ===

const STAGING_RANDOM_BYTES: usize = 8;
/// Bound for copying one skill tree into place; skill archives run up to
/// `GITHUB_ARCHIVE_MAX_BYTES`, far more than a config-sized workload job moves.
const SKILL_TREE_COPY_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Marker proving a skill directory was installed by acp-stack; `remove` and
/// overwrite refuse directories without it, so hand-placed skills are never
/// deleted. It lives inside the skill dir so it cannot diverge from the files.
pub(super) fn write_managed_marker(
    workload: &WorkloadHome,
    target_dir: &Path,
    source_id: &str,
) -> Result<()> {
    let marker = target_dir.join(MANAGED_SKILL_MARKER);
    workload.write_atomic(&marker, format!("{source_id}\n").into_bytes())
}

pub(super) fn has_managed_marker(workload: &WorkloadHome, target_dir: &Path) -> bool {
    let marker = target_dir.join(MANAGED_SKILL_MARKER);
    match workload.stat(&marker) {
        Ok(Some(metadata)) => metadata.kind == EntryKind::File,
        Ok(None) => false,
        Err(source) => {
            // Unreadable reads as unmanaged so nothing gets deleted, but the
            // operator must see why removal suddenly refuses.
            tracing::warn!(
                marker = %marker.display(),
                %source,
                "failed to probe managed skill marker; treating skill as unmanaged"
            );
            false
        }
    }
}

/// Source id recorded in the managed marker, or `None` for hand-placed skills;
/// a bad marker degrades to `None` rather than failing the whole listing.
pub(super) fn read_managed_marker_source(
    workload: &WorkloadHome,
    target_dir: &Path,
) -> Option<String> {
    if !has_managed_marker(workload, target_dir) {
        return None;
    }
    let marker = target_dir.join(MANAGED_SKILL_MARKER);
    let content = workload.read(&marker).and_then(|content| {
        String::from_utf8(content.unwrap_or_default())
            .map_err(|source| skill_io_err("decode managed marker", &marker, source))
    });
    match content {
        Ok(content) => {
            let source_id = content.trim();
            if source_id.is_empty() {
                None
            } else {
                Some(source_id.to_owned())
            }
        }
        Err(source) => {
            tracing::warn!(
                marker = %marker.display(),
                %source,
                "failed to read managed skill marker; omitting source provenance"
            );
            None
        }
    }
}

pub(super) fn ensure_no_installed_skill_ancestor(
    workload: &WorkloadHome,
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
        match workload.stat(&descriptor) {
            Ok(Some(metadata)) if metadata.kind == EntryKind::File => {
                return Err(StackError::SkillInstallTargetConflict {
                    path: ancestor,
                    reason: "nested target would modify an already-installed skill".to_owned(),
                });
            }
            Ok(Some(_)) => {
                return Err(StackError::SkillInstallTargetConflict {
                    path: descriptor,
                    reason: "ancestor SKILL.md is not a regular file".to_owned(),
                });
            }
            Ok(None) => {}
            Err(source) => {
                return Err(StackError::SkillInstallFailed {
                    reason: format!("stat skill ancestor `{}`: {source}", descriptor.display()),
                });
            }
        }
    }
    Ok(())
}

pub(super) fn existing_target_state(
    workload: &WorkloadHome,
    target_dir: &Path,
) -> Result<ExistingTargetState> {
    let metadata = match workload.stat(target_dir) {
        Ok(Some(metadata)) => metadata,
        Ok(None) => {
            return Ok(ExistingTargetState::Missing);
        }
        Err(source) => {
            return Err(StackError::SkillInstallFailed {
                reason: format!("stat skill target `{}`: {source}", target_dir.display()),
            });
        }
    };
    if metadata.kind != EntryKind::Dir {
        return Err(StackError::SkillInstallTargetConflict {
            path: target_dir.to_path_buf(),
            reason: "target exists but is not a directory".to_owned(),
        });
    }
    let descriptor = target_dir.join(SKILL_DESCRIPTOR);
    let descriptor_metadata = match workload.stat(&descriptor) {
        Ok(Some(metadata)) => metadata,
        Ok(None) => {
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
    if descriptor_metadata.kind != EntryKind::File {
        return Err(StackError::SkillInstallTargetConflict {
            path: target_dir.to_path_buf(),
            reason: "target SKILL.md is not a regular file".to_owned(),
        });
    }
    Ok(ExistingTargetState::AlreadyInstalled)
}

/// Copy a skill tree into place via a sibling staging directory and rename. An install
/// (`managed_source` set) hands a runtime-staged archive tree in, and the managed marker is
/// written INSIDE the staging directory, so a skill can never reach the target without it. A
/// port copies between roots below the workload home and keeps the source dir's marker.
pub(super) fn copy_skill_dir_atomically(
    workload: &WorkloadHome,
    source_dir: &Path,
    target_dir: &Path,
    skill_name: &str,
    managed_source: Option<&str>,
) -> Result<()> {
    let staging = staging_path(target_dir, skill_name, "")?;
    let staged = match managed_source {
        Some(source_id) => handoff_tree(
            workload.executor(),
            source_dir,
            workload.home(),
            &workload.relative(&staging)?,
            &HandoffOptions {
                symlinks: SymlinkPolicy::Reject,
                hard_links: workload.accepts_hard_links(),
                destination_links: LinkPolicy::Follow { contained: false },
                file_mode: workload.file_mode(),
                dir_mode: workload.dir_mode(),
                timeout: SKILL_TREE_COPY_TIMEOUT,
            },
        )
        .and_then(|_| write_managed_marker(workload, &staging, source_id)),
        None => copy_dir_recursive(workload, source_dir, &staging),
    };
    let staging = workload.relative(&staging)?;
    let target = workload.relative(target_dir)?;
    let target_display = target_dir.to_path_buf();
    workload.run(move |anchor, _| {
        let moved = staged.and_then(|()| {
            rename(anchor, &staging, &target)
                .map_err(|source| skill_io_err("move installed skill to", &target_display, source))
        });
        if moved.is_err() {
            discard_staging(anchor, &staging);
        }
        moved
    })
}

pub(super) fn replace_skill_dir_atomically(
    workload: &WorkloadHome,
    source_dir: &Path,
    target_dir: &Path,
    skill_name: &str,
) -> Result<()> {
    let staging = staging_path(target_dir, skill_name, "")?;
    let backup = staging_path(target_dir, skill_name, ".backup")?;
    let copied = copy_dir_recursive(workload, source_dir, &staging);
    let staging = workload.relative(&staging)?;
    let backup = workload.relative(&backup)?;
    let target = workload.relative(target_dir)?;
    let target_display = target_dir.to_path_buf();
    workload.run(move |anchor, _| {
        if let Err(error) = copied.and_then(|()| rename(anchor, &target, &backup)) {
            discard_staging(anchor, &staging);
            return Err(error);
        }
        if let Err(source) = rename(anchor, &staging, &target) {
            let restore_message = rename(anchor, &backup, &target)
                .err()
                .map(|err| format!("; restore failed: {err}"))
                .unwrap_or_default();
            discard_staging(anchor, &staging);
            return Err(StackError::SkillInstallFailed {
                reason: format!(
                    "replace installed skill at `{}`: {source}{restore_message}",
                    target_display.display()
                ),
            });
        }
        discard_staging(anchor, &backup);
        Ok(())
    })
}

fn skill_temp_prefix(skill_name: &str) -> &str {
    skill_name.rsplit('/').next().unwrap_or("skill")
}

/// A fresh hidden sibling of `target_dir` to stage a copy or a backup in.
fn staging_path(target_dir: &Path, skill_name: &str, label: &str) -> Result<PathBuf> {
    let parent = target_dir
        .parent()
        .ok_or_else(|| StackError::SkillInstallFailed {
            reason: format!("skill target `{}` has no parent", target_dir.display()),
        })?;
    let mut random = [0u8; STAGING_RANDOM_BYTES];
    rand::rng().fill(&mut random);
    let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(parent.join(format!(
        ".{}{label}.{suffix}",
        skill_temp_prefix(skill_name)
    )))
}

fn discard_staging(anchor: &Anchor, staging: &Path) {
    match remove_tree(anchor, staging) {
        Ok(()) | Err(StackError::WorkloadFsNotFound { .. }) => {}
        Err(error) => {
            tracing::warn!(
                staging = %staging.display(),
                %error,
                "failed to remove a skill staging directory"
            );
        }
    }
}

/// Copy a tree between two places below the workload home as one workload job; the source is
/// workload-controlled, so it is never read with the runtime's own credentials.
fn copy_dir_recursive(workload: &WorkloadHome, source_dir: &Path, target_dir: &Path) -> Result<()> {
    let source = workload.relative(source_dir)?;
    let target = workload.relative(target_dir)?;
    workload.run_with_timeout(SKILL_TREE_COPY_TIMEOUT, move |anchor, options| {
        copy_tree(anchor, &source, &target, options)
    })
}

fn copy_tree(anchor: &Anchor, source: &Path, target: &Path, options: &WriteOptions) -> Result<()> {
    create_dir_all(anchor, target, options.dir_mode)?;
    for (name, metadata) in list_dir(anchor, source)? {
        let entry_path = source.join(&name);
        let target_path = target.join(&name);
        match metadata.kind {
            EntryKind::Dir => copy_tree(anchor, &entry_path, &target_path, options)?,
            EntryKind::File => copy_file(anchor, &entry_path, &target_path, options)?,
            EntryKind::Symlink => {
                return Err(StackError::SkillInstallFailed {
                    reason: format!(
                        "refusing to port symlink `{}`",
                        anchor.path().join(&entry_path).display()
                    ),
                });
            }
            EntryKind::Other => {
                return Err(StackError::SkillInstallFailed {
                    reason: format!(
                        "refusing to port special file `{}`",
                        anchor.path().join(&entry_path).display()
                    ),
                });
            }
        }
    }
    Ok(())
}

pub(super) fn validate_skill_dir_for_port(
    workload: &WorkloadHome,
    source_dir: &Path,
) -> Result<()> {
    for (name, metadata) in workload.list_dir(source_dir)? {
        let entry_path = source_dir.join(name);
        if metadata.kind == EntryKind::Symlink {
            return Err(StackError::SkillInstallFailed {
                reason: format!("refusing to port symlink `{}`", entry_path.display()),
            });
        }
        if metadata.kind == EntryKind::Dir {
            validate_skill_dir_for_port(workload, &entry_path)?;
        } else if metadata.kind != EntryKind::File {
            return Err(StackError::SkillInstallFailed {
                reason: format!("refusing to port special file `{}`", entry_path.display()),
            });
        }
    }
    Ok(())
}

/// Walk `path` below the workload home one component at a time, following links: every existing
/// component must resolve to a directory, and missing ones are created when `create_missing` is
/// set. Returns whether `path` exists.
pub(super) fn ensure_directory_path(
    workload: &WorkloadHome,
    path: &Path,
    create_missing: bool,
) -> Result<bool> {
    let relative = workload.relative(path)?;
    workload.run(move |anchor, options| {
        let mut current = PathBuf::new();
        for component in relative.components() {
            current.push(component);
            match stat_followed(anchor, &current)? {
                Some(metadata) if metadata.kind == EntryKind::Dir => {}
                Some(_) => {
                    return Err(StackError::SkillInstallTargetConflict {
                        path: anchor.path().join(&current),
                        reason: "skill directory path segment is not a directory".to_owned(),
                    });
                }
                None if create_missing => create_dir_all(anchor, &current, options.dir_mode)?,
                None => return Ok(false),
            }
        }
        Ok(true)
    })
}

/// Whether the skills directory at `path` exists, following symlinks as the workload home's
/// link policy allows.
pub(super) fn skill_directory_exists(workload: &WorkloadHome, path: &Path) -> Result<bool> {
    match workload.list_dir(path) {
        Ok(_) => Ok(true),
        Err(StackError::WorkloadFsNotFound { .. }) => Ok(false),
        Err(error) => Err(error),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ExistingTargetState {
    Missing,
    AlreadyInstalled,
}
