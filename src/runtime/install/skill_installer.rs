//! Agent Skills installer used by `acps init`.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Result, StackError};
use crate::fs_util::{create_dir_owner_only, set_owner_only_dir, set_owner_only_file};
use crate::runtime::install::agent_registry::{RegistryCatalog, RegistryEntry};
use crate::runtime::install::skill_registry::{
    PluginBundleDirectory, SkillCatalog, SkillDirectory, SkillSource,
};
use crate::runtime::workspace_sources::safe_download::{DownloadOpts, download_to_file};
use crate::runtime::workspace_sources::safe_extract::{ExtractOpts, extract_archive};

pub const SOURCE_OPENAI: &str = "openai";
pub const SOURCE_ANTHROPIC: &str = "anthropic";
pub const SOURCE_CUSTOM_GITHUB_PREFIX: &str = "github:";
pub const OPENAI_SKILLS_SOURCE_ID: &str = "openai-skills";
pub const ANTHROPIC_SKILLS_SOURCE_ID: &str = "anthropic-skills";
pub const OPENAI_PLUGINS_SOURCE_ID: &str = "openai-plugins";
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
pub enum PluginSourceSelection {
    Official { id: String },
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
    pub directories: Vec<ResolvedSkillDirectory>,
    pub plugin_bundles: Vec<ResolvedPluginBundleDirectory>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSkillDirectory {
    pub path: String,
    pub installable: bool,
    pub indexed_names: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPluginBundleDirectory {
    pub path: String,
    pub installable_plugins: Vec<String>,
    pub excluded_plugins: Vec<String>,
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

pub fn parse_skill_source(value: &str) -> Result<SkillSourceSelection> {
    let trimmed = value.trim();
    match trimmed {
        SOURCE_OPENAI => Ok(SkillSourceSelection::Official {
            id: OPENAI_SKILLS_SOURCE_ID.to_owned(),
        }),
        SOURCE_ANTHROPIC => Ok(SkillSourceSelection::Official {
            id: ANTHROPIC_SKILLS_SOURCE_ID.to_owned(),
        }),
        _ => {
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
    }
}

pub fn parse_plugin_source(value: &str) -> Result<PluginSourceSelection> {
    let trimmed = value.trim();
    match trimmed {
        SOURCE_OPENAI => Ok(PluginSourceSelection::Official {
            id: OPENAI_PLUGINS_SOURCE_ID.to_owned(),
        }),
        _ => Err(StackError::PluginInstallInvalidSource {
            source_id: trimmed.to_owned(),
        }),
    }
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
            validate_skill_name(name)?;
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

pub fn parse_plugin_names(values: &[String]) -> Result<Vec<String>> {
    let mut parsed = Vec::new();
    let mut seen = HashSet::new();
    for value in values {
        for raw in value.split(',') {
            let name = raw.trim();
            if name.is_empty() {
                return Err(StackError::PluginInstallInvalidName {
                    name: raw.to_owned(),
                });
            }
            validate_plugin_name(name)?;
            if !seen.insert(name.to_owned()) {
                return Err(StackError::PluginInstallFailed {
                    reason: format!("duplicate plugin `{name}`"),
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
                directories: vec![ResolvedSkillDirectory {
                    path: CUSTOM_SKILLS_DIRECTORY.to_owned(),
                    installable: true,
                    indexed_names: Vec::new(),
                }],
                plugin_bundles: Vec::new(),
            })
        }
    }
}

pub fn resolve_plugin_source(
    selection: &PluginSourceSelection,
    catalog: &SkillCatalog,
) -> Result<ResolvedSkillSource> {
    match selection {
        PluginSourceSelection::Official { id } => {
            let source =
                catalog
                    .lookup(id)
                    .ok_or_else(|| StackError::PluginInstallSourceMissing {
                        source_id: id.clone(),
                    })?;
            let resolved = resolve_official_source(source);
            if resolved.plugin_bundles.is_empty() {
                return Err(StackError::PluginInstallSourceMissing {
                    source_id: id.clone(),
                });
            }
            Ok(resolved)
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

pub fn install_plugins_from_github(
    source: &ResolvedSkillSource,
    destination_root: &Path,
    plugin_names: &[String],
) -> Result<SkillInstallReport> {
    let names = parse_plugin_names(plugin_names)?;
    validate_requested_plugins(source, &names)?;

    let tempdir = tempfile::tempdir().map_err(|source| StackError::PluginInstallFailed {
        reason: format!("create temporary plugin install directory: {source}"),
    })?;
    let archive_path = tempdir.path().join("plugins.tar.gz");
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
        .ok_or_else(|| StackError::PluginInstallFailed {
            reason: format!(
                "GitHub archive for plugin source `{}` did not contain a single top-level directory",
                source.id
            ),
        })?;
    install_plugins_from_extracted_root(source, &archive_root, destination_root, &names)
}

fn source_archive_reference(source: &ResolvedSkillSource) -> &str {
    source
        .verified_commit
        .as_deref()
        .or(source.indexed_commit.as_deref())
        .unwrap_or(source.branch.as_str())
}

pub fn install_plugins_from_extracted_root(
    source: &ResolvedSkillSource,
    archive_root: &Path,
    destination_root: &Path,
    plugin_names: &[String],
) -> Result<SkillInstallReport> {
    if source.descriptor != SKILL_DESCRIPTOR {
        return Err(StackError::PluginInstallFailed {
            reason: format!("plugin source `{}` descriptor is not SKILL.md", source.id),
        });
    }
    let names = parse_plugin_names(plugin_names)?;
    if names.is_empty() {
        return Ok(SkillInstallReport {
            source_id: source.id.clone(),
            destination_root: destination_root.to_path_buf(),
            installed: Vec::new(),
            skipped: Vec::new(),
        });
    }
    validate_requested_plugins(source, &names)?;

    let mut resolved_skills = Vec::new();
    let mut seen_skill_names = HashSet::new();
    for plugin_name in names {
        let skill_dirs = find_plugin_skill_dirs(source, archive_root, &plugin_name)?;
        if skill_dirs.is_empty() {
            return Err(StackError::PluginInstallNoSkills {
                source_id: source.id.clone(),
                plugin: plugin_name,
            });
        }
        for (skill_name, source_dir) in skill_dirs {
            if !seen_skill_names.insert(skill_name.clone()) {
                return Err(StackError::PluginInstallFailed {
                    reason: format!(
                        "duplicate skill `{skill_name}` expanded from selected plugins"
                    ),
                });
            }
            resolved_skills.push((skill_name, source_dir));
        }
    }

    install_resolved_skill_dirs(&source.id, destination_root, resolved_skills)
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
    let names = parse_skill_names(skill_names)?;
    if names.is_empty() {
        return Ok(SkillInstallReport {
            source_id: source.id.clone(),
            destination_root: destination_root.to_path_buf(),
            installed: Vec::new(),
            skipped: Vec::new(),
        });
    }
    let mut resolved = Vec::with_capacity(names.len());
    for name in names {
        let source_dir = find_skill_dir(source, archive_root, &name)?;
        resolved.push((name, source_dir));
    }
    install_resolved_skill_dirs(&source.id, destination_root, resolved)
}

fn install_resolved_skill_dirs(
    source_id: &str,
    destination_root: &Path,
    resolved_skills: Vec<(String, PathBuf)>,
) -> Result<SkillInstallReport> {
    ensure_directory_no_symlink_ancestors(destination_root, true)?;
    let mut resolved = Vec::with_capacity(resolved_skills.len());
    for (name, source_dir) in resolved_skills {
        let target_dir = destination_root.join(&name);
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

pub fn all_skills_installed(destination_root: &Path, skill_names: &[String]) -> bool {
    if ensure_directory_no_symlink_ancestors(destination_root, false).is_err() {
        return false;
    }
    parse_skill_names(skill_names).is_ok_and(|names| {
        names.iter().all(|name| {
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
    for entry in
        std::fs::read_dir(source_root).map_err(|source| StackError::SkillInstallFailed {
            reason: format!(
                "read source skills directory `{}`: {source}",
                source_root.display()
            ),
        })?
    {
        let entry = entry.map_err(|source| StackError::SkillInstallFailed {
            reason: format!(
                "read source skills directory entry `{}`: {source}",
                source_root.display()
            ),
        })?;
        let entry_path = entry.path();
        let entry_metadata = std::fs::symlink_metadata(&entry_path).map_err(|source| {
            StackError::SkillInstallFailed {
                reason: format!(
                    "stat source skill entry `{}`: {source}",
                    entry_path.display()
                ),
            }
        })?;
        if entry_metadata.file_type().is_symlink() {
            return Err(StackError::SkillInstallFailed {
                reason: format!("refusing to port symlink `{}`", entry_path.display()),
            });
        }
        if entry_metadata.is_file() {
            continue;
        }
        if !entry_metadata.is_dir() {
            return Err(StackError::SkillInstallFailed {
                reason: format!("refusing to port special file `{}`", entry_path.display()),
            });
        }
        let Some(skill_name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if validate_skill_name(&skill_name).is_err() {
            continue;
        }
        let descriptor = entry_path.join(SKILL_DESCRIPTOR);
        let descriptor_metadata = match std::fs::symlink_metadata(&descriptor) {
            Ok(metadata) => metadata,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(StackError::SkillInstallFailed {
                    reason: format!("stat skill descriptor `{}`: {source}", descriptor.display()),
                });
            }
        };
        if descriptor_metadata.file_type().is_symlink() || !descriptor_metadata.is_file() {
            return Err(StackError::SkillInstallFailed {
                reason: format!("skill `{skill_name}` descriptor must be a regular SKILL.md file"),
            });
        }
        validate_skill_dir_for_port(&entry_path)?;
        candidates.push((skill_name, entry_path));
    }

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
        directories: source.directories.iter().map(resolve_directory).collect(),
        plugin_bundles: source
            .plugin_bundles
            .iter()
            .map(resolve_plugin_bundle)
            .collect(),
    }
}

fn resolve_directory(directory: &SkillDirectory) -> ResolvedSkillDirectory {
    ResolvedSkillDirectory {
        path: directory.path.clone(),
        installable: directory.installable,
        indexed_names: directory.indexed_names.clone(),
    }
}

fn resolve_plugin_bundle(plugin_bundle: &PluginBundleDirectory) -> ResolvedPluginBundleDirectory {
    ResolvedPluginBundleDirectory {
        path: plugin_bundle.path.clone(),
        installable_plugins: plugin_bundle.installable_plugins.clone(),
        excluded_plugins: plugin_bundle.excluded_plugins.clone(),
    }
}

fn find_skill_dir(
    source: &ResolvedSkillSource,
    archive_root: &Path,
    skill_name: &str,
) -> Result<PathBuf> {
    for directory in source
        .directories
        .iter()
        .filter(|directory| directory.installable)
    {
        validate_registry_relative_path(&directory.path)?;
        let base = archive_root.join(&directory.path);
        let candidate = base.join(skill_name);
        if !candidate.exists() {
            continue;
        }
        let metadata = std::fs::symlink_metadata(&candidate).map_err(|source| {
            StackError::SkillInstallFailed {
                reason: format!("stat skill directory `{}`: {source}", candidate.display()),
            }
        })?;
        if !metadata.is_dir() {
            return Err(StackError::SkillInstallFailed {
                reason: format!("skill `{skill_name}` source path is not a directory"),
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
                reason: format!("skill `{skill_name}` descriptor must be a regular SKILL.md file"),
            });
        }
        return Ok(candidate);
    }
    Err(StackError::SkillInstallSkillMissing {
        source_id: source.id.clone(),
        skill: skill_name.to_owned(),
    })
}

fn validate_requested_plugins(source: &ResolvedSkillSource, plugin_names: &[String]) -> Result<()> {
    for plugin_name in plugin_names {
        if plugin_is_installable(source, plugin_name) {
            continue;
        }
        if plugin_is_excluded(source, plugin_name) {
            return Err(StackError::PluginInstallNoSkills {
                source_id: source.id.clone(),
                plugin: plugin_name.clone(),
            });
        }
        return Err(StackError::PluginInstallPluginMissing {
            source_id: source.id.clone(),
            plugin: plugin_name.clone(),
        });
    }
    Ok(())
}

fn plugin_is_installable(source: &ResolvedSkillSource, plugin_name: &str) -> bool {
    source.plugin_bundles.iter().any(|plugin_bundle| {
        plugin_bundle
            .installable_plugins
            .iter()
            .any(|candidate| candidate == plugin_name)
    })
}

fn plugin_is_excluded(source: &ResolvedSkillSource, plugin_name: &str) -> bool {
    source.plugin_bundles.iter().any(|plugin_bundle| {
        plugin_bundle
            .excluded_plugins
            .iter()
            .any(|candidate| candidate == plugin_name)
    })
}

fn find_plugin_skill_dirs(
    source: &ResolvedSkillSource,
    archive_root: &Path,
    plugin_name: &str,
) -> Result<Vec<(String, PathBuf)>> {
    let mut skills = Vec::new();
    for plugin_bundle in &source.plugin_bundles {
        validate_registry_relative_path(&plugin_bundle.path)?;
        let skills_root = archive_root
            .join(&plugin_bundle.path)
            .join(plugin_name)
            .join("skills");
        let root_metadata = match std::fs::symlink_metadata(&skills_root) {
            Ok(metadata) => metadata,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(StackError::PluginInstallFailed {
                    reason: format!(
                        "stat plugin skills root `{}`: {source}",
                        skills_root.display()
                    ),
                });
            }
        };
        if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
            return Err(StackError::PluginInstallFailed {
                reason: format!(
                    "plugin `{plugin_name}` skills path `{}` is not a directory",
                    skills_root.display()
                ),
            });
        }
        for entry in
            std::fs::read_dir(&skills_root).map_err(|source| StackError::PluginInstallFailed {
                reason: format!(
                    "read plugin skills root `{}`: {source}",
                    skills_root.display()
                ),
            })?
        {
            let entry = entry.map_err(|source| StackError::PluginInstallFailed {
                reason: format!(
                    "read plugin skills root entry `{}`: {source}",
                    skills_root.display()
                ),
            })?;
            let skill_name = entry.file_name().to_string_lossy().into_owned();
            let entry_path = entry.path();
            if validate_skill_name(&skill_name).is_err() {
                continue;
            }
            let entry_metadata = std::fs::symlink_metadata(&entry_path).map_err(|source| {
                StackError::PluginInstallFailed {
                    reason: format!("stat plugin skill `{}`: {source}", entry_path.display()),
                }
            })?;
            if entry_metadata.file_type().is_symlink() || !entry_metadata.is_dir() {
                continue;
            }
            let descriptor = entry_path.join(SKILL_DESCRIPTOR);
            let descriptor_metadata = match std::fs::symlink_metadata(&descriptor) {
                Ok(metadata) => metadata,
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => continue,
                Err(source) => {
                    return Err(StackError::PluginInstallFailed {
                        reason: format!(
                            "stat plugin skill descriptor `{}`: {source}",
                            descriptor.display()
                        ),
                    });
                }
            };
            if descriptor_metadata.file_type().is_symlink() || !descriptor_metadata.is_file() {
                return Err(StackError::PluginInstallFailed {
                    reason: format!(
                        "plugin skill `{skill_name}` descriptor must be a regular SKILL.md file"
                    ),
                });
            }
            skills.push((skill_name, entry_path));
        }
    }
    skills.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(skills)
}

fn existing_target_state(target_dir: &Path) -> Result<ExistingTargetState> {
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

fn copy_skill_dir_atomically(source_dir: &Path, target_dir: &Path, skill_name: &str) -> Result<()> {
    let parent = target_dir
        .parent()
        .ok_or_else(|| StackError::SkillInstallFailed {
            reason: format!("skill target `{}` has no parent", target_dir.display()),
        })?;
    let tempdir = tempfile::Builder::new()
        .prefix(&format!(".{skill_name}."))
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

fn replace_skill_dir_atomically(
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
        .prefix(&format!(".{skill_name}."))
        .tempdir_in(parent)
        .map_err(|source| StackError::SkillInstallFailed {
            reason: format!(
                "create temporary skill target in `{}`: {source}",
                parent.display()
            ),
        })?;
    copy_dir_recursive(source_dir, tempdir.path())?;

    let backup = tempfile::Builder::new()
        .prefix(&format!(".{skill_name}.backup."))
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

fn copy_dir_recursive(source_dir: &Path, target_dir: &Path) -> Result<()> {
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

fn validate_skill_dir_for_port(source_dir: &Path) -> Result<()> {
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

fn ensure_directory_no_symlink_ancestors(path: &Path, create_missing: bool) -> Result<()> {
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

fn source_root_exists_without_symlink_ancestors(path: &Path) -> Result<bool> {
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

fn validate_skill_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.split('-').all(|segment| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        });
    if valid {
        Ok(())
    } else {
        Err(StackError::SkillInstallInvalidName {
            name: name.to_owned(),
        })
    }
}

fn validate_plugin_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.split('-').all(|segment| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        });
    if valid {
        Ok(())
    } else {
        Err(StackError::PluginInstallInvalidName {
            name: name.to_owned(),
        })
    }
}

fn validate_github_owner(owner: &str) -> Result<()> {
    let valid = !owner.is_empty()
        && owner.len() <= 39
        && !owner.starts_with('-')
        && !owner.ends_with('-')
        && owner
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
    if valid {
        Ok(())
    } else {
        Err(StackError::SkillInstallInvalidSource {
            source_id: format!("{SOURCE_CUSTOM_GITHUB_PREFIX}{owner}"),
        })
    }
}

fn validate_registry_relative_path(value: &str) -> Result<()> {
    let path = Path::new(value);
    if path.is_absolute() {
        return Err(StackError::SkillInstallFailed {
            reason: format!("skill directory `{value}` must be relative"),
        });
    }
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                return Err(StackError::SkillInstallFailed {
                    reason: format!("skill directory `{value}` contains an unsafe path segment"),
                });
            }
        }
    }
    Ok(())
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExistingTargetState {
    Missing,
    AlreadyInstalled,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> ResolvedSkillSource {
        ResolvedSkillSource {
            id: "openai-skills".to_owned(),
            name: "OpenAI Agent Skills".to_owned(),
            owner: "openai".to_owned(),
            repo: "skills".to_owned(),
            url: "https://github.com/openai/skills".to_owned(),
            branch: "main".to_owned(),
            verified_commit: None,
            indexed_commit: None,
            descriptor: SKILL_DESCRIPTOR.to_owned(),
            directories: vec![
                ResolvedSkillDirectory {
                    path: "skills/.system".to_owned(),
                    installable: false,
                    indexed_names: Vec::new(),
                },
                ResolvedSkillDirectory {
                    path: "skills/.curated".to_owned(),
                    installable: true,
                    indexed_names: Vec::new(),
                },
            ],
            plugin_bundles: Vec::new(),
        }
    }

    fn plugin_source() -> ResolvedSkillSource {
        ResolvedSkillSource {
            id: OPENAI_PLUGINS_SOURCE_ID.to_owned(),
            name: "OpenAI Plugin Skills".to_owned(),
            owner: "openai".to_owned(),
            repo: "plugins".to_owned(),
            url: "https://github.com/openai/plugins".to_owned(),
            branch: "main".to_owned(),
            verified_commit: None,
            indexed_commit: None,
            descriptor: SKILL_DESCRIPTOR.to_owned(),
            directories: Vec::new(),
            plugin_bundles: vec![ResolvedPluginBundleDirectory {
                path: "plugins".to_owned(),
                installable_plugins: vec!["cloudflare".to_owned(), "github".to_owned()],
                excluded_plugins: vec!["empty-plugin".to_owned()],
            }],
        }
    }

    fn write_skill(root: &Path, directory: &str, name: &str) {
        let skill_dir = root.join(directory).join(name);
        std::fs::create_dir_all(&skill_dir).expect("skill dir");
        std::fs::write(skill_dir.join(SKILL_DESCRIPTOR), "# Skill\n").expect("descriptor");
        std::fs::write(skill_dir.join("script.sh"), "true\n").expect("script");
    }

    fn write_plugin_skill(root: &Path, plugin: &str, name: &str) {
        write_skill(root, &format!("plugins/{plugin}/skills"), name);
    }

    fn write_installed_skill(root: &Path, name: &str, descriptor: &str) {
        let skill_dir = root.join(name);
        std::fs::create_dir_all(&skill_dir).expect("skill dir");
        std::fs::write(skill_dir.join(SKILL_DESCRIPTOR), descriptor).expect("descriptor");
        std::fs::write(skill_dir.join("script.sh"), "true\n").expect("script");
    }

    fn canonical_temp_home(tempdir: &tempfile::TempDir) -> PathBuf {
        tempdir.path().canonicalize().expect("canonical temp home")
    }

    #[test]
    fn parses_official_and_custom_sources() {
        assert_eq!(
            parse_skill_source("openai").expect("openai"),
            SkillSourceSelection::Official {
                id: OPENAI_SKILLS_SOURCE_ID.to_owned()
            }
        );
        assert_eq!(
            parse_skill_source("anthropic").expect("anthropic"),
            SkillSourceSelection::Official {
                id: ANTHROPIC_SKILLS_SOURCE_ID.to_owned()
            }
        );
        assert_eq!(
            parse_skill_source("github:my-org").expect("custom"),
            SkillSourceSelection::CustomGithubOwner {
                owner: "my-org".to_owned()
            }
        );
    }

    #[test]
    fn parses_openai_plugin_source() {
        assert_eq!(
            parse_plugin_source("openai").expect("openai"),
            PluginSourceSelection::Official {
                id: OPENAI_PLUGINS_SOURCE_ID.to_owned()
            }
        );
        let err = parse_plugin_source("anthropic").expect_err("unsupported");
        assert!(matches!(err, StackError::PluginInstallInvalidSource { .. }));
    }

    #[test]
    fn rejects_invalid_skill_names() {
        for name in ["", "Upper", "two--dash", "-bad", "bad_", "bad/name"] {
            let err = parse_skill_names(&[name.to_owned()]).expect_err("invalid");
            assert!(matches!(err, StackError::SkillInstallInvalidName { .. }));
        }
    }

    #[test]
    fn rejects_duplicate_skill_names() {
        let err =
            parse_skill_names(&["repo-map,repo-map".to_owned()]).expect_err("duplicate rejected");
        assert!(matches!(err, StackError::SkillInstallFailed { .. }));
    }

    #[test]
    fn rejects_invalid_plugin_names() {
        for name in ["", "Upper", "two--dash", "-bad", "bad_", "bad/name"] {
            let err = parse_plugin_names(&[name.to_owned()]).expect_err("invalid");
            assert!(matches!(err, StackError::PluginInstallInvalidName { .. }));
        }
    }

    #[test]
    fn rejects_duplicate_plugin_names() {
        let err = parse_plugin_names(&["cloudflare,cloudflare".to_owned()]).expect_err("duplicate");
        assert!(matches!(err, StackError::PluginInstallFailed { .. }));
    }

    #[test]
    fn resolves_custom_github_owner_to_skills_repo() {
        let catalog = SkillCatalog::load_embedded().expect("catalog");
        let selection = SkillSourceSelection::CustomGithubOwner {
            owner: "example-org".to_owned(),
        };

        let source = resolve_source(&selection, &catalog).expect("custom source");

        assert_eq!(source.owner, "example-org");
        assert_eq!(source.repo, CUSTOM_SKILLS_REPO);
        assert_eq!(source.branch, CUSTOM_SKILLS_BRANCH);
        assert_eq!(source.url, "https://github.com/example-org/skills");
        assert_eq!(source.directories[0].path, CUSTOM_SKILLS_DIRECTORY);
        assert!(source.directories[0].installable);
    }

    #[test]
    fn install_from_extracted_root_copies_multiple_skills() {
        let archive = tempfile::tempdir().expect("archive");
        let home = tempfile::tempdir().expect("home");
        write_skill(archive.path(), "skills/.curated", "repo-map");
        write_skill(archive.path(), "skills/.curated", "code-review");
        let destination = canonical_temp_home(&home).join(".agents/skills");

        let report = install_from_extracted_root(
            &source(),
            archive.path(),
            &destination,
            &["repo-map,code-review".to_owned()],
        )
        .expect("install");

        assert_eq!(report.installed.len(), 2);
        assert!(
            destination
                .join("repo-map")
                .join(SKILL_DESCRIPTOR)
                .is_file()
        );
        assert!(destination.join("code-review").join("script.sh").is_file());
    }

    #[test]
    fn install_from_extracted_root_ignores_noninstallable_system_directory() {
        let archive = tempfile::tempdir().expect("archive");
        let home = tempfile::tempdir().expect("home");
        write_skill(archive.path(), "skills/.system", "internal-only");
        let destination = canonical_temp_home(&home).join(".agents/skills");

        let err = install_from_extracted_root(
            &source(),
            archive.path(),
            &destination,
            &["internal-only".to_owned()],
        )
        .expect_err("system skill not installable");

        assert!(matches!(err, StackError::SkillInstallSkillMissing { .. }));
    }

    #[test]
    fn install_from_extracted_root_rejects_missing_skill() {
        let archive = tempfile::tempdir().expect("archive");
        let home = tempfile::tempdir().expect("home");

        let err = install_from_extracted_root(
            &source(),
            archive.path(),
            &canonical_temp_home(&home).join(".agents/skills"),
            &["missing-skill".to_owned()],
        )
        .expect_err("missing skill");

        assert!(matches!(err, StackError::SkillInstallSkillMissing { .. }));
    }

    #[test]
    fn install_from_extracted_root_rejects_descriptor_symlink() {
        let archive = tempfile::tempdir().expect("archive");
        let home = tempfile::tempdir().expect("home");
        let skill_dir = archive.path().join("skills/.curated/linked-skill");
        std::fs::create_dir_all(&skill_dir).expect("skill dir");
        std::fs::write(archive.path().join("target.md"), "# Skill\n").expect("target");
        #[cfg(unix)]
        std::os::unix::fs::symlink("../../target.md", skill_dir.join(SKILL_DESCRIPTOR))
            .expect("symlink");

        #[cfg(unix)]
        {
            let err = install_from_extracted_root(
                &source(),
                archive.path(),
                &canonical_temp_home(&home).join(".agents/skills"),
                &["linked-skill".to_owned()],
            )
            .expect_err("symlink descriptor rejected");
            assert!(matches!(err, StackError::SkillInstallFailed { .. }));
        }
    }

    #[test]
    fn install_from_extracted_root_rejects_target_conflict() {
        let archive = tempfile::tempdir().expect("archive");
        let home = tempfile::tempdir().expect("home");
        write_skill(archive.path(), "skills/.curated", "repo-map");
        let destination = canonical_temp_home(&home).join(".agents/skills");
        std::fs::create_dir_all(destination.join("repo-map")).expect("target");

        let err = install_from_extracted_root(
            &source(),
            archive.path(),
            &destination,
            &["repo-map".to_owned()],
        )
        .expect_err("target conflict");

        assert!(matches!(err, StackError::SkillInstallTargetConflict { .. }));
    }

    #[test]
    fn install_from_extracted_root_skips_existing_skill() {
        let archive = tempfile::tempdir().expect("archive");
        let home = tempfile::tempdir().expect("home");
        write_skill(archive.path(), "skills/.curated", "repo-map");
        let destination = canonical_temp_home(&home).join(".agents/skills");
        std::fs::create_dir_all(destination.join("repo-map")).expect("target");
        std::fs::write(
            destination.join("repo-map").join(SKILL_DESCRIPTOR),
            "# Old\n",
        )
        .expect("descriptor");

        let report = install_from_extracted_root(
            &source(),
            archive.path(),
            &destination,
            &["repo-map".to_owned()],
        )
        .expect("idempotent skip");

        assert!(report.installed.is_empty());
        assert_eq!(report.skipped.len(), 1);
    }

    #[test]
    fn install_plugins_from_extracted_root_installs_all_plugin_skills() {
        let archive = tempfile::tempdir().expect("archive");
        let home = tempfile::tempdir().expect("home");
        write_plugin_skill(archive.path(), "cloudflare", "workers-best-practices");
        write_plugin_skill(archive.path(), "cloudflare", "wrangler");
        std::fs::write(
            archive
                .path()
                .join("plugins/cloudflare/asset-outside-skills.txt"),
            "ignored\n",
        )
        .expect("plugin asset");
        let destination = canonical_temp_home(&home).join(".agents/skills");

        let report = install_plugins_from_extracted_root(
            &plugin_source(),
            archive.path(),
            &destination,
            &["cloudflare".to_owned()],
        )
        .expect("install plugin");

        assert_eq!(
            report
                .installed
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["workers-best-practices", "wrangler"]
        );
        assert!(
            destination
                .join("workers-best-practices")
                .join(SKILL_DESCRIPTOR)
                .is_file()
        );
        assert!(!destination.join("asset-outside-skills.txt").exists());
    }

    #[test]
    fn install_plugins_from_extracted_root_rejects_excluded_plugin() {
        let archive = tempfile::tempdir().expect("archive");
        let home = tempfile::tempdir().expect("home");

        let err = install_plugins_from_extracted_root(
            &plugin_source(),
            archive.path(),
            &canonical_temp_home(&home).join(".agents/skills"),
            &["empty-plugin".to_owned()],
        )
        .expect_err("excluded plugin rejected");

        assert!(matches!(err, StackError::PluginInstallNoSkills { .. }));
    }

    #[test]
    fn install_plugins_from_extracted_root_rejects_unknown_plugin() {
        let archive = tempfile::tempdir().expect("archive");
        let home = tempfile::tempdir().expect("home");

        let err = install_plugins_from_extracted_root(
            &plugin_source(),
            archive.path(),
            &canonical_temp_home(&home).join(".agents/skills"),
            &["missing-plugin".to_owned()],
        )
        .expect_err("unknown plugin rejected");

        assert!(matches!(err, StackError::PluginInstallPluginMissing { .. }));
    }

    #[test]
    fn install_plugins_from_extracted_root_rejects_plugin_without_valid_skills() {
        let archive = tempfile::tempdir().expect("archive");
        let home = tempfile::tempdir().expect("home");
        let mut source = plugin_source();
        source.plugin_bundles[0]
            .installable_plugins
            .push("declared-empty".to_owned());
        std::fs::create_dir_all(archive.path().join("plugins/declared-empty/skills"))
            .expect("empty skills dir");

        let err = install_plugins_from_extracted_root(
            &source,
            archive.path(),
            &canonical_temp_home(&home).join(".agents/skills"),
            &["declared-empty".to_owned()],
        )
        .expect_err("empty plugin rejected");

        assert!(matches!(err, StackError::PluginInstallNoSkills { .. }));
    }

    #[test]
    fn install_plugins_from_extracted_root_rejects_duplicate_expanded_skill_names() {
        let archive = tempfile::tempdir().expect("archive");
        let home = tempfile::tempdir().expect("home");
        write_plugin_skill(archive.path(), "cloudflare", "shared-skill");
        write_plugin_skill(archive.path(), "github", "shared-skill");

        let err = install_plugins_from_extracted_root(
            &plugin_source(),
            archive.path(),
            &canonical_temp_home(&home).join(".agents/skills"),
            &["cloudflare,github".to_owned()],
        )
        .expect_err("duplicate expanded skill rejected");

        assert!(matches!(err, StackError::PluginInstallFailed { .. }));
    }

    #[test]
    fn install_plugins_from_extracted_root_rejects_target_conflict() {
        let archive = tempfile::tempdir().expect("archive");
        let home = tempfile::tempdir().expect("home");
        write_plugin_skill(archive.path(), "cloudflare", "wrangler");
        let destination = canonical_temp_home(&home).join(".agents/skills");
        std::fs::create_dir_all(destination.join("wrangler")).expect("target");

        let err = install_plugins_from_extracted_root(
            &plugin_source(),
            archive.path(),
            &destination,
            &["cloudflare".to_owned()],
        )
        .expect_err("target conflict");

        assert!(matches!(err, StackError::SkillInstallTargetConflict { .. }));
    }

    #[test]
    #[cfg(unix)]
    fn all_skills_installed_rejects_symlinked_target() {
        let home = tempfile::tempdir().expect("home");
        let destination = canonical_temp_home(&home).join(".agents/skills");
        let external = tempfile::tempdir().expect("external");
        std::fs::create_dir_all(&destination).expect("destination");
        std::fs::write(external.path().join(SKILL_DESCRIPTOR), "# Skill\n").expect("descriptor");
        std::os::unix::fs::symlink(external.path(), destination.join("repo-map")).expect("symlink");

        assert!(!all_skills_installed(
            &destination,
            &["repo-map".to_owned()]
        ));
    }

    #[test]
    #[cfg(unix)]
    fn install_from_extracted_root_rejects_symlinked_destination_ancestor() {
        let archive = tempfile::tempdir().expect("archive");
        let home = tempfile::tempdir().expect("home");
        let external = tempfile::tempdir().expect("external");
        write_skill(archive.path(), "skills/.curated", "repo-map");
        let home_path = canonical_temp_home(&home);
        std::os::unix::fs::symlink(external.path(), home_path.join(".agents")).expect("symlink");
        let destination = home_path.join(".agents/skills");

        let err = install_from_extracted_root(
            &source(),
            archive.path(),
            &destination,
            &["repo-map".to_owned()],
        )
        .expect_err("symlinked ancestor rejected");

        assert!(matches!(err, StackError::SkillInstallTargetConflict { .. }));
    }

    #[test]
    fn expands_home_relative_install_dir() {
        let home = Path::new("/tmp/test-home");
        assert_eq!(
            expand_agent_skills_install_dir(home, "~/.agents/skills").expect("expand"),
            Path::new("/tmp/test-home/.agents/skills")
        );
    }

    #[test]
    fn port_skill_directories_shared_path_is_noop() {
        let home = tempfile::tempdir().expect("home");
        let source = canonical_temp_home(&home).join(".agents/skills");

        let report = port_skill_directories(&source, &source).expect("port");

        assert_eq!(report.status, SkillPortStatus::Shared);
        assert!(report.copied.is_empty());
        assert!(report.overwritten.is_empty());
    }

    #[test]
    fn port_skill_directories_copies_valid_skills() {
        let home = tempfile::tempdir().expect("home");
        let home = canonical_temp_home(&home);
        let source = home.join(".agents/skills");
        let target = home.join(".config/agents/skills");
        write_installed_skill(&source, "repo-map", "# Repo Map\n");
        write_installed_skill(&source, "code-review", "# Code Review\n");

        let report = port_skill_directories(&source, &target).expect("port");

        assert_eq!(report.status, SkillPortStatus::Copied);
        assert_eq!(report.copied.len(), 2);
        assert!(target.join("repo-map").join(SKILL_DESCRIPTOR).is_file());
        assert!(target.join("code-review").join("script.sh").is_file());
    }

    #[test]
    fn port_skill_directories_overwrites_valid_target_skill() {
        let home = tempfile::tempdir().expect("home");
        let home = canonical_temp_home(&home);
        let source = home.join(".agents/skills");
        let target = home.join(".config/agents/skills");
        write_installed_skill(&source, "repo-map", "# New\n");
        write_installed_skill(&target, "repo-map", "# Old\n");
        std::fs::write(target.join("repo-map").join("old.txt"), "old\n").expect("old file");

        let report = port_skill_directories(&source, &target).expect("port");

        assert_eq!(report.status, SkillPortStatus::Copied);
        assert!(report.copied.is_empty());
        assert_eq!(report.overwritten.len(), 1);
        assert_eq!(
            std::fs::read_to_string(target.join("repo-map").join(SKILL_DESCRIPTOR))
                .expect("descriptor"),
            "# New\n"
        );
        assert!(!target.join("repo-map").join("old.txt").exists());
    }

    #[test]
    #[cfg(unix)]
    fn port_skill_directories_preflight_rejects_nested_symlink_before_target_mutation() {
        let home = tempfile::tempdir().expect("home");
        let home = canonical_temp_home(&home);
        let source = home.join(".agents/skills");
        let target = home.join(".config/agents/skills");
        write_installed_skill(&source, "a-skill", "# New\n");
        write_installed_skill(&target, "a-skill", "# Old\n");
        write_installed_skill(&source, "b-skill", "# B\n");
        let external = tempfile::tempdir().expect("external");
        std::fs::create_dir_all(source.join("b-skill/nested")).expect("nested");
        std::os::unix::fs::symlink(external.path(), source.join("b-skill/nested/symlinked-dir"))
            .expect("symlink");

        let err = port_skill_directories(&source, &target).expect_err("nested symlink");

        assert!(matches!(err, StackError::SkillInstallFailed { .. }));
        assert_eq!(
            std::fs::read_to_string(target.join("a-skill").join(SKILL_DESCRIPTOR))
                .expect("descriptor"),
            "# Old\n"
        );
    }

    #[test]
    fn port_skill_directories_rejects_target_conflict() {
        let home = tempfile::tempdir().expect("home");
        let home = canonical_temp_home(&home);
        let source = home.join(".agents/skills");
        let target = home.join(".config/agents/skills");
        write_installed_skill(&source, "repo-map", "# Repo Map\n");
        std::fs::create_dir_all(target.join("repo-map")).expect("target");

        let err = port_skill_directories(&source, &target).expect_err("conflict");

        assert!(matches!(err, StackError::SkillInstallTargetConflict { .. }));
    }

    #[test]
    #[cfg(unix)]
    fn port_skill_directories_rejects_source_symlink() {
        let home = tempfile::tempdir().expect("home");
        let home = canonical_temp_home(&home);
        let source = home.join(".agents/skills");
        let target = home.join(".config/agents/skills");
        let external = tempfile::tempdir().expect("external");
        std::fs::create_dir_all(&source).expect("source root");
        std::fs::write(external.path().join(SKILL_DESCRIPTOR), "# Skill\n").expect("descriptor");
        std::os::unix::fs::symlink(external.path(), source.join("repo-map")).expect("symlink");

        let err = port_skill_directories(&source, &target).expect_err("symlink");

        assert!(matches!(err, StackError::SkillInstallFailed { .. }));
    }

    #[test]
    fn port_skill_directories_skips_non_skills_and_invalid_names() {
        let home = tempfile::tempdir().expect("home");
        let home = canonical_temp_home(&home);
        let source = home.join(".agents/skills");
        let target = home.join(".config/agents/skills");
        std::fs::create_dir_all(source.join("notes")).expect("notes");
        std::fs::create_dir_all(source.join("BadName")).expect("bad name");
        std::fs::write(source.join("README.md"), "readme\n").expect("readme");

        let report = port_skill_directories(&source, &target).expect("port");

        assert_eq!(report.status, SkillPortStatus::NoneFound);
        assert!(!target.exists());
    }

    #[test]
    fn port_skill_directories_missing_source_is_none_found() {
        let home = tempfile::tempdir().expect("home");
        let home = canonical_temp_home(&home);

        let report = port_skill_directories(
            &home.join(".agents/skills"),
            &home.join(".config/agents/skills"),
        )
        .expect("port");

        assert_eq!(report.status, SkillPortStatus::NoneFound);
        assert!(report.copied.is_empty());
        assert!(report.overwritten.is_empty());
    }

    #[test]
    fn port_agent_skills_treats_unknown_source_agent_as_noop() {
        let home = tempfile::tempdir().expect("home");
        let catalog = RegistryCatalog::load_embedded().expect("registry");

        let report =
            port_agent_skills(home.path(), &catalog, "removed-agent", "opencode").expect("port");

        assert_eq!(report, None);
    }
}
