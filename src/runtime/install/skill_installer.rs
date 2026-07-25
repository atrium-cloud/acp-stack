//! Agent Skills installer used by `acps init`.

mod fs;
mod validate;

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Result, StackError};
use crate::fs_util::{create_dir_owner_only, set_owner_only_dir, set_owner_only_file};
use crate::runtime::install::agent_registry::{RegistryCatalog, RegistryEntry};
use crate::runtime::install::skill_registry::{
    CatalogSkill, SkillCatalog, SkillDirectory, SkillSource,
};
use crate::runtime::workspace_sources::safe_download::{DownloadOpts, download_to_file};
use crate::runtime::workspace_sources::safe_extract::{ExtractOpts, extract_archive};

use self::fs::*;
use self::validate::{
    validate_github_owner, validate_install_target_name, validate_registry_relative_path,
    validate_skill_name, validate_skill_selector,
};

pub const SOURCE_CUSTOM_GITHUB_PREFIX: &str = "github:";
const CUSTOM_SKILLS_REPO: &str = "skills";
const CUSTOM_SKILLS_BRANCH: &str = "main";
const CUSTOM_SKILLS_DIRECTORY: &str = "skills";
const SKILL_DESCRIPTOR: &str = "SKILL.md";
const GITHUB_ARCHIVE_MAX_BYTES: u64 = 200 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillSourceSelection {
    Official { id: String },
    CustomGithubOwner { owner: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSkillSource {
    pub id: String,
    pub name: String,
    pub owner: String,
    pub repo: String,
    pub url: String,
    pub branch: String,
    pub verified_commit: Option<String>,
    pub indexed_commit: Option<String>,
    pub descriptor: String,
    pub catalog_managed: bool,
    pub directories: Vec<ResolvedSkillDirectory>,
    pub indexed_skills: Vec<CatalogSkill>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSkillDirectory {
    pub path: String,
    pub installable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SkillInstallReport {
    pub source_id: String,
    pub destination_root: PathBuf,
    pub installed: Vec<SkillInstallEntry>,
    pub skipped: Vec<SkillInstallEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SkillInstallEntry {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkillPortReport {
    pub source_root: PathBuf,
    pub target_root: PathBuf,
    pub status: SkillPortStatus,
    pub copied: Vec<SkillInstallEntry>,
    pub overwritten: Vec<SkillInstallEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillPortStatus {
    Shared,
    Copied,
    NoneFound,
}

pub fn parse_skill_source(value: &str, catalog: &SkillCatalog) -> Result<SkillSourceSelection> {
    let trimmed = value.trim();
    if let Some(source) = catalog.lookup_alias(trimmed) {
        return Ok(SkillSourceSelection::Official {
            id: source.id.clone(),
        });
    }
    let Some(owner) = trimmed.strip_prefix(SOURCE_CUSTOM_GITHUB_PREFIX) else {
        return Err(StackError::SkillInstallInvalidSource {
            source_id: trimmed.to_owned(),
        });
    };
    validate_github_owner(owner)?;
    Ok(SkillSourceSelection::CustomGithubOwner {
        owner: owner.to_owned(),
    })
}

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

pub fn resolve_source(
    selection: &SkillSourceSelection,
    catalog: &SkillCatalog,
) -> Result<ResolvedSkillSource> {
    match selection {
        SkillSourceSelection::Official { id } => {
            let source =
                catalog
                    .lookup(id)
                    .ok_or_else(|| StackError::SkillInstallSourceMissing {
                        source_id: id.clone(),
                    })?;
            Ok(resolve_official_source(source))
        }
        SkillSourceSelection::CustomGithubOwner { owner } => {
            validate_github_owner(owner)?;
            Ok(ResolvedSkillSource {
                id: format!("{owner}-skills"),
                name: format!("{owner} Agent Skills"),
                owner: owner.clone(),
                repo: CUSTOM_SKILLS_REPO.to_owned(),
                url: format!("https://github.com/{owner}/{CUSTOM_SKILLS_REPO}"),
                branch: CUSTOM_SKILLS_BRANCH.to_owned(),
                verified_commit: None,
                indexed_commit: None,
                descriptor: SKILL_DESCRIPTOR.to_owned(),
                catalog_managed: false,
                directories: vec![ResolvedSkillDirectory {
                    path: CUSTOM_SKILLS_DIRECTORY.to_owned(),
                    installable: true,
                }],
                indexed_skills: Vec::new(),
            })
        }
    }
}

pub fn expand_agent_skills_install_dir(home: &Path, value: &str) -> Result<PathBuf> {
    if value == "~" {
        return Ok(home.to_path_buf());
    }
    if let Some(rest) = value.strip_prefix("~/") {
        return Ok(home.join(rest));
    }
    let path = Path::new(value);
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Err(StackError::SkillInstallFailed {
        reason: format!("agent skill install dir `{value}` must be absolute or start with `~/`"),
    })
}

pub fn install_from_github(
    source: &ResolvedSkillSource,
    destination_root: &Path,
    skill_names: &[String],
) -> Result<SkillInstallReport> {
    validate_requested_skills(source, skill_names)?;
    let tempdir = tempfile::tempdir().map_err(|source| StackError::SkillInstallFailed {
        reason: format!("create temporary skill install directory: {source}"),
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
    install_from_extracted_root(source, &archive_root, destination_root, skill_names)
}

fn source_archive_reference(source: &ResolvedSkillSource) -> &str {
    source
        .verified_commit
        .as_deref()
        .or(source.indexed_commit.as_deref())
        .unwrap_or(source.branch.as_str())
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

fn validate_requested_skills(
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

pub fn port_agent_skills(
    home: &Path,
    registry: &RegistryCatalog,
    old_agent_id: &str,
    target_agent_id: &str,
) -> Result<Option<SkillPortReport>> {
    let home = home
        .canonicalize()
        .map_err(|source| StackError::SkillInstallFailed {
            reason: format!("canonicalize home directory `{}`: {source}", home.display()),
        })?;
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

fn agent_skill_root(home: &Path, entry: &RegistryEntry) -> Result<Option<PathBuf>> {
    if !entry.supports_agent_skills {
        return Ok(None);
    }
    let Some(install_dir) = entry.agent_skills_install_dir.as_deref() else {
        return Ok(None);
    };
    expand_agent_skills_install_dir(home, install_dir).map(Some)
}

fn port_skill_directories(source_root: &Path, target_root: &Path) -> Result<SkillPortReport> {
    if source_root == target_root {
        return Ok(SkillPortReport {
            source_root: source_root.to_path_buf(),
            target_root: target_root.to_path_buf(),
            status: SkillPortStatus::Shared,
            copied: Vec::new(),
            overwritten: Vec::new(),
        });
    }
    if !source_root_exists_without_symlink_ancestors(source_root)? {
        return Ok(SkillPortReport {
            source_root: source_root.to_path_buf(),
            target_root: target_root.to_path_buf(),
            status: SkillPortStatus::NoneFound,
            copied: Vec::new(),
            overwritten: Vec::new(),
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
        });
    }

    ensure_directory_no_symlink_ancestors(target_root, true)?;
    let mut installs = Vec::with_capacity(candidates.len());
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
            ExistingTargetState::AlreadyInstalled => PortAction::Overwrite,
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
                copy_skill_dir_atomically(&install.source_dir, &install.target_dir, &install.name)?;
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
    Ok(SkillPortReport {
        source_root: source_root.to_path_buf(),
        target_root: target_root.to_path_buf(),
        status: SkillPortStatus::Copied,
        copied,
        overwritten,
    })
}

fn collect_port_skill_directories(
    source_root: &Path,
    directory: &Path,
    candidates: &mut Vec<(String, PathBuf)>,
) -> Result<()> {
    let descriptor = directory.join(SKILL_DESCRIPTOR);
    match std::fs::symlink_metadata(&descriptor) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(StackError::SkillInstallFailed {
                    reason: format!(
                        "skill descriptor `{}` must be a regular SKILL.md file",
                        descriptor.display()
                    ),
                });
            }
            let relative = directory.strip_prefix(source_root).map_err(|source| {
                StackError::SkillInstallFailed {
                    reason: format!(
                        "resolve source skill path `{}`: {source}",
                        directory.display()
                    ),
                }
            })?;
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
            validate_install_target_name(&skill_name)?;
            validate_skill_dir_for_port(directory)?;
            candidates.push((skill_name, directory.to_path_buf()));
            return Ok(());
        }
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(StackError::SkillInstallFailed {
                reason: format!("stat skill descriptor `{}`: {source}", descriptor.display()),
            });
        }
    }

    for entry in std::fs::read_dir(directory).map_err(|source| StackError::SkillInstallFailed {
        reason: format!(
            "read source skills directory `{}`: {source}",
            directory.display()
        ),
    })? {
        let entry = entry.map_err(|source| StackError::SkillInstallFailed {
            reason: format!(
                "read source skills directory entry `{}`: {source}",
                directory.display()
            ),
        })?;
        let entry_path = entry.path();
        let metadata = std::fs::symlink_metadata(&entry_path).map_err(|source| {
            StackError::SkillInstallFailed {
                reason: format!(
                    "stat source skill entry `{}`: {source}",
                    entry_path.display()
                ),
            }
        })?;
        if metadata.file_type().is_symlink() {
            return Err(StackError::SkillInstallFailed {
                reason: format!("refusing to port symlink `{}`", entry_path.display()),
            });
        }
        if metadata.is_dir() {
            collect_port_skill_directories(source_root, &entry_path, candidates)?;
        } else if !metadata.is_file() {
            return Err(StackError::SkillInstallFailed {
                reason: format!("refusing to port special file `{}`", entry_path.display()),
            });
        }
    }
    Ok(())
}

fn resolve_official_source(source: &SkillSource) -> ResolvedSkillSource {
    ResolvedSkillSource {
        id: source.id.clone(),
        name: source.name.clone(),
        owner: source.owner.clone(),
        repo: source.repo.clone(),
        url: source.url.clone(),
        branch: source.branch.clone(),
        verified_commit: source.verified_commit.clone(),
        indexed_commit: source.indexed_commit.clone(),
        descriptor: source.descriptor.clone(),
        catalog_managed: true,
        directories: source.directories.iter().map(resolve_directory).collect(),
        indexed_skills: source.indexed_skills.clone(),
    }
}

fn resolve_directory(directory: &SkillDirectory) -> ResolvedSkillDirectory {
    ResolvedSkillDirectory {
        path: directory.path.clone(),
        installable: directory.installable,
    }
}

fn find_skill_dir(
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
    let metadata =
        std::fs::symlink_metadata(candidate).map_err(|source| StackError::SkillInstallFailed {
            reason: format!("stat skill directory `{}`: {source}", candidate.display()),
        })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(StackError::SkillInstallFailed {
            reason: format!("skill `{selector}` source path is not a regular directory"),
        });
    }
    let descriptor = candidate.join(SKILL_DESCRIPTOR);
    let descriptor_metadata = std::fs::symlink_metadata(&descriptor).map_err(|source| {
        StackError::SkillInstallFailed {
            reason: format!("stat skill descriptor `{}`: {source}", descriptor.display()),
        }
    })?;
    if descriptor_metadata.file_type().is_symlink() || !descriptor_metadata.is_file() {
        return Err(StackError::SkillInstallFailed {
            reason: format!("skill `{selector}` descriptor must be a regular SKILL.md file"),
        });
    }
    Ok(())
}

fn install_name_for_selector<'a>(
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
    #[derive(Deserialize)]
    struct Frontmatter {
        name: String,
    }

    let body =
        std::fs::read_to_string(descriptor).map_err(|source| StackError::SkillInstallFailed {
            reason: format!("read skill descriptor `{}`: {source}", descriptor.display()),
        })?;
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
    let frontmatter: Frontmatter =
        serde_norway::from_str(&yaml).map_err(|source| StackError::SkillInstallFailed {
            reason: format!(
                "parse skill descriptor frontmatter `{}`: {source}",
                descriptor.display()
            ),
        })?;
    Ok(frontmatter.name)
}

#[derive(Debug)]
struct ResolvedInstall {
    name: String,
    source_dir: PathBuf,
    target_dir: PathBuf,
    action: InstallAction,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallAction {
    Copy,
    Skip,
}

#[cfg(test)]
mod tests;
