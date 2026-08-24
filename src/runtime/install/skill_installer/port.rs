//! Porting installed skills between agents on `agent switch`, when the two
//! agents do not already share an install root.

use super::*;

pub fn port_agent_skills(
    home: &Path,
    registry: &RegistryCatalog,
    old_agent_id: &str,
    target_agent_id: &str,
) -> Result<Option<SkillPortReport>> {
    let home = home
        .canonicalize()
        .map_err(|source| skill_io_err("canonicalize home directory", home, source))?;
    let Some(old_entry) = registry.lookup(old_agent_id) else {
        return Ok(None);
    };
    let target_entry =
        registry
            .lookup(target_agent_id)
            .ok_or_else(|| StackError::AgentRegistryMissing {
                id: target_agent_id.to_owned(),
            })?;
    let Some(source_root) = agent_skill_root(&home, old_entry)? else {
        return Ok(None);
    };
    let Some(target_root) = agent_skill_root(&home, target_entry)? else {
        return Ok(None);
    };
    port_skill_directories(&source_root, &target_root).map(Some)
}

pub(super) fn port_skill_directories(
    source_root: &Path,
    target_root: &Path,
) -> Result<SkillPortReport> {
    if source_root == target_root {
        return Ok(SkillPortReport {
            source_root: source_root.to_path_buf(),
            target_root: target_root.to_path_buf(),
            status: SkillPortStatus::Shared,
            copied: Vec::new(),
            overwritten: Vec::new(),
            kept_unmanaged: Vec::new(),
        });
    }
    if !source_root_exists_without_symlink_ancestors(source_root)? {
        return Ok(SkillPortReport {
            source_root: source_root.to_path_buf(),
            target_root: target_root.to_path_buf(),
            status: SkillPortStatus::NoneFound,
            copied: Vec::new(),
            overwritten: Vec::new(),
            kept_unmanaged: Vec::new(),
        });
    }
    let mut candidates = Vec::new();
    collect_port_skill_directories(source_root, source_root, &mut candidates)?;

    if candidates.is_empty() {
        return Ok(SkillPortReport {
            source_root: source_root.to_path_buf(),
            target_root: target_root.to_path_buf(),
            status: SkillPortStatus::NoneFound,
            copied: Vec::new(),
            overwritten: Vec::new(),
            kept_unmanaged: Vec::new(),
        });
    }

    ensure_directory_no_symlink_ancestors(target_root, true)?;
    let mut installs = Vec::with_capacity(candidates.len());
    let mut kept_unmanaged = Vec::new();
    for (skill_name, entry_path) in candidates {
        let target_dir = target_root.join(&skill_name);
        let target_parent = target_dir
            .parent()
            .ok_or_else(|| StackError::SkillInstallFailed {
                reason: format!("skill target `{}` has no parent", target_dir.display()),
            })?;
        ensure_directory_no_symlink_ancestors(target_parent, true)?;
        let action = match existing_target_state(&target_dir)? {
            ExistingTargetState::Missing => PortAction::Copy,
            ExistingTargetState::AlreadyInstalled if has_managed_marker(&target_dir) => {
                PortAction::Overwrite
            }
            ExistingTargetState::AlreadyInstalled => {
                // A same-named skill the runtime did not install stays untouched.
                tracing::warn!(
                    skill = %skill_name,
                    "skipping port overwrite of a skill not installed by acp-stack"
                );
                kept_unmanaged.push(SkillInstallEntry {
                    name: skill_name,
                    path: target_dir,
                });
                continue;
            }
        };
        installs.push(ResolvedPort {
            name: skill_name,
            source_dir: entry_path,
            target_dir,
            action,
        });
    }

    let mut copied = Vec::new();
    let mut overwritten = Vec::new();
    for install in installs {
        match install.action {
            PortAction::Copy => {
                // Ported skills keep the source dir's marker; nothing new is
                // installed here, so no marker is staged.
                copy_skill_dir_atomically(
                    &install.source_dir,
                    &install.target_dir,
                    &install.name,
                    None,
                )?;
                copied.push(SkillInstallEntry {
                    name: install.name,
                    path: install.target_dir,
                });
            }
            PortAction::Overwrite => {
                replace_skill_dir_atomically(
                    &install.source_dir,
                    &install.target_dir,
                    &install.name,
                )?;
                overwritten.push(SkillInstallEntry {
                    name: install.name,
                    path: install.target_dir,
                });
            }
        }
    }
    copied.sort_by(|left, right| left.name.cmp(&right.name));
    overwritten.sort_by(|left, right| left.name.cmp(&right.name));
    kept_unmanaged.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(SkillPortReport {
        source_root: source_root.to_path_buf(),
        target_root: target_root.to_path_buf(),
        status: SkillPortStatus::Copied,
        copied,
        overwritten,
        kept_unmanaged,
    })
}

#[derive(Debug)]
struct ResolvedPort {
    name: String,
    source_dir: PathBuf,
    target_dir: PathBuf,
    action: PortAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PortAction {
    Copy,
    Overwrite,
}
