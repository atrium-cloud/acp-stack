//! Catalog of trusted Agent Skills and plugin-bundled skill sources.
//!
//! The embedded `data/skills.toml` records official skill directories and
//! plugin bundle snapshots that `acp-stack` may later use for opt-in
//! installation or provisioning. Loading this catalog is intentionally
//! side-effect free: it does not fetch, install, or modify agent-owned config.

use std::path::{Component, Path};

use serde::Deserialize;

use crate::error::{Result, StackError};

const EMBEDDED_SKILL_REGISTRY: &str = include_str!("../../../data/skills.toml");
const SKILL_DESCRIPTOR: &str = "SKILL.md";
const GITHUB_URL_PREFIX: &str = "https://github.com/";
const PATH_SEPARATOR: char = '/';
const CURRENT_DIRECTORY_SEGMENT: &str = ".";
const OFFICIAL_SOURCES: &[(&str, &str)] = &[
    ("anthropics", "skills"),
    ("openai", "skills"),
    ("openai", "plugins"),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillCatalog {
    sources: Vec<SkillSource>,
}

impl SkillCatalog {
    pub fn load_embedded() -> Result<Self> {
        Self::from_toml(EMBEDDED_SKILL_REGISTRY)
    }

    pub fn from_toml(body: &str) -> Result<Self> {
        let parsed: SkillRegistryFile =
            toml::from_str(body).map_err(|source| StackError::RegistryLoad {
                reason: format!("skill registry TOML is invalid: {source}"),
            })?;
        let catalog = Self {
            sources: parsed.sources,
        };
        catalog.validate()?;
        Ok(catalog)
    }

    pub fn lookup(&self, id: &str) -> Option<&SkillSource> {
        self.sources.iter().find(|source| source.id == id)
    }

    pub fn sources(&self) -> &[SkillSource] {
        &self.sources
    }

    pub fn essential_skill_names(&self, source_id: &str) -> Vec<String> {
        self.lookup(source_id)
            .map(|source| {
                source
                    .directories
                    .iter()
                    .flat_map(|directory| directory.essential_names.iter().cloned())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn essential_plugin_names(&self, source_id: &str) -> Vec<String> {
        self.lookup(source_id)
            .map(|source| {
                source
                    .plugin_bundles
                    .iter()
                    .flat_map(|plugin_bundle| plugin_bundle.essential_plugins.iter().cloned())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn validate(&self) -> Result<()> {
        if self.sources.is_empty() {
            return Err(StackError::RegistryLoad {
                reason: "skill registry must declare at least one source".to_owned(),
            });
        }
        let mut seen = std::collections::HashSet::new();
        for source in &self.sources {
            source.validate()?;
            if !seen.insert(source.id.as_str()) {
                return Err(StackError::RegistryLoad {
                    reason: format!("duplicate skill source id `{}`", source.id),
                });
            }
        }
        self.validate_unique_essentials()?;
        Ok(())
    }

    fn validate_unique_essentials(&self) -> Result<()> {
        let mut skill_names = std::collections::HashSet::new();
        let mut plugin_names = std::collections::HashSet::new();
        for source in &self.sources {
            for directory in &source.directories {
                for name in &directory.essential_names {
                    if !skill_names.insert(name.as_str()) {
                        return Err(StackError::RegistryLoad {
                            reason: format!("duplicate essential skill name `{name}`"),
                        });
                    }
                }
            }
            for plugin_bundle in &source.plugin_bundles {
                for name in &plugin_bundle.essential_plugins {
                    if !plugin_names.insert(name.as_str()) {
                        return Err(StackError::RegistryLoad {
                            reason: format!("duplicate essential plugin name `{name}`"),
                        });
                    }
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillSource {
    pub id: String,
    pub name: String,
    pub owner: String,
    pub repo: String,
    pub url: String,
    pub docs: Vec<String>,
    #[serde(default)]
    pub official: bool,
    #[serde(default)]
    pub trusted: bool,
    pub reviewed_at: String,
    pub branch: String,
    #[serde(default)]
    pub verified_commit: Option<String>,
    #[serde(default)]
    pub indexed_commit: Option<String>,
    pub descriptor: String,
    #[serde(default)]
    pub directories: Vec<SkillDirectory>,
    #[serde(default)]
    pub plugin_bundles: Vec<PluginBundleDirectory>,
}

impl SkillSource {
    fn validate(&self) -> Result<()> {
        validate_nonempty("id", &self.id)?;
        validate_nonempty("name", &self.name)?;
        validate_nonempty("owner", &self.owner)?;
        validate_nonempty("repo", &self.repo)?;
        if self.docs.is_empty() {
            return Err(StackError::RegistryLoad {
                reason: format!("skill source `{}` must declare docs sources", self.id),
            });
        }
        for doc_url in &self.docs {
            validate_https_url(&self.id, "docs", doc_url)?;
        }
        validate_nonempty("reviewed_at", &self.reviewed_at)?;
        validate_nonempty("branch", &self.branch)?;
        if !self.official {
            return Err(StackError::RegistryLoad {
                reason: format!("skill source `{}` must be marked official", self.id),
            });
        }
        if !self.trusted {
            return Err(StackError::RegistryLoad {
                reason: format!("skill source `{}` must be marked trusted", self.id),
            });
        }
        if !is_official_source(&self.owner, &self.repo) {
            return Err(StackError::RegistryLoad {
                reason: format!(
                    "official skill source `{}` is not an allowlisted official repository",
                    self.id
                ),
            });
        }
        validate_github_url_matches(self)?;
        if let Some(commit) = self.verified_commit.as_deref() {
            validate_commit_sha(&self.id, "verified_commit", commit)?;
        }
        if let Some(commit) = self.indexed_commit.as_deref() {
            validate_commit_sha(&self.id, "indexed_commit", commit)?;
        }
        if self.descriptor != SKILL_DESCRIPTOR {
            return Err(StackError::RegistryLoad {
                reason: format!(
                    "skill source `{}` descriptor must be `{SKILL_DESCRIPTOR}`",
                    self.id
                ),
            });
        }
        if self.directories.is_empty() && self.plugin_bundles.is_empty() {
            return Err(StackError::RegistryLoad {
                reason: format!(
                    "skill source `{}` must declare at least one directory or plugin bundle",
                    self.id
                ),
            });
        }
        for directory in &self.directories {
            directory.validate(self)?;
        }
        for plugin_bundle in &self.plugin_bundles {
            plugin_bundle.validate(self)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillDirectory {
    pub path: String,
    pub source_url: String,
    #[serde(default)]
    pub verified: bool,
    #[serde(default)]
    pub installable: bool,
    #[serde(default)]
    pub indexed_names: Vec<String>,
    #[serde(default)]
    pub essential_names: Vec<String>,
}

impl SkillDirectory {
    fn validate(&self, source: &SkillSource) -> Result<()> {
        validate_nonempty("directories.path", &self.path)?;
        validate_nonempty("directories.source_url", &self.source_url)?;
        validate_https_url(&source.id, "directories.source_url", &self.source_url)?;
        validate_relative_path(&source.id, &self.path)?;
        validate_directory_source_url(source, self)?;
        if !self.verified {
            return Err(StackError::RegistryLoad {
                reason: format!(
                    "skill source `{}` directory `{}` must be marked verified",
                    source.id, self.path
                ),
            });
        }
        validate_catalog_names(
            &source.id,
            &format!("directory `{}` indexed_names", self.path),
            &self.indexed_names,
        )?;
        validate_catalog_names(
            &source.id,
            &format!("directory `{}` essential_names", self.path),
            &self.essential_names,
        )?;
        validate_essential_catalog_subset(
            &source.id,
            &format!("directory `{}` essential_names", self.path),
            &self.essential_names,
            "indexed_names",
            &self.indexed_names,
        )?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginBundleDirectory {
    pub path: String,
    pub source_url: String,
    #[serde(default)]
    pub verified: bool,
    #[serde(default)]
    pub installable_plugins: Vec<String>,
    #[serde(default)]
    pub essential_plugins: Vec<String>,
    #[serde(default)]
    pub excluded_plugins: Vec<String>,
}

impl PluginBundleDirectory {
    fn validate(&self, source: &SkillSource) -> Result<()> {
        validate_nonempty("plugin_bundles.path", &self.path)?;
        validate_nonempty("plugin_bundles.source_url", &self.source_url)?;
        validate_https_url(&source.id, "plugin_bundles.source_url", &self.source_url)?;
        validate_relative_path(&source.id, &self.path)?;
        validate_plugin_bundle_source_url(source, self)?;
        if !self.verified {
            return Err(StackError::RegistryLoad {
                reason: format!(
                    "skill source `{}` plugin bundle `{}` must be marked verified",
                    source.id, self.path
                ),
            });
        }
        validate_catalog_names(
            &source.id,
            &format!("plugin bundle `{}` installable_plugins", self.path),
            &self.installable_plugins,
        )?;
        validate_catalog_names(
            &source.id,
            &format!("plugin bundle `{}` essential_plugins", self.path),
            &self.essential_plugins,
        )?;
        validate_catalog_names(
            &source.id,
            &format!("plugin bundle `{}` excluded_plugins", self.path),
            &self.excluded_plugins,
        )?;
        validate_essential_catalog_subset(
            &source.id,
            &format!("plugin bundle `{}` essential_plugins", self.path),
            &self.essential_plugins,
            "installable_plugins",
            &self.installable_plugins,
        )?;
        for plugin in &self.installable_plugins {
            if self
                .excluded_plugins
                .iter()
                .any(|excluded| excluded == plugin)
            {
                return Err(StackError::RegistryLoad {
                    reason: format!(
                        "skill source `{}` plugin bundle `{}` lists plugin `{plugin}` as both installable and excluded",
                        source.id, self.path
                    ),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillRegistryFile {
    #[serde(default)]
    sources: Vec<SkillSource>,
}

fn validate_nonempty(field: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(StackError::RegistryLoad {
            reason: format!("skill registry field `{field}` is empty"),
        });
    }
    Ok(())
}

fn is_official_source(owner: &str, repo: &str) -> bool {
    OFFICIAL_SOURCES
        .iter()
        .any(|(official_owner, official_repo)| owner == *official_owner && repo == *official_repo)
}

fn validate_github_url_matches(source: &SkillSource) -> Result<()> {
    let expected = format!("{GITHUB_URL_PREFIX}{}/{}", source.owner, source.repo);
    if source.url.trim_end_matches('/') == expected {
        return Ok(());
    }
    Err(StackError::RegistryLoad {
        reason: format!(
            "skill source `{}` url must match owner/repo `{}/{}`",
            source.id, source.owner, source.repo
        ),
    })
}

fn validate_https_url(source_id: &str, field: &str, value: &str) -> Result<()> {
    if value.starts_with("https://") {
        return Ok(());
    }
    Err(StackError::RegistryLoad {
        reason: format!("skill source `{source_id}` {field} entries must be HTTPS URLs"),
    })
}

fn validate_directory_source_url(source: &SkillSource, directory: &SkillDirectory) -> Result<()> {
    let expected = format!(
        "{}/tree/{}/{}",
        source.url.trim_end_matches('/'),
        source.branch,
        directory.path
    );
    if directory.source_url.trim_end_matches('/') == expected {
        return Ok(());
    }
    Err(StackError::RegistryLoad {
        reason: format!(
            "skill source `{}` directory `{}` source_url must match source url, branch, and path",
            source.id, directory.path
        ),
    })
}

fn validate_plugin_bundle_source_url(
    source: &SkillSource,
    plugin_bundle: &PluginBundleDirectory,
) -> Result<()> {
    let expected = format!(
        "{}/tree/{}/{}",
        source.url.trim_end_matches('/'),
        source.branch,
        plugin_bundle.path
    );
    if plugin_bundle.source_url.trim_end_matches('/') == expected {
        return Ok(());
    }
    Err(StackError::RegistryLoad {
        reason: format!(
            "skill source `{}` plugin bundle `{}` source_url must match source url, branch, and path",
            source.id, plugin_bundle.path
        ),
    })
}

fn validate_commit_sha(source_id: &str, field: &str, commit: &str) -> Result<()> {
    if commit.len() == 40 && commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok(());
    }
    Err(StackError::RegistryLoad {
        reason: format!("skill source `{source_id}` {field} must be a full 40-character hex SHA"),
    })
}

fn validate_catalog_names(source_id: &str, field: &str, names: &[String]) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for name in names {
        if !is_catalog_name(name) {
            return Err(StackError::RegistryLoad {
                reason: format!(
                    "skill source `{source_id}` {field} contains invalid name `{name}`"
                ),
            });
        }
        if !seen.insert(name.as_str()) {
            return Err(StackError::RegistryLoad {
                reason: format!(
                    "skill source `{source_id}` {field} contains duplicate name `{name}`"
                ),
            });
        }
    }
    Ok(())
}

fn validate_essential_catalog_subset(
    source_id: &str,
    field: &str,
    essential_names: &[String],
    indexed_field: &str,
    indexed_names: &[String],
) -> Result<()> {
    for name in essential_names {
        if !indexed_names
            .iter()
            .any(|indexed_name| indexed_name == name)
        {
            return Err(StackError::RegistryLoad {
                reason: format!(
                    "skill source `{source_id}` {field} contains `{name}` missing from {indexed_field}"
                ),
            });
        }
    }
    Ok(())
}

fn is_catalog_name(name: &str) -> bool {
    !name.is_empty()
        && name.split('-').all(|segment| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
}

fn validate_relative_path(source_id: &str, value: &str) -> Result<()> {
    let path = Path::new(value);
    if path.is_absolute() {
        return Err(StackError::RegistryLoad {
            reason: format!("skill source `{source_id}` directory path must be relative"),
        });
    }
    if value
        .split(PATH_SEPARATOR)
        .any(|segment| segment == CURRENT_DIRECTORY_SEGMENT)
    {
        return Err(StackError::RegistryLoad {
            reason: format!("skill source `{source_id}` directory path must not contain `.`"),
        });
    }
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            Component::CurDir => {
                return Err(StackError::RegistryLoad {
                    reason: format!(
                        "skill source `{source_id}` directory path must not contain `.`"
                    ),
                });
            }
            Component::ParentDir => {
                return Err(StackError::RegistryLoad {
                    reason: format!(
                        "skill source `{source_id}` directory path must not contain `..`"
                    ),
                });
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(StackError::RegistryLoad {
                    reason: format!("skill source `{source_id}` directory path must be relative"),
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_body() -> &'static str {
        r#"
[[sources]]
id = "openai-skills"
name = "OpenAI Agent Skills"
owner = "openai"
repo = "skills"
url = "https://github.com/openai/skills"
docs = [
  "https://github.com/openai/skills",
  "https://developers.openai.com/codex/skills",
]
official = true
trusted = true
reviewed_at = "2026-05-27"
branch = "main"
descriptor = "SKILL.md"

[[sources.directories]]
path = "skills/.curated"
source_url = "https://github.com/openai/skills/tree/main/skills/.curated"
verified = true
"#
    }

    #[test]
    fn embedded_skill_registry_parses() {
        let catalog = SkillCatalog::load_embedded().expect("embedded skill registry");
        assert_eq!(catalog.sources().len(), 3);
        assert!(catalog.lookup("anthropic-skills").is_some());
        assert!(catalog.lookup("openai-skills").is_some());
        assert!(catalog.lookup("openai-plugins").is_some());
    }

    #[test]
    fn embedded_skill_registry_contains_official_sources() {
        let catalog = SkillCatalog::load_embedded().expect("embedded skill registry");
        let anthropic = catalog
            .lookup("anthropic-skills")
            .expect("anthropic source exists");
        assert_eq!(anthropic.owner, "anthropics");
        assert_eq!(anthropic.repo, "skills");
        assert_eq!(anthropic.branch, "main");
        assert_eq!(
            anthropic.verified_commit.as_deref(),
            Some("690f15cac7f7b4c055c5ab109c79ed9259934081")
        );
        assert_eq!(anthropic.descriptor, SKILL_DESCRIPTOR);
        assert!(
            anthropic
                .docs
                .iter()
                .any(|url| url == "https://agentskills.io/specification")
        );
        assert!(anthropic.official);
        assert!(anthropic.trusted);
        assert_eq!(
            anthropic
                .directories
                .iter()
                .map(|directory| directory.path.as_str())
                .collect::<Vec<_>>(),
            ["skills"]
        );
        assert_eq!(
            anthropic.directories[0].source_url,
            "https://github.com/anthropics/skills/tree/main/skills"
        );
        assert!(anthropic.directories[0].installable);
        assert_eq!(
            anthropic.directories[0].essential_names,
            ["docx", "pptx", "xlsx", "pdf"]
        );
        assert_eq!(
            catalog.essential_skill_names("anthropic-skills"),
            ["docx", "pptx", "xlsx", "pdf"]
        );

        let openai = catalog
            .lookup("openai-skills")
            .expect("openai source exists");
        assert_eq!(openai.owner, "openai");
        assert_eq!(openai.repo, "skills");
        assert_eq!(
            openai.verified_commit.as_deref(),
            Some("b0401f07213a66414d84a65cb50c1d226f99485a")
        );
        assert!(
            openai
                .docs
                .iter()
                .any(|url| url == "https://developers.openai.com/codex/skills")
        );
        assert!(openai.official);
        assert!(openai.trusted);
        assert_eq!(
            openai
                .directories
                .iter()
                .map(|directory| directory.path.as_str())
                .collect::<Vec<_>>(),
            ["skills/.system", "skills/.curated"]
        );
        assert!(!openai.directories[0].installable);
        assert!(openai.directories[1].installable);
        assert_eq!(
            openai.directories[1].source_url,
            "https://github.com/openai/skills/tree/main/skills/.curated"
        );

        let openai_plugins = catalog
            .lookup("openai-plugins")
            .expect("openai plugins source exists");
        assert_eq!(openai_plugins.owner, "openai");
        assert_eq!(openai_plugins.repo, "plugins");
        assert!(openai_plugins.directories.is_empty());
        assert_eq!(openai_plugins.plugin_bundles[0].path, "plugins");
        assert!(
            openai_plugins.plugin_bundles[0]
                .installable_plugins
                .iter()
                .any(|name| name == "cloudflare")
        );
        assert_eq!(
            openai_plugins.plugin_bundles[0].essential_plugins,
            ["github"]
        );
        assert_eq!(catalog.essential_plugin_names("openai-plugins"), ["github"]);
    }

    #[test]
    fn rejects_duplicate_ids() {
        let body = format!("{}\n{}", valid_body(), valid_body());
        let err = SkillCatalog::from_toml(&body).expect_err("duplicate id rejected");
        assert_registry_reason(err, "duplicate skill source id");
    }

    #[test]
    fn rejects_empty_catalog() {
        let err = SkillCatalog::from_toml("").expect_err("empty catalog rejected");
        assert_registry_reason(err, "must declare at least one source");
    }

    #[test]
    fn rejects_non_allowlisted_official_repo() {
        let body = valid_body().replace("owner = \"openai\"", "owner = \"example\"");
        let err = SkillCatalog::from_toml(&body).expect_err("unknown official source rejected");
        assert_registry_reason(err, "not an allowlisted official repository");
    }

    #[test]
    fn rejects_source_not_marked_official() {
        let body = valid_body().replace("official = true", "official = false");
        let err = SkillCatalog::from_toml(&body).expect_err("unofficial source rejected");
        assert_registry_reason(err, "must be marked official");
    }

    #[test]
    fn rejects_source_not_marked_trusted() {
        let body = valid_body().replace("trusted = true", "trusted = false");
        let err = SkillCatalog::from_toml(&body).expect_err("untrusted source rejected");
        assert_registry_reason(err, "must be marked trusted");
    }

    #[test]
    fn rejects_directory_not_marked_verified() {
        let body = valid_body().replace("verified = true", "verified = false");
        let err = SkillCatalog::from_toml(&body).expect_err("unverified directory rejected");
        assert_registry_reason(err, "must be marked verified");
    }

    #[test]
    fn rejects_mismatched_url() {
        let body = valid_body().replace(
            "url = \"https://github.com/openai/skills\"",
            "url = \"https://github.com/anthropics/skills\"",
        );
        let err = SkillCatalog::from_toml(&body).expect_err("mismatched url rejected");
        assert_registry_reason(err, "url must match owner/repo");
    }

    #[test]
    fn rejects_missing_docs_sources() {
        let body = valid_body().replace(
            "docs = [\n  \"https://github.com/openai/skills\",\n  \"https://developers.openai.com/codex/skills\",\n]\n",
            "",
        );
        let err = SkillCatalog::from_toml(&body).expect_err("missing docs rejected");
        assert_registry_reason(err, "missing field `docs`");
    }

    #[test]
    fn rejects_insecure_docs_source() {
        let body = valid_body().replace(
            "https://developers.openai.com/codex/skills",
            "http://developers.openai.com/codex/skills",
        );
        let err = SkillCatalog::from_toml(&body).expect_err("insecure docs url rejected");
        assert_registry_reason(err, "docs entries must be HTTPS URLs");
    }

    #[test]
    fn rejects_missing_directory_source_url() {
        let body = valid_body().replace(
            "source_url = \"https://github.com/openai/skills/tree/main/skills/.curated\"\n",
            "",
        );
        let err = SkillCatalog::from_toml(&body).expect_err("missing directory source rejected");
        assert_registry_reason(err, "missing field `source_url`");
    }

    #[test]
    fn rejects_mismatched_directory_source_url() {
        let body = valid_body().replace(
            "source_url = \"https://github.com/openai/skills/tree/main/skills/.curated\"",
            "source_url = \"https://github.com/openai/skills/tree/main/skills/.system\"",
        );
        let err = SkillCatalog::from_toml(&body).expect_err("mismatched source url rejected");
        assert_registry_reason(err, "source_url must match source url, branch, and path");
    }

    #[test]
    fn rejects_invalid_commit_pin() {
        let body = valid_body().replace(
            "branch = \"main\"",
            "branch = \"main\"\nverified_commit = \"abc123\"",
        );
        let err = SkillCatalog::from_toml(&body).expect_err("short commit rejected");
        assert_registry_reason(err, "verified_commit");
    }

    #[test]
    fn accepts_full_commit_pin() {
        let body = valid_body().replace(
            "branch = \"main\"",
            "branch = \"main\"\nverified_commit = \"0123456789abcdef0123456789abcdef01234567\"",
        );
        let catalog = SkillCatalog::from_toml(&body).expect("full commit accepted");
        assert_eq!(
            catalog
                .lookup("openai-skills")
                .and_then(|source| source.verified_commit.as_deref()),
            Some("0123456789abcdef0123456789abcdef01234567")
        );
    }

    #[test]
    fn accepts_full_indexed_commit() {
        let body = valid_body().replace(
            "branch = \"main\"",
            "branch = \"main\"\nindexed_commit = \"0123456789abcdef0123456789abcdef01234567\"",
        );
        let catalog = SkillCatalog::from_toml(&body).expect("full commit accepted");
        assert_eq!(
            catalog
                .lookup("openai-skills")
                .and_then(|source| source.indexed_commit.as_deref()),
            Some("0123456789abcdef0123456789abcdef01234567")
        );
    }

    #[test]
    fn accepts_plugin_bundle_source_without_directories() {
        let body = r#"
[[sources]]
id = "openai-plugins"
name = "OpenAI Plugin Skills"
owner = "openai"
repo = "plugins"
url = "https://github.com/openai/plugins"
docs = ["https://github.com/openai/plugins"]
official = true
trusted = true
reviewed_at = "2026-06-23"
branch = "main"
descriptor = "SKILL.md"

[[sources.plugin_bundles]]
path = "plugins"
source_url = "https://github.com/openai/plugins/tree/main/plugins"
verified = true
installable_plugins = ["cloudflare"]
essential_plugins = ["cloudflare"]
excluded_plugins = ["empty-plugin"]
"#;
        let catalog = SkillCatalog::from_toml(body).expect("plugin source accepted");
        let source = catalog.lookup("openai-plugins").expect("source");
        assert_eq!(source.plugin_bundles[0].installable_plugins, ["cloudflare"]);
        assert_eq!(source.plugin_bundles[0].essential_plugins, ["cloudflare"]);
        assert_eq!(source.plugin_bundles[0].excluded_plugins, ["empty-plugin"]);
    }

    #[test]
    fn rejects_plugin_bundle_overlap() {
        let body = r#"
[[sources]]
id = "openai-plugins"
name = "OpenAI Plugin Skills"
owner = "openai"
repo = "plugins"
url = "https://github.com/openai/plugins"
docs = ["https://github.com/openai/plugins"]
official = true
trusted = true
reviewed_at = "2026-06-23"
branch = "main"
descriptor = "SKILL.md"

[[sources.plugin_bundles]]
path = "plugins"
source_url = "https://github.com/openai/plugins/tree/main/plugins"
verified = true
installable_plugins = ["cloudflare"]
excluded_plugins = ["cloudflare"]
"#;
        let err = SkillCatalog::from_toml(body).expect_err("overlap rejected");
        assert_registry_reason(err, "both installable and excluded");
    }

    #[test]
    fn accepts_directory_essential_names() {
        let body = valid_body().replace(
            "verified = true\n",
            "verified = true\ninstallable = true\nindexed_names = [\"docx\", \"pdf\"]\nessential_names = [\"docx\"]\n",
        );
        let catalog = SkillCatalog::from_toml(&body).expect("essential names accepted");

        assert_eq!(catalog.essential_skill_names("openai-skills"), ["docx"]);
    }

    #[test]
    fn rejects_unknown_directory_essential_names() {
        let body = valid_body().replace(
            "verified = true\n",
            "verified = true\ninstallable = true\nindexed_names = [\"docx\"]\nessential_names = [\"pdf\"]\n",
        );
        let err = SkillCatalog::from_toml(&body).expect_err("unknown essential rejected");
        assert_registry_reason(err, "missing from indexed_names");
    }

    #[test]
    fn rejects_duplicate_directory_essential_names() {
        let body = valid_body().replace(
            "verified = true\n",
            "verified = true\ninstallable = true\nindexed_names = [\"docx\"]\nessential_names = [\"docx\", \"docx\"]\n",
        );
        let err = SkillCatalog::from_toml(&body).expect_err("duplicate essential rejected");
        assert_registry_reason(err, "duplicate name `docx`");
    }

    #[test]
    fn rejects_plugin_essential_names_not_installable() {
        let body = r#"
[[sources]]
id = "openai-plugins"
name = "OpenAI Plugin Skills"
owner = "openai"
repo = "plugins"
url = "https://github.com/openai/plugins"
docs = ["https://github.com/openai/plugins"]
official = true
trusted = true
reviewed_at = "2026-06-23"
branch = "main"
descriptor = "SKILL.md"

[[sources.plugin_bundles]]
path = "plugins"
source_url = "https://github.com/openai/plugins/tree/main/plugins"
verified = true
installable_plugins = ["cloudflare"]
essential_plugins = ["empty-plugin"]
excluded_plugins = ["empty-plugin"]
"#;
        let err =
            SkillCatalog::from_toml(body).expect_err("non-installable essential plugin rejected");
        assert_registry_reason(err, "missing from installable_plugins");
    }

    #[test]
    fn rejects_duplicate_plugin_essential_names() {
        let body = r#"
[[sources]]
id = "openai-plugins"
name = "OpenAI Plugin Skills"
owner = "openai"
repo = "plugins"
url = "https://github.com/openai/plugins"
docs = ["https://github.com/openai/plugins"]
official = true
trusted = true
reviewed_at = "2026-06-23"
branch = "main"
descriptor = "SKILL.md"

[[sources.plugin_bundles]]
path = "plugins"
source_url = "https://github.com/openai/plugins/tree/main/plugins"
verified = true
installable_plugins = ["cloudflare"]
essential_plugins = ["cloudflare", "cloudflare"]
excluded_plugins = []
"#;
        let err = SkillCatalog::from_toml(body).expect_err("duplicate essential plugin rejected");
        assert_registry_reason(err, "duplicate name `cloudflare`");
    }

    #[test]
    fn rejects_absolute_directory_path() {
        let body = valid_body().replace("path = \"skills/.curated\"", "path = \"/skills\"");
        let err = SkillCatalog::from_toml(&body).expect_err("absolute path rejected");
        assert_registry_reason(err, "must be relative");
    }

    #[test]
    fn rejects_parent_directory_path() {
        let body = valid_body().replace("path = \"skills/.curated\"", "path = \"skills/../x\"");
        let err = SkillCatalog::from_toml(&body).expect_err("parent path rejected");
        assert_registry_reason(err, "must not contain `..`");
    }

    #[test]
    fn rejects_current_directory_path() {
        let body = valid_body().replace("path = \"skills/.curated\"", "path = \".\"");
        let err = SkillCatalog::from_toml(&body).expect_err("current path rejected");
        assert_registry_reason(err, "must not contain `.`");
    }

    #[test]
    fn rejects_current_directory_segment() {
        let body = valid_body().replace("path = \"skills/.curated\"", "path = \"skills/./x\"");
        let err = SkillCatalog::from_toml(&body).expect_err("current segment rejected");
        assert_registry_reason(err, "must not contain `.`");
    }

    #[test]
    fn rejects_unsupported_descriptor() {
        let body = valid_body().replace("descriptor = \"SKILL.md\"", "descriptor = \"README.md\"");
        let err = SkillCatalog::from_toml(&body).expect_err("descriptor rejected");
        assert_registry_reason(err, "descriptor must be `SKILL.md`");
    }

    fn assert_registry_reason(err: StackError, expected: &str) {
        match err {
            StackError::RegistryLoad { reason } => {
                assert!(reason.contains(expected), "reason: {reason}");
            }
            other => panic!("expected RegistryLoad, got {other:?}"),
        }
    }
}
