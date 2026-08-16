//! Skill discovery: enumerating installable skills in an extracted archive,
//! locating one skill directory by selector, walking an existing install root,
//! and reading `SKILL.md` frontmatter.

use super::*;

/// Download a source and list its installable skills with the `name` and
/// `description` read from each `SKILL.md`. Blocking (network + extract); call
/// off the async runtime. Backs `source get`.
pub fn inspect_source(source: &ResolvedSkillSource) -> Result<Vec<SkillMetadata>> {
    let (_tempdir, archive_root) = fetch_and_extract_source(source)?;
    discover_source_skills(source, &archive_root)
}

/// Enumerate installable skills from an already-extracted archive. For a
/// catalog source this walks the embedded index (exact paths); for a
/// user/ad-hoc source it discovers `SKILL.md` directories flat under each
/// installable directory (the `skills/` convention). Split out from
/// [`inspect_source`] so it is testable without network access.
pub fn discover_source_skills(
    source: &ResolvedSkillSource,
    archive_root: &Path,
) -> Result<Vec<SkillMetadata>> {
    let mut skills = Vec::new();
    if source.catalog_managed {
        for skill in &source.indexed_skills {
            validate_registry_relative_path(&skill.path)?;
            let candidate = archive_root.join(&skill.path);
            // `add` (via `find_skill_dir`) requires a non-symlink skill dir
            // with a non-symlink regular SKILL.md whose frontmatter parses and
            // whose name matches the index, so a catalog skill failing any of
            // those checks must not be listed here — otherwise `get` would
            // offer a skill that `add` cannot install.
            if let Err(error) = validate_skill_candidate(&candidate, &skill.selector) {
                tracing::warn!(
                    skill = %skill.selector,
                    %error,
                    "skipping catalog skill with an unsafe source path"
                );
                continue;
            }
            match read_skill_descriptor(&candidate.join(SKILL_DESCRIPTOR)) {
                Ok(descriptor) if descriptor.name == skill.name => {
                    skills.push(SkillMetadata {
                        selector: skill.selector.clone(),
                        name: skill.name.clone(),
                        description: descriptor.description,
                        path: skill.path.clone(),
                    });
                }
                Ok(descriptor) => {
                    tracing::warn!(
                        skill = %skill.selector,
                        index_name = %skill.name,
                        descriptor_name = %descriptor.name,
                        "skipping catalog skill whose descriptor name disagrees with the index"
                    );
                }
                Err(error) => {
                    tracing::warn!(
                        skill = %skill.selector,
                        %error,
                        "skipping catalog skill with an unreadable descriptor"
                    );
                }
            }
        }
    } else {
        for directory in source
            .directories
            .iter()
            .filter(|directory| directory.installable)
        {
            validate_registry_relative_path(&directory.path)?;
            let base = archive_root.join(&directory.path);
            let entries = match std::fs::read_dir(&base) {
                Ok(entries) => entries,
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => continue,
                Err(source) => {
                    return Err(skill_io_err("read source directory", &base, source));
                }
            };
            for entry in entries {
                let entry = entry
                    .map_err(|source| skill_io_err("read source directory entry", &base, source))?;
                let leaf = entry.file_name().to_string_lossy().into_owned();
                // Only surface entries that are directly installable by selector.
                if validate_skill_name(&leaf).is_err() {
                    continue;
                }
                let descriptor = entry.path().join(SKILL_DESCRIPTOR);
                let Ok(metadata) = std::fs::symlink_metadata(&descriptor) else {
                    continue;
                };
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    continue;
                }
                // `add` (via `find_skill_dir`) only requires the descriptor to be
                // a regular file, not valid frontmatter, so a sibling with a
                // malformed `SKILL.md` must still appear here (degraded to the
                // leaf name) rather than failing the whole listing — otherwise
                // `get` would omit a skill that `add` would happily install.
                let descriptor = read_skill_descriptor(&descriptor).ok();
                skills.push(SkillMetadata {
                    selector: leaf.clone(),
                    name: descriptor
                        .as_ref()
                        .map(|descriptor| descriptor.name.clone())
                        .unwrap_or_else(|| leaf.clone()),
                    description: descriptor.and_then(|descriptor| descriptor.description),
                    path: format!("{}/{leaf}", directory.path.trim_end_matches('/')),
                });
            }
        }
    }
    skills.sort_by(|left, right| left.selector.cmp(&right.selector));
    Ok(skills)
}

pub(super) fn source_archive_reference(source: &ResolvedSkillSource) -> &str {
    source
        .verified_commit
        .as_deref()
        .or(source.indexed_commit.as_deref())
        .unwrap_or(source.branch.as_str())
}

