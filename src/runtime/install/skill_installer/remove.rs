//! Day-2 read and delete surfaces: listing what is installed under an agent's
//! shared install root, and removing one managed skill from it.

use super::*;

/// Enumerate the skills currently installed under an agent's shared install root.
/// No skills support, no install dir, or a missing root all yield an empty list
/// rather than an error, so callers can render the "nothing installed" state.
pub fn list_installed_skills(
    workload: &WorkloadHome,
    entry: &RegistryEntry,
) -> Result<Vec<InstalledSkill>> {
    let Some(root) = agent_skill_root(&workload.canonical_home()?, entry)? else {
        return Ok(Vec::new());
    };
    if !skill_directory_exists(workload, &root)? {
        return Ok(Vec::new());
    }
    let mut candidates = Vec::new();
    collect_link_skill_directories(workload, &root, &root, &mut candidates)?;
    let mut skills = candidates
        .into_iter()
        .map(|(name, path)| {
            let source = read_managed_marker_source(workload, &path);
            InstalledSkill { name, path, source }
        })
        .collect::<Vec<_>>();
    skills.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(skills)
}

/// Remove one installed skill from an agent's shared install root. The target must
/// carry the `.acp-stack-managed` marker: anything else is the user's own content
/// and surfaces as a conflict rather than being deleted. Callers refresh links
/// afterwards via [`link_agent_skills_best_effort`] to prune the dangling mirror.
pub fn remove_agent_skill(
    workload: &WorkloadHome,
    entry: &RegistryEntry,
    skill_name: &str,
) -> Result<SkillRemoveReport> {
    validate_install_target_name(skill_name)?;
    let root = agent_skill_root(&workload.canonical_home()?, entry)?.ok_or_else(|| {
        StackError::SkillInstallFailed {
            reason: format!(
                "agent `{}` does not declare an Agent Skills install directory",
                entry.id
            ),
        }
    })?;
    // A symlinked group segment would redirect the recursive remove outside the install root.
    let mut target_dir = root.clone();
    for component in skill_name.split('/') {
        target_dir.push(component);
        let reason = match workload.stat(&target_dir)? {
            None => {
                return Err(StackError::SkillNotInstalled {
                    skill: skill_name.to_owned(),
                });
            }
            Some(metadata) => match metadata.kind {
                EntryKind::Dir => continue,
                EntryKind::Symlink => "skill path segment is a symlink",
                EntryKind::File | EntryKind::Other => "skill path segment is not a directory",
            },
        };
        return Err(StackError::SkillInstallTargetConflict {
            path: target_dir,
            reason: reason.to_owned(),
        });
    }
    match existing_target_state(workload, &target_dir)? {
        ExistingTargetState::AlreadyInstalled => {}
        ExistingTargetState::Missing => {
            return Err(StackError::SkillNotInstalled {
                skill: skill_name.to_owned(),
            });
        }
    }
    // Only skills acp-stack installed carry the marker; the user's own content must
    // never be deleted here, even when it looks exactly like a managed skill.
    if !has_managed_marker(workload, &target_dir) {
        return Err(StackError::SkillInstallTargetConflict {
            path: target_dir,
            reason: "skill was not installed by acp-stack; refusing to remove it".to_owned(),
        });
    }
    workload
        .remove_tree(&target_dir)
        .map_err(|source| skill_io_err("remove installed skill", &target_dir, source))?;
    // The skill is already gone; a cleanup failure must not report the completed
    // removal as failed, which would also skip the caller's link refresh.
    if let Err(error) = remove_emptied_group_parents(workload, &root, &target_dir) {
        tracing::warn!(error = %error, "skill group directory cleanup failed after removal");
    }
    Ok(SkillRemoveReport {
        install_root: root,
        removed: SkillInstallEntry {
            name: skill_name.to_owned(),
            path: target_dir,
        },
    })
}

/// Remove parent group directories left empty by a nested-skill removal, walking up
/// to (but not including) the install root.
fn remove_emptied_group_parents(
    workload: &WorkloadHome,
    root: &Path,
    target_dir: &Path,
) -> Result<()> {
    let mut current = target_dir.parent();
    while let Some(dir) = current {
        if dir == root || !dir.starts_with(root) {
            break;
        }
        match workload.list_dir(dir) {
            Ok(entries) => {
                if !entries.is_empty() {
                    break;
                }
            }
            Err(StackError::WorkloadFsNotFound { .. }) => break,
            Err(source) => {
                return Err(skill_io_err("read skill group directory", dir, source));
            }
        }
        workload
            .remove_empty_dir(dir)
            .map_err(|source| skill_io_err("remove emptied skill group directory", dir, source))?;
        current = dir.parent();
    }
    Ok(())
}
