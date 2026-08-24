//! The symlink mirror: reflecting the shared install root into a harness's
//! own skills directory, and pruning what an uninstall left behind.

use super::*;

/// Idempotently symlink every skill under the agent's install root into its
/// `agent_skills_link_dir`, for harnesses that only discover skills from their own directory.
/// The mirror is one-way: the install root is the source of truth and real files at a link path
/// are reported as conflicts, never overwritten.
pub fn link_agent_skills(home: &Path, entry: &RegistryEntry) -> Result<Option<SkillLinkReport>> {
    let Some(link_dir) = entry.agent_skills_link_dir.as_deref() else {
        return Ok(None);
    };
    let home = home
        .canonicalize()
        .map_err(|source| skill_io_err("canonicalize home directory", home, source))?;
    let Some(install_root) = agent_skill_root(&home, entry)? else {
        return Ok(None);
    };
    // Resolve symlinked ancestors (e.g. a dotfiles-managed `~/.agents`) so linking works there
    // instead of failing the no-symlink-ancestor check that copy flows require.
    let install_root = resolve_existing_prefix(&install_root)?;
    if !install_root.is_dir() {
        return Ok(None);
    }
    let link_root = resolve_existing_prefix(&expand_agent_skills_install_dir(&home, link_dir)?)?;
    let mut report = SkillLinkReport {
        install_root: install_root.clone(),
        link_root: link_root.clone(),
        linked: Vec::new(),
        unchanged: Vec::new(),
        conflicts: Vec::new(),
        pruned: Vec::new(),
        errors: Vec::new(),
    };
    let mut candidates = Vec::new();
    collect_link_skill_directories(&install_root, &install_root, &mut candidates)?;
    if !candidates.is_empty() {
        ensure_directory_no_symlink_ancestors(&link_root, true)?;
    }
    for (skill_name, install_dir) in candidates {
        // One bad skill must not take down the rest of the refresh, and the prune below still runs.
        match link_one_skill(&link_root, &skill_name, &install_dir) {
            Ok(SkillLinkDisposition::Linked(entry)) => report.linked.push(entry),
            Ok(SkillLinkDisposition::Unchanged(entry)) => report.unchanged.push(entry),
            Ok(SkillLinkDisposition::Conflict(entry)) => report.conflicts.push(entry),
            Err(error) => {
                tracing::warn!(skill = %skill_name, error = %error, "skill link failed");
                report.errors.push(format!("{skill_name}: {error}"));
            }
        }
    }
    prune_dangling_skill_links(&link_root, &link_root, &install_root, &mut report.pruned)?;
    report
        .linked
        .sort_by(|left, right| left.name.cmp(&right.name));
    report
        .unchanged
        .sort_by(|left, right| left.name.cmp(&right.name));
    report
        .conflicts
        .sort_by(|left, right| left.name.cmp(&right.name));
    report
        .pruned
        .sort_by(|left, right| left.name.cmp(&right.name));
    report.errors.sort();
    Ok(Some(report))
}

enum SkillLinkDisposition {
    Linked(SkillInstallEntry),
    Unchanged(SkillInstallEntry),
    Conflict(SkillInstallEntry),
}

/// Link one skill: create the symlink, repoint a stale one, keep a correct one, or leave a real
/// file in place as a conflict.
fn link_one_skill(
    link_root: &Path,
    skill_name: &str,
    install_dir: &Path,
) -> Result<SkillLinkDisposition> {
    let link_path = link_root.join(skill_name);
    let link_parent = link_path
        .parent()
        .ok_or_else(|| StackError::SkillInstallFailed {
            reason: format!("skill link `{}` has no parent", link_path.display()),
        })?;
    ensure_directory_no_symlink_ancestors(link_parent, true)?;
    let link_entry = SkillInstallEntry {
        name: skill_name.to_owned(),
        path: link_path.clone(),
    };
    match std::fs::symlink_metadata(&link_path) {
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            create_skill_symlink(install_dir, &link_path)?;
            Ok(SkillLinkDisposition::Linked(link_entry))
        }
        Err(source) => Err(skill_io_err("stat skill link", &link_path, source)),
        Ok(metadata) if metadata.file_type().is_symlink() => {
            let target = std::fs::read_link(&link_path)
                .map_err(|source| skill_io_err("read skill link", &link_path, source))?;
            if target != install_dir {
                std::fs::remove_file(&link_path).map_err(|source| {
                    skill_io_err("remove stale skill link", &link_path, source)
                })?;
                create_skill_symlink(install_dir, &link_path)?;
                Ok(SkillLinkDisposition::Linked(link_entry))
            } else {
                Ok(SkillLinkDisposition::Unchanged(link_entry))
            }
        }
        Ok(_) => {
            tracing::warn!(
                path = %link_path.display(),
                "skill link path already holds a real file or directory; leaving it in place"
            );
            Ok(SkillLinkDisposition::Conflict(link_entry))
        }
    }
}