/// How a skill-tree walk treats entries it did not expect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CollectPolicy {
    /// Port: every unexpected entry is an error, because the walk feeds a copy
    /// and copying a symlink or special file would be unsafe.
    Port,
    /// Link: nothing is copied — only the skill dir is symlinked — so
    /// unexpected entries are skipped with a warning and only a failure to
    /// read the root itself propagates.
    Link,
}

fn collect_skill_directories(
    policy: CollectPolicy,
    root: &Path,
    directory: &Path,
    candidates: &mut Vec<(String, PathBuf)>,
) -> Result<()> {
    let strict = policy == CollectPolicy::Port;
    let at_root = directory == root;
    let descriptor = directory.join(SKILL_DESCRIPTOR);
    match std::fs::symlink_metadata(&descriptor) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                if strict {
                    return Err(StackError::SkillInstallFailed {
                        reason: format!(
                            "skill descriptor `{}` must be a regular SKILL.md file",
                            descriptor.display()
                        ),
                    });
                }
                tracing::warn!(
                    path = %descriptor.display(),
                    "skipping skill: descriptor is not a regular SKILL.md file"
                );
                return Ok(());
            }
            let relative = directory
                .strip_prefix(root)
                .map_err(|source| skill_io_err("resolve source skill path", directory, source))?;
            let skill_name = relative
                .components()
                .map(|component| match component {
                    Component::Normal(value) => value.to_str().map(str::to_owned),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()
                .map(|components| components.join("/"))
                .filter(|name| !name.is_empty())
                .ok_or_else(|| StackError::SkillInstallFailed {
                    reason: format!(
                        "skill descriptor `{}` does not map to a portable skill directory",
                        descriptor.display()
                    ),
                })?;
            if let Err(error) = validate_install_target_name(&skill_name) {
                if strict {
                    return Err(error);
                }
                tracing::warn!(
                    path = %directory.display(),
                    error = %error,
                    "skipping skill: directory does not map to a valid link name"
                );
                return Ok(());
            }
            if strict {
                validate_skill_dir_for_port(directory)?;
            }
            candidates.push((skill_name, directory.to_path_buf()));
            return Ok(());
        }
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            if strict || at_root {
                return Err(skill_io_err("stat skill descriptor", &descriptor, source));
            }
            tracing::warn!(
                path = %descriptor.display(),
                error = %source,
                "skipping skill: could not stat descriptor"
            );
            return Ok(());
        }
    }

    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(source) => {
            if strict || at_root {
                return Err(skill_io_err(
                    "read source skills directory",
                    directory,
                    source,
                ));
            }
            tracing::warn!(
                path = %directory.display(),
                error = %source,
                "skipping unreadable skills directory"
            );
            return Ok(());
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(source) => {
                if strict {
                    return Err(skill_io_err(
                        "read source skills directory entry",
                        directory,
                        source,
                    ));
                }
                tracing::warn!(
                    path = %directory.display(),
                    error = %source,
                    "skipping unreadable skills directory entry"
                );
                continue;
            }
        };
        let entry_path = entry.path();
        let metadata = match std::fs::symlink_metadata(&entry_path) {
            Ok(metadata) => metadata,
            Err(source) => {
                if strict {
                    return Err(skill_io_err("stat source skill entry", &entry_path, source));
                }
                tracing::warn!(
                    path = %entry_path.display(),
                    error = %source,
                    "skipping skills directory entry: could not stat"
                );
                continue;
            }
        };
        if metadata.file_type().is_symlink() {
            if strict {
                return Err(StackError::SkillInstallFailed {
                    reason: format!("refusing to port symlink `{}`", entry_path.display()),
                });
            }
            tracing::warn!(
                path = %entry_path.display(),
                "skipping symlinked entry in skills directory"
            );
            continue;
        }
        if metadata.is_dir() {
            collect_skill_directories(policy, root, &entry_path, candidates)?;
        } else if !metadata.is_file() {
            if strict {
                return Err(StackError::SkillInstallFailed {
                    reason: format!("refusing to port special file `{}`", entry_path.display()),
                });
            }
            tracing::warn!(
                path = %entry_path.display(),
                "skipping special file in skills directory"
            );
        }
    }
    Ok(())
}

pub(super) fn collect_port_skill_directories(
    source_root: &Path,
    directory: &Path,
    candidates: &mut Vec<(String, PathBuf)>,
) -> Result<()> {
    collect_skill_directories(CollectPolicy::Port, source_root, directory, candidates)
}

