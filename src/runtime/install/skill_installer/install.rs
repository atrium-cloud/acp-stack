//! The install flow: selector parsing, archive fetch, and the copy of
//! resolved skill directories into an agent's shared install root.

use super::*;

pub fn parse_skill_names(values: &[String]) -> Result<Vec<String>> {
    let mut parsed = Vec::new();
    let mut seen = HashSet::new();
    for value in values {
        for raw in value.split(',') {
            let name = raw.trim();
            if name.is_empty() {
                return Err(StackError::SkillInstallInvalidName {
                    name: raw.to_owned(),
                });
            }
            validate_skill_selector(name)?;
            if !seen.insert(name.to_owned()) {
                return Err(StackError::SkillInstallFailed {
                    reason: format!("duplicate skill `{name}`"),
                });
            }
            parsed.push(name.to_owned());
        }
    }
    Ok(parsed)
}

pub fn install_from_github(
    source: &ResolvedSkillSource,
    destination_root: &Path,
    skill_names: &[String],
) -> Result<SkillInstallReport> {
    validate_requested_skills(source, skill_names)?;
    let (_tempdir, archive_root) = fetch_and_extract_source(source)?;
    install_from_extracted_root(source, &archive_root, destination_root, skill_names)
}

/// Download and extract a source's GitHub archive into a fresh temporary
/// directory, returning the tempdir guard (keep it alive) and the archive's
/// single top-level directory. Shared by install and inspection. Exposed so the
/// add route can fetch off the async runtime and *before* taking the agent
/// config-mutation lock, keeping a slow download from blocking `agent switch`.
pub fn fetch_and_extract_source(
    source: &ResolvedSkillSource,
) -> Result<(tempfile::TempDir, PathBuf)> {
    let tempdir = tempfile::tempdir().map_err(|source| StackError::SkillInstallFailed {
        reason: format!("create temporary skill directory: {source}"),
    })?;
    let archive_path = tempdir.path().join("skills.tar.gz");
    let extract_dir = tempdir.path().join("extract");
    let reference = source_archive_reference(source);
    let archive_url = format!("{}/archive/{reference}.tar.gz", source.url);
    let download_opts = DownloadOpts {
        max_bytes: GITHUB_ARCHIVE_MAX_BYTES,
        ..DownloadOpts::default()
    };
    download_to_file(&archive_url, &archive_path, &download_opts)?;
    let report = extract_archive(&archive_path, &extract_dir, &ExtractOpts::default())?;
    let archive_root = report
        .top_level_dir
        .as_deref()
        .map(|top| extract_dir.join(top))
        .ok_or_else(|| StackError::SkillInstallFailed {
            reason: format!(
                "GitHub archive for skill source `{}` did not contain a single top-level directory",
                source.id
            ),
        })?;
    Ok((tempdir, archive_root))
}

pub fn install_from_extracted_root(
    source: &ResolvedSkillSource,
    archive_root: &Path,
    destination_root: &Path,
    skill_names: &[String],
) -> Result<SkillInstallReport> {
    if source.descriptor != SKILL_DESCRIPTOR {
        return Err(StackError::SkillInstallFailed {
            reason: format!("skill source `{}` descriptor is not SKILL.md", source.id),
        });
    }
    let names = validate_requested_skills(source, skill_names)?;
    if names.is_empty() {
        return Ok(SkillInstallReport {
            source_id: source.id.clone(),
            destination_root: destination_root.to_path_buf(),
            installed: Vec::new(),
            skipped: Vec::new(),
        });
    }
    let mut resolved = Vec::with_capacity(names.len());
    for selector in names {
        let (name, source_dir) = find_skill_dir(source, archive_root, &selector)?;
        resolved.push((name, source_dir));
    }
    install_resolved_skill_dirs(&source.id, destination_root, resolved)
}

