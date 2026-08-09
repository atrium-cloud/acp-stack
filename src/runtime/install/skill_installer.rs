//! Agent Skills installer used by `acps init`.

mod fs;
mod validate;

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{DEFAULT_SKILL_SOURCE_BRANCH, UserSkillSource};
use crate::error::{Result, StackError};
use crate::fs_util::{create_dir_owner_only, set_owner_only_dir, set_owner_only_file};
use crate::runtime::install::agent_registry::{RegistryCatalog, RegistryEntry};
use crate::runtime::install::skill_registry::{
    CatalogSkill, SkillCatalog, SkillDirectory, SkillSource,
};
use crate::runtime::workspace_sources::safe_download::{DownloadOpts, download_to_file};
use crate::runtime::workspace_sources::safe_extract::{ExtractOpts, extract_archive};

use self::fs::*;
pub(crate) use self::validate::validate_install_target_name;
use self::validate::{
    validate_github_owner, validate_registry_relative_path, validate_skill_name,
    validate_skill_selector,
};

pub const SOURCE_CUSTOM_GITHUB_PREFIX: &str = "github:";
const CUSTOM_SKILLS_REPO: &str = "skills";
const CUSTOM_SKILLS_DIRECTORY: &str = "skills";
const SKILL_DESCRIPTOR: &str = "SKILL.md";
/// Marker file inside each acp-stack-installed skill dir proving the runtime
/// manages it. `remove` and switch-port overwrite refuse unmarked dirs, so
/// skills a user placed in the install root by hand are never deleted. The
/// content is the id of the source the skill was installed from.
pub(crate) const MANAGED_SKILL_MARKER: &str = ".acp-stack-managed";
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