/// Best-effort wrapper for install/switch flows: a failed link refresh only degrades harness
/// discovery, so it is logged and returned rather than propagated.
pub fn link_agent_skills_best_effort(home: &Path, entry: &RegistryEntry) -> SkillLinkOutcome {
    match link_agent_skills(home, entry) {
        Ok(report) => SkillLinkOutcome {
            report,
            error: None,
        },
        Err(error) => {
            tracing::warn!(agent = %entry.id, error = %error, "skill link refresh failed");
            SkillLinkOutcome {
                report: None,
                error: Some(error.to_string()),
            }
        }
    }
}

/// Canonicalize the longest existing prefix of `path` and re-append the missing tail, so a
/// dotfiles-managed harness config directory does not fail the no-symlink-ancestor checks.
fn resolve_existing_prefix(path: &Path) -> Result<PathBuf> {
    let mut prefix = path.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        match prefix.canonicalize() {
            Ok(resolved) => {
                let mut result = resolved;
                for component in tail.iter().rev() {
                    result.push(component);
                }
                return Ok(result);
            }
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                match (prefix.file_name(), prefix.parent()) {
                    (Some(name), Some(parent)) if !parent.as_os_str().is_empty() => {
                        tail.push(name.to_owned());
                        prefix = parent.to_path_buf();
                    }
                    _ => return Ok(path.to_path_buf()),
                }
            }
            Err(source) => {
                return Err(skill_io_err("resolve skill link directory", path, source));
            }
        }
    }
}

/// Remove symlinks under the link root that point into the install root but whose target is gone.
/// Only links into the managed install root, and group directories this prune emptied, are touched;
/// real files, links pointing elsewhere, and non-empty directories are user-owned and left alone.
fn prune_dangling_skill_links(
    link_root: &Path,
    directory: &Path,
    install_root: &Path,
    pruned: &mut Vec<SkillInstallEntry>,
) -> Result<()> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(skill_io_err("read skill link directory", directory, source));
        }
    };
    let pruned_before = pruned.len();
    for entry in entries {
        let entry = entry
            .map_err(|source| skill_io_err("read skill link directory entry", directory, source))?;
        let entry_path = entry.path();
        let metadata = std::fs::symlink_metadata(&entry_path)
            .map_err(|source| skill_io_err("stat skill link entry", &entry_path, source))?;
        if metadata.file_type().is_symlink() {
            let target = std::fs::read_link(&entry_path)
                .map_err(|source| skill_io_err("read skill link", &entry_path, source))?;
            if !target.starts_with(install_root) {
                continue;
            }
            let dangling = match std::fs::symlink_metadata(&target) {
                Ok(_) => false,
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => true,
                // A target that cannot be stat'd for another reason may still exist; keep the link.
                Err(source) => {
                    tracing::warn!(
                        path = %entry_path.display(),
                        error = %source,
                        "keeping skill link: could not stat its target"
                    );
                    false
                }
            };
            if !dangling {
                continue;
            }
            std::fs::remove_file(&entry_path).map_err(|source| {
                skill_io_err("remove dangling skill link", &entry_path, source)
            })?;
            let name = entry_path
                .strip_prefix(link_root)
                .unwrap_or(&entry_path)
                .to_string_lossy()
                .into_owned();
            pruned.push(SkillInstallEntry {
                name,
                path: entry_path,
            });
        } else if metadata.is_dir() {
            prune_dangling_skill_links(link_root, &entry_path, install_root, pruned)?;
        }
    }
    if directory != link_root && pruned.len() > pruned_before {
        let mut remaining = std::fs::read_dir(directory)
            .map_err(|source| skill_io_err("re-read skill link directory", directory, source))?;
        if remaining.next().is_none() {
            std::fs::remove_dir(directory).map_err(|source| {
                skill_io_err("remove emptied skill link directory", directory, source)
            })?;
        }
    }
    Ok(())
}

fn create_skill_symlink(install_dir: &Path, link_path: &Path) -> Result<()> {
    std::os::unix::fs::symlink(install_dir, link_path).map_err(|source| {
        StackError::SkillInstallFailed {
            reason: format!(
                "create skill link `{}` -> `{}`: {source}",
                link_path.display(),
                install_dir.display()
            ),
        }
    })
}