pub(super) fn collect_link_skill_directories(
    install_root: &Path,
    directory: &Path,
    candidates: &mut Vec<(String, PathBuf)>,
) -> Result<()> {
    collect_skill_directories(CollectPolicy::Link, install_root, directory, candidates)
}

pub(super) fn find_skill_dir(
    source: &ResolvedSkillSource,
    archive_root: &Path,
    selector: &str,
) -> Result<(String, PathBuf)> {
    if source.catalog_managed {
        let skill = source
            .indexed_skills
            .iter()
            .find(|skill| skill.selector == selector)
            .ok_or_else(|| StackError::SkillInstallSkillMissing {
                source_id: source.id.clone(),
                skill: selector.to_owned(),
            })?;
        validate_registry_relative_path(&skill.path)?;
        let candidate = archive_root.join(&skill.path);
        validate_skill_candidate(&candidate, selector)?;
        let descriptor_name = skill_descriptor_name(&candidate.join(SKILL_DESCRIPTOR))?;
        if descriptor_name != skill.name {
            return Err(StackError::SkillInstallFailed {
                reason: format!(
                    "skill selector `{selector}` expected frontmatter name `{}` but archive declared `{descriptor_name}`",
                    skill.name
                ),
            });
        }
        return Ok((skill.name.clone(), candidate));
    }

    validate_skill_name(selector)?;
    for directory in source
        .directories
        .iter()
        .filter(|directory| directory.installable)
    {
        validate_registry_relative_path(&directory.path)?;
        let base = archive_root.join(&directory.path);
        let candidate = base.join(selector);
        if !candidate.exists() {
            continue;
        }
        validate_skill_candidate(&candidate, selector)?;
        return Ok((selector.to_owned(), candidate));
    }
    Err(StackError::SkillInstallSkillMissing {
        source_id: source.id.clone(),
        skill: selector.to_owned(),
    })
}

fn validate_skill_candidate(candidate: &Path, selector: &str) -> Result<()> {
    let metadata = std::fs::symlink_metadata(candidate)
        .map_err(|source| skill_io_err("stat skill directory", candidate, source))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(StackError::SkillInstallFailed {
            reason: format!("skill `{selector}` source path is not a regular directory"),
        });
    }
    let descriptor = candidate.join(SKILL_DESCRIPTOR);
    let descriptor_metadata = std::fs::symlink_metadata(&descriptor)
        .map_err(|source| skill_io_err("stat skill descriptor", &descriptor, source))?;
    if descriptor_metadata.file_type().is_symlink() || !descriptor_metadata.is_file() {
        return Err(StackError::SkillInstallFailed {
            reason: format!("skill `{selector}` descriptor must be a regular SKILL.md file"),
        });
    }
    Ok(())
}

pub(super) fn install_name_for_selector<'a>(
    source: &'a ResolvedSkillSource,
    selector: &'a str,
) -> Option<&'a str> {
    if source.catalog_managed {
        source
            .indexed_skills
            .iter()
            .find(|skill| skill.selector == selector)
            .map(|skill| skill.name.as_str())
    } else if validate_skill_name(selector).is_ok() {
        Some(selector)
    } else {
        None
    }
}

fn skill_descriptor_name(descriptor: &Path) -> Result<String> {
    Ok(read_skill_descriptor(descriptor)?.name)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SkillDescriptor {
    name: String,
    description: Option<String>,
}

fn read_skill_descriptor(descriptor: &Path) -> Result<SkillDescriptor> {
    #[derive(Deserialize)]
    struct Frontmatter {
        name: String,
        #[serde(default)]
        description: Option<String>,
    }

    let body = std::fs::read_to_string(descriptor)
        .map_err(|source| skill_io_err("read skill descriptor", descriptor, source))?;
    let mut lines = body.lines();
    if lines.next() != Some("---") {
        return Err(StackError::SkillInstallFailed {
            reason: format!(
                "skill descriptor `{}` is missing YAML frontmatter",
                descriptor.display()
            ),
        });
    }
    let mut yaml = String::new();
    let mut closed = false;
    for line in lines {
        if line == "---" {
            closed = true;
            break;
        }
        yaml.push_str(line);
        yaml.push('\n');
    }
    if !closed {
        return Err(StackError::SkillInstallFailed {
            reason: format!(
                "skill descriptor `{}` has unterminated YAML frontmatter",
                descriptor.display()
            ),
        });
    }
    let frontmatter: Frontmatter = serde_norway::from_str(&yaml)
        .map_err(|source| skill_io_err("parse skill descriptor frontmatter", descriptor, source))?;
    Ok(SkillDescriptor {
        name: frontmatter.name,
        description: frontmatter
            .description
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty()),
    })
}