/// One entry in the day-2 list surface. Unlike [`SkillInstallEntry`] it
/// carries provenance: the source id recorded in the managed marker at
/// install time, absent for skills the user placed in the install root by
/// hand (which `remove` will refuse to delete).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstalledSkill {
    pub name: String,
    pub path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkillPortReport {
    pub source_root: PathBuf,
    pub target_root: PathBuf,
    pub status: SkillPortStatus,
    pub copied: Vec<SkillInstallEntry>,
    pub overwritten: Vec<SkillInstallEntry>,
    /// Same-named target skills left untouched because they carry no managed
    /// marker — user-owned content is never replaced.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub kept_unmanaged: Vec<SkillInstallEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillPortStatus {
    Shared,
    Copied,
    NoneFound,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkillLinkReport {
    pub install_root: PathBuf,
    pub link_root: PathBuf,
    pub linked: Vec<SkillInstallEntry>,
    pub unchanged: Vec<SkillInstallEntry>,
    pub conflicts: Vec<SkillInstallEntry>,
    pub pruned: Vec<SkillInstallEntry>,
    /// Per-skill failures (`"<skill>: <error>"`) that were skipped so the
    /// rest of the refresh could continue; empty when every skill linked.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

/// Result of a best-effort link refresh: the report on success, or the
/// failure reason when the refresh could not run. The error travels
/// alongside instead of only being logged so API/CLI callers can surface it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillLinkOutcome {
    pub report: Option<SkillLinkReport>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkillRemoveReport {
    pub install_root: PathBuf,
    pub removed: SkillInstallEntry,
}

/// One installable skill surfaced by `source get` inspection: the selector to
/// pass to `add`, plus the frontmatter identity read from its `SKILL.md`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkillMetadata {
    pub selector: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub path: String,
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
                branch: DEFAULT_SKILL_SOURCE_BRANCH.to_owned(),
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

/// Resolve a day-2 source reference against, in order: the embedded catalog by
/// alias, then configured user sources (`[[skills.sources]]`), then an ad-hoc
/// `github:<owner>` (→ `<owner>/skills`) or `github:<owner>/<repo>`. The catalog
/// is checked first so a hand-edited `[[skills.sources]]` entry whose alias
/// collides with a curated one cannot hijack it (the add route already refuses
/// to create such an alias; this keeps the invariant even for direct edits).
pub fn resolve_source_ref(
    source_ref: &str,
    user_sources: &[UserSkillSource],
    catalog: &SkillCatalog,
) -> Result<ResolvedSkillSource> {
    let trimmed = source_ref.trim();
    if let Some(catalog_source) = catalog.lookup_alias(trimmed) {
        return Ok(resolve_official_source(catalog_source));
    }
    if let Some(user) = user_sources.iter().find(|source| source.alias == trimmed) {
        return resolved_user_source(user);
    }
    if let Some(rest) = trimmed.strip_prefix(SOURCE_CUSTOM_GITHUB_PREFIX) {
        return resolve_ad_hoc_github(rest);
    }
    Err(StackError::SkillInstallInvalidSource {
        source_id: trimmed.to_owned(),
    })
}

fn resolved_user_source(user: &UserSkillSource) -> Result<ResolvedSkillSource> {
    let (owner, repo) = split_owner_repo(&user.github)?;
    Ok(github_repo_source(
        user.alias.clone(),
        format!("{} (user source)", user.alias),
        owner,
        repo,
        user.branch.clone(),
    ))
}

fn resolve_ad_hoc_github(rest: &str) -> Result<ResolvedSkillSource> {
    if rest.contains('/') {
        let (owner, repo) = split_owner_repo(rest)?;
        Ok(github_repo_source(
            format!("{owner}-{repo}-skills"),
            format!("{owner}/{repo} Agent Skills"),
            owner,
            repo,
            DEFAULT_SKILL_SOURCE_BRANCH.to_owned(),
        ))
    } else {
        validate_github_owner(rest)?;
        Ok(github_repo_source(
            format!("{rest}-skills"),
            format!("{rest} Agent Skills"),
            rest.to_owned(),
            CUSTOM_SKILLS_REPO.to_owned(),
            DEFAULT_SKILL_SOURCE_BRANCH.to_owned(),
        ))
    }
}

/// Build a non-catalog `ResolvedSkillSource` for a whole GitHub repo. Skills
/// are expected flat under `skills/` (the same convention as `github:<owner>`);
/// nested/frontmatter-indexed layouts are the curated catalog's domain.
fn github_repo_source(
    id: String,
    name: String,
    owner: String,
    repo: String,
    branch: String,
) -> ResolvedSkillSource {
    ResolvedSkillSource {
        id,
        name,
        url: format!("https://github.com/{owner}/{repo}"),
        owner,
        repo,
        branch,
        verified_commit: None,
        indexed_commit: None,
        descriptor: SKILL_DESCRIPTOR.to_owned(),
        catalog_managed: false,
        directories: vec![ResolvedSkillDirectory {
            path: CUSTOM_SKILLS_DIRECTORY.to_owned(),
            installable: true,
        }],
        indexed_skills: Vec::new(),
    }
}

fn split_owner_repo(github: &str) -> Result<(String, String)> {
    let mut parts = github.splitn(3, '/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(owner), Some(repo), None) => {
            validate_github_owner(owner)?;
            validate_github_repo(repo)?;
            Ok((owner.to_owned(), repo.to_owned()))
        }
        _ => Err(StackError::SkillInstallInvalidSource {
            source_id: github.to_owned(),
        }),
    }
}

fn validate_github_repo(repo: &str) -> Result<()> {
    if crate::config::is_valid_github_repo(repo) {
        Ok(())
    } else {
        Err(StackError::SkillInstallInvalidSource {
            source_id: repo.to_owned(),
        })
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
                    return Err(StackError::SkillInstallFailed {
                        reason: format!("read source directory `{}`: {source}", base.display()),
                    });
                }
            };
            for entry in entries {
                let entry = entry.map_err(|source| StackError::SkillInstallFailed {
                    reason: format!("read source directory entry `{}`: {source}", base.display()),
                })?;
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
            return Err(StackError::SkillInstallFailed {
                reason: format!("resolve skill install dir `{}`: {source}", root.display()),
            });
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
            return Err(StackError::SkillInstallFailed {
                reason: format!("resolve skill install dir `{}`: {source}", root.display()),
            });
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
                return Err(StackError::SkillInstallFailed {
                    reason: format!("stat skill path `{}`: {source}", target_dir.display()),
                });
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
    std::fs::remove_dir_all(&target_dir).map_err(|source| StackError::SkillInstallFailed {
        reason: format!(
            "remove installed skill `{}`: {source}",
            target_dir.display()
        ),
    })?;
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
                return Err(StackError::SkillInstallFailed {
                    reason: format!("read skill group directory `{}`: {source}", dir.display()),
                });
            }
        }
        std::fs::remove_dir(dir).map_err(|source| StackError::SkillInstallFailed {
            reason: format!(
                "remove emptied skill group directory `{}`: {source}",
                dir.display()
            ),
        })?;
        current = dir.parent();
    }
    Ok(())
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