pub fn validate_requested_skills(
    source: &ResolvedSkillSource,
    skill_names: &[String],
) -> Result<Vec<String>> {
    let selectors = parse_skill_names(skill_names)?;
    let mut install_names = HashSet::<String>::new();
    for selector in &selectors {
        let name = if source.catalog_managed {
            source
                .indexed_skills
                .iter()
                .find(|skill| skill.selector == *selector)
                .map(|skill| skill.name.as_str())
                .ok_or_else(|| StackError::SkillInstallSkillMissing {
                    source_id: source.id.clone(),
                    skill: selector.clone(),
                })?
        } else {
            validate_skill_name(selector)?;
            selector
        };
        validate_install_target_name(name)?;
        if let Some(existing) = install_names
            .iter()
            .find(|existing| install_target_names_overlap(existing, name))
        {
            return Err(StackError::SkillInstallFailed {
                reason: format!(
                    "selected skills resolve to overlapping install paths `{existing}` and `{name}`"
                ),
            });
        }
        install_names.insert(name.to_owned());
    }
    Ok(selectors)
}

pub(crate) fn install_target_names_overlap(left: &str, right: &str) -> bool {
    left == right
        || right
            .strip_prefix(left)
            .is_some_and(|suffix| suffix.starts_with('/'))
        || left
            .strip_prefix(right)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn install_resolved_skill_dirs(
    source_id: &str,
    destination_root: &Path,
    resolved_skills: Vec<(String, PathBuf)>,
) -> Result<SkillInstallReport> {
    ensure_directory_no_symlink_ancestors(destination_root, true)?;
    let mut resolved = Vec::with_capacity(resolved_skills.len());
    for (name, source_dir) in resolved_skills {
        validate_install_target_name(&name)?;
        ensure_no_installed_skill_ancestor(destination_root, &name)?;
        let target_dir = destination_root.join(&name);
        let target_parent = target_dir
            .parent()
            .ok_or_else(|| StackError::SkillInstallFailed {
                reason: format!("skill target `{}` has no parent", target_dir.display()),
            })?;
        ensure_directory_no_symlink_ancestors(target_parent, true)?;
        match existing_target_state(&target_dir)? {
            ExistingTargetState::AlreadyInstalled => {
                resolved.push(ResolvedInstall {
                    name,
                    source_dir,
                    target_dir,
                    action: InstallAction::Skip,
                });
            }
            ExistingTargetState::Missing => {
                resolved.push(ResolvedInstall {
                    name,
                    source_dir,
                    target_dir,
                    action: InstallAction::Copy,
                });
            }
        }
    }
    let mut installed = Vec::new();
    let mut skipped = Vec::new();
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for install in resolved {
            match install.action {
                InstallAction::Skip => skipped.push(SkillInstallEntry {
                    name: install.name,
                    path: install.target_dir,
                }),
                InstallAction::Copy => {
                    handles.push(scope.spawn(move || {
                        copy_skill_dir_atomically(
                            &install.source_dir,
                            &install.target_dir,
                            &install.name,
                            Some(source_id),
                        )
                        .map(|()| SkillInstallEntry {
                            name: install.name,
                            path: install.target_dir,
                        })
                    }));
                }
            }
        }
        for handle in handles {
            let entry = handle.join().map_err(|_| StackError::SkillInstallFailed {
                reason: "skill install worker panicked".to_owned(),
            })??;
            installed.push(entry);
        }
        Ok::<(), StackError>(())
    })?;

    installed.sort_by(|left, right| left.name.cmp(&right.name));
    skipped.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(SkillInstallReport {
        source_id: source_id.to_owned(),
        destination_root: destination_root.to_path_buf(),
        installed,
        skipped,
    })
}

pub fn all_skills_installed(
    source: &ResolvedSkillSource,
    destination_root: &Path,
    skill_names: &[String],
) -> bool {
    if ensure_directory_no_symlink_ancestors(destination_root, false).is_err() {
        return false;
    }
    parse_skill_names(skill_names).is_ok_and(|names| {
        names.iter().all(|selector| {
            let Some(name) = install_name_for_selector(source, selector) else {
                return false;
            };
            matches!(
                existing_target_state(&destination_root.join(name)),
                Ok(ExistingTargetState::AlreadyInstalled)
            )
        })
    })
}

#[derive(Debug)]
struct ResolvedInstall {
    name: String,
    source_dir: PathBuf,
    target_dir: PathBuf,
    action: InstallAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallAction {
    Copy,
    Skip,
}
