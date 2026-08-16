//! Public data model shared by the skill install, port, link, and day-2
//! surfaces: source selections, resolved sources, and the reports the CLI and
//! HTTP routes serialize.

use super::*;

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
