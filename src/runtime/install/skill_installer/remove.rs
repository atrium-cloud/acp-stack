//! Day-2 read and delete surfaces: listing what is installed under an agent's
//! shared install root, and removing one managed skill from it.

use super::*;

/// Enumerate the skills currently installed under an agent's shared install
/// root. Day-2 read surface for `acps skills list` and the HTTP list route.
///
/// Returns an empty list — never an error — when the agent declares no skills
/// support, has no install dir, or the root does not yet exist: hosted callers
/// must be able to render the "nothing installed" state. Only a genuine read
/// failure of an existing root propagates. Enumeration reuses the tolerant
/// link collector, so a stray non-skill entry is skipped with a warning rather
/// than failing the whole listing.
pub fn list_installed_skills(home: &Path, entry: &RegistryEntry) -> Result<Vec<InstalledSkill>> {
    let Some(root) = agent_skill_root(home, entry)? else {
        return Ok(Vec::new());
    };
    // Canonicalize so a dotfiles-managed symlinked install root (e.g.
    // `~/.agents` -> elsewhere) is followed the same way `remove_agent_skill`
    // and `link_agent_skills` follow it, instead of being silently reported as
    // empty. A missing root simply means nothing is installed yet.
    let canonical_root = match root.canonicalize() {
        Ok(path) => path,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(skill_io_err("resolve skill install dir", &root, source));
        }
    };
    if !canonical_root.is_dir() {
        return Ok(Vec::new());
    }
    let mut candidates = Vec::new();
    collect_link_skill_directories(&canonical_root, &canonical_root, &mut candidates)?;
    let mut skills = candidates
        .into_iter()
        .map(|(name, path)| {
            let source = read_managed_marker_source(&path);
            InstalledSkill { name, path, source }
        })
        .collect::<Vec<_>>();
    skills.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(skills)
}

/// Remove one installed skill from an agent's shared install root and prune the
/// now-dangling symlink mirror. Day-2 write surface for `acps skills remove`
/// and the HTTP remove route.
///
/// `skill_name` is the install name (a `/`-joined relative path for nested
/// skills). The target must be a real *managed* skill — a directory with a
/// regular `SKILL.md` and the `.acp-stack-managed` marker written at install
/// time. A missing target is `SkillNotInstalled` (404); a path that exists but
/// is not a clean managed skill — including a user's own folder, which carries
/// no marker — surfaces as the same conflict the installer raises (409), so
/// manually added skills are never deleted here. After removal, emptied parent
/// group directories are cleaned up to (not including) the root, and the
/// caller is expected to refresh links via [`link_agent_skills_best_effort`]
/// so the dangling mirror symlink (Claude Code / Hermes) is pruned.
pub fn remove_agent_skill(
    home: &Path,
    entry: &RegistryEntry,
    skill_name: &str,
) -> Result<SkillRemoveReport> {
    validate_install_target_name(skill_name)?;
    let root = agent_skill_root(home, entry)?.ok_or_else(|| StackError::SkillInstallFailed {
        reason: format!(
            "agent `{}` does not declare an Agent Skills install directory",
            entry.id
        ),
    })?;
    // Canonicalize the install root so a dotfiles-managed symlinked ancestor
    // (e.g. `~/.agents`) is followed the same way linking does, instead of
    // being rejected. A missing root means nothing is installed.
    let canonical_root = match root.canonicalize() {
        Ok(path) => path,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Err(StackError::SkillNotInstalled {
                skill: skill_name.to_owned(),
            });
        }
        Err(source) => {
            return Err(skill_io_err("resolve skill install dir", &root, source));
        }
    };
    // Walk into the skill directory one component at a time, rejecting any
    // symlinked path segment below the (already real) root so the recursive
    // remove can never be redirected outside the managed root.
    let mut target_dir = canonical_root.clone();
    for component in skill_name.split('/') {
        target_dir.push(component);
        match std::fs::symlink_metadata(&target_dir) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(StackError::SkillInstallTargetConflict {
                    path: target_dir,
                    reason: "skill path segment is a symlink".to_owned(),
                });
            }
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                return Err(StackError::SkillInstallTargetConflict {
                    path: target_dir,
                    reason: "skill path segment is not a directory".to_owned(),
                });
            }
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Err(StackError::SkillNotInstalled {
                    skill: skill_name.to_owned(),
                });
            }
            Err(source) => {
                return Err(skill_io_err("stat skill path", &target_dir, source));
            }
        }
    }
    // Confirm it is a real installed skill (a directory with a regular
    // SKILL.md); a directory without one surfaces as the installer's conflict.
    match existing_target_state(&target_dir)? {
        ExistingTargetState::AlreadyInstalled => {}
        ExistingTargetState::Missing => {
            return Err(StackError::SkillNotInstalled {
                skill: skill_name.to_owned(),
            });
        }
    }
    // Only skills acp-stack installed carry the managed marker. Anything else
    // in the install root is the user's own content and must never be deleted
    // here, even when it looks exactly like a managed skill.
    if !has_managed_marker(&target_dir) {
        return Err(StackError::SkillInstallTargetConflict {
            path: target_dir,
            reason: "skill was not installed by acp-stack; refusing to remove it".to_owned(),
        });
    }
    std::fs::remove_dir_all(&target_dir)
        .map_err(|source| skill_io_err("remove installed skill", &target_dir, source))?;
    // The skill is already gone; a failure to tidy an emptied group directory
    // is cosmetic, so log it and continue rather than reporting the completed
    // removal as a failure (which would also skip the caller's link refresh).
    if let Err(error) = remove_emptied_group_parents(&canonical_root, &target_dir) {
        tracing::warn!(error = %error, "skill group directory cleanup failed after removal");
    }
    Ok(SkillRemoveReport {
        install_root: canonical_root,
        removed: SkillInstallEntry {
            name: skill_name.to_owned(),
            path: target_dir,
        },
    })
}

/// Remove parent group directories left empty by a nested-skill removal,
/// walking up from the removed skill to (but not including) the install root.
/// A directory with any remaining content stops the walk.
fn remove_emptied_group_parents(root: &Path, target_dir: &Path) -> Result<()> {
    let mut current = target_dir.parent();
    while let Some(dir) = current {
        if dir == root || !dir.starts_with(root) {
            break;
        }
        match std::fs::read_dir(dir) {
            Ok(mut entries) => {
                if entries.next().is_some() {
                    break;
                }
            }
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => break,
            Err(source) => {
                return Err(skill_io_err("read skill group directory", dir, source));
            }
        }
        std::fs::remove_dir(dir)
            .map_err(|source| skill_io_err("remove emptied skill group directory", dir, source))?;
        current = dir.parent();
    }
    Ok(())
}