/// Symlink every skill under the agent's install root into its
/// `agent_skills_link_dir`, for harnesses that only discover skills from
/// their own directory (e.g. Claude Code reads `~/.claude/skills`, not the
/// shared `~/.agents/skills`). Linking is a one-way mirror: the managed
/// install root is the source of truth and the link dir only receives
/// symlinks. Idempotent: correct links are kept, stale links are repointed,
/// dangling top-level links into the install root are pruned, and real
/// files or directories already at a link path are left alone and reported
/// as conflicts instead of failing.
pub fn link_agent_skills(home: &Path, entry: &RegistryEntry) -> Result<Option<SkillLinkReport>> {
    let Some(link_dir) = entry.agent_skills_link_dir.as_deref() else {
        return Ok(None);
    };
    let home = home
        .canonicalize()
        .map_err(|source| StackError::SkillInstallFailed {
            reason: format!("canonicalize home directory `{}`: {source}", home.display()),
        })?;
    let Some(install_root) = agent_skill_root(&home, entry)? else {
        return Ok(None);
    };
    // Resolve symlinked ancestors (e.g. a dotfiles-managed `~/.agents`) the
    // same way the link root is resolved, so linking works there instead of
    // failing the no-symlink-ancestor check that copy flows require.
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
        // One bad skill must not take down the rest of the refresh: per-skill
        // failures are collected and reported, linking continues, and the
        // prune below still runs.
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

/// Link one skill into the link root: create the symlink, repoint a stale
/// one, keep a correct one, or leave a real file/directory in place as a
/// conflict. Failures are the caller's to downgrade to per-skill errors.
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
        Err(source) => Err(StackError::SkillInstallFailed {
            reason: format!("stat skill link `{}`: {source}", link_path.display()),
        }),
        Ok(metadata) if metadata.file_type().is_symlink() => {
            let target = std::fs::read_link(&link_path).map_err(|source| {
                StackError::SkillInstallFailed {
                    reason: format!("read skill link `{}`: {source}", link_path.display()),
                }
            })?;
            if target != install_dir {
                std::fs::remove_file(&link_path).map_err(|source| {
                    StackError::SkillInstallFailed {
                        reason: format!(
                            "remove stale skill link `{}`: {source}",
                            link_path.display()
                        ),
                    }
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

/// Best-effort wrapper for install/switch flows: a failed link refresh must
/// not abort an otherwise successful operation — the skills stay installed
/// in the shared root and only harness discovery is degraded, so the
/// failure is logged and returned for the caller to surface instead of
/// propagated.
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

/// Canonicalize the longest existing prefix of `path` and re-append the
/// missing tail. A dotfiles-managed home commonly symlinks the harness
/// config directory itself (e.g. `~/.claude` -> `~/dotfiles/claude`);
/// resolving it up front lets the no-symlink-ancestor checks operate on the
/// real directory instead of rejecting the whole link step.
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
                return Err(StackError::SkillInstallFailed {
                    reason: format!(
                        "resolve skill link directory `{}`: {source}",
                        path.display()
                    ),
                });
            }
        }
    }
}

/// Remove symlinks under the link root that point into the install root
/// but whose target no longer exists — the leftover of an uninstalled
/// skill. Linking is a one-way mirror: symlinks pointing into the managed
/// install root are ours wherever they sit, so real directories are
/// recursed into (nested skills live in group directories the linker
/// created), and a directory left empty by pruning is removed with them.
/// Everything else — real files, links pointing elsewhere, directories
/// with any content left — is user-owned and left completely alone.
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
            return Err(StackError::SkillInstallFailed {
                reason: format!(
                    "read skill link directory `{}`: {source}",
                    directory.display()
                ),
            });
        }
    };
    let pruned_before = pruned.len();
    for entry in entries {
        let entry = entry.map_err(|source| StackError::SkillInstallFailed {
            reason: format!(
                "read skill link directory entry `{}`: {source}",
                directory.display()
            ),
        })?;
        let entry_path = entry.path();
        let metadata = std::fs::symlink_metadata(&entry_path).map_err(|source| {
            StackError::SkillInstallFailed {
                reason: format!("stat skill link entry `{}`: {source}", entry_path.display()),
            }
        })?;
        if metadata.file_type().is_symlink() {
            let target = std::fs::read_link(&entry_path).map_err(|source| {
                StackError::SkillInstallFailed {
                    reason: format!("read skill link `{}`: {source}", entry_path.display()),
                }
            })?;
            if !target.starts_with(install_root) {
                continue;
            }
            let dangling = match std::fs::symlink_metadata(&target) {
                Ok(_) => false,
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => true,
                // A target that cannot be stat'd for another reason (e.g. an
                // unreadable ancestor) may still exist; keep the link.
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
            std::fs::remove_file(&entry_path).map_err(|source| StackError::SkillInstallFailed {
                reason: format!(
                    "remove dangling skill link `{}`: {source}",
                    entry_path.display()
                ),
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
    // Remove a group directory only when this prune emptied it; without a
    // prune, or with any content left (user-owned or not), it stays.
    if directory != link_root && pruned.len() > pruned_before {
        let mut remaining =
            std::fs::read_dir(directory).map_err(|source| StackError::SkillInstallFailed {
                reason: format!(
                    "re-read skill link directory `{}`: {source}",
                    directory.display()
                ),
            })?;
        if remaining.next().is_none() {
            std::fs::remove_dir(directory).map_err(|source| StackError::SkillInstallFailed {
                reason: format!(
                    "remove emptied skill link directory `{}`: {source}",
                    directory.display()
                ),
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
                // A same-named skill the runtime did not install: leave the
                // user's folder untouched rather than replacing it.
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
                // Ported skills keep whatever marker the source dir carries;
                // nothing new is installed here, so no marker is staged.
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

/// Tolerant variant of `collect_port_skill_directories` for linking. The
/// port collector's strictness (no symlinks, no special files anywhere in
/// the tree) exists because copying such entries would be unsafe; linking
/// copies nothing — it only symlinks the skill dir — so unexpected entries
/// are skipped with a warning instead of failing the whole refresh. Only a
/// failure to read the install root itself propagates.
fn collect_link_skill_directories(
    install_root: &Path,
    directory: &Path,
    candidates: &mut Vec<(String, PathBuf)>,
) -> Result<()> {
    let descriptor = directory.join(SKILL_DESCRIPTOR);
    match std::fs::symlink_metadata(&descriptor) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                tracing::warn!(
                    path = %descriptor.display(),
                    "skipping skill: descriptor is not a regular SKILL.md file"
                );
                return Ok(());
            }
            let relative = directory.strip_prefix(install_root).map_err(|source| {
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
            if let Err(error) = validate_install_target_name(&skill_name) {
                tracing::warn!(
                    path = %directory.display(),
                    error = %error,
                    "skipping skill: directory does not map to a valid link name"
                );
                return Ok(());
            }
            candidates.push((skill_name, directory.to_path_buf()));
            return Ok(());
        }
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            if directory == install_root {
                return Err(StackError::SkillInstallFailed {
                    reason: format!("stat skill descriptor `{}`: {source}", descriptor.display()),
                });
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
            if directory == install_root {
                return Err(StackError::SkillInstallFailed {
                    reason: format!(
                        "read source skills directory `{}`: {source}",
                        directory.display()
                    ),
                });
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
                tracing::warn!(
                    path = %entry_path.display(),
                    error = %source,
                    "skipping skills directory entry: could not stat"
                );
                continue;
            }
        };
        if metadata.file_type().is_symlink() {
            tracing::warn!(
                path = %entry_path.display(),
                "skipping symlinked entry in skills directory"
            );
            continue;
        }
        if metadata.is_dir() {
            collect_link_skill_directories(install_root, &entry_path, candidates)?;
        } else if !metadata.is_file() {
            tracing::warn!(
                path = %entry_path.display(),
                "skipping special file in skills directory"
            );
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
    Ok(SkillDescriptor {
        name: frontmatter.name,
        description: frontmatter
            .description
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty()),
    })
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
