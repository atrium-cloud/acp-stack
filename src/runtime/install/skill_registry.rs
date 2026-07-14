//! Catalog of reviewed Agent Skill sources and exact installable skill paths.
//!
//! Loading the embedded catalog is side-effect free. Network refresh and source
//! discovery are handled by the `sync-skills-catalog` development command.

use std::collections::HashSet;
use std::path::{Component, Path};

use serde::Deserialize;

use crate::error::{Result, StackError};

const EMBEDDED_SKILL_REGISTRY: &str = include_str!("../../../data/skills.toml");
const SKILL_DESCRIPTOR: &str = "SKILL.md";
const GITHUB_URL_PREFIX: &str = "https://github.com/";
const APPROVED_SOURCES: &[(&str, &str, bool)] = &[
    ("anthropics", "skills", true),
    ("openai", "skills", true),
    ("openai", "plugins", true),
    ("anthropics", "claude-for-legal", true),
    ("anthropics", "financial-services", true),
    ("anthropics", "knowledge-work-plugins", true),
    ("k-dense-ai", "scientific-agent-skills", false),
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

    pub fn lookup_alias(&self, alias: &str) -> Option<&SkillSource> {
        self.sources.iter().find(|source| source.alias == alias)
    }

    pub fn sources(&self) -> &[SkillSource] {
        &self.sources
    }

    fn validate(&self) -> Result<()> {
        if self.sources.is_empty() {
            return Err(registry_error(
                "skill registry must declare at least one source",
            ));
        }
        let mut ids = HashSet::new();
        let mut aliases = HashSet::new();
        for source in &self.sources {
            source.validate()?;
            if !ids.insert(source.id.as_str()) {
                return Err(registry_error(format!(
                    "duplicate skill source id `{}`",
                    source.id
                )));
            }
            if !aliases.insert(source.alias.as_str()) {
                return Err(registry_error(format!(
                    "duplicate skill source alias `{}`",
                    source.alias
                )));
            }
        }
        self.validate_unique_essential_targets()
    }

    fn validate_unique_essential_targets(&self) -> Result<()> {
        let mut names = HashSet::new();
        for source in &self.sources {
            for selector in &source.essential_skills {
                let skill = source
                    .indexed_skills
                    .iter()
                    .find(|skill| skill.selector == *selector)
                    .ok_or_else(|| {
                        registry_error(format!(
                            "skill source `{}` essential skill `{selector}` is not indexed",
                            source.id
                        ))
                    })?;
                if !names.insert(skill.name.as_str()) {
                    return Err(registry_error(format!(
                        "duplicate essential skill install name `{}`",
                        skill.name
                    )));
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillDiscovery {
    Direct,
    Recursive,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillSource {
    pub id: String,
    pub alias: String,
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
    pub discovery: SkillDiscovery,
    #[serde(default)]
    pub preferred_paths: Vec<String>,
    #[serde(default)]
    pub excluded_skills: Vec<String>,
    #[serde(default)]
    pub essential_skills: Vec<String>,
    #[serde(default)]
    pub indexed_skills: Vec<CatalogSkill>,
    #[serde(default)]
    pub directories: Vec<SkillDirectory>,
}

impl SkillSource {
    fn validate(&self) -> Result<()> {
        validate_catalog_name(&self.id, "id", &self.id)?;
        validate_catalog_name(&self.id, "alias", &self.alias)?;
        validate_nonempty("name", &self.name)?;
        validate_nonempty("owner", &self.owner)?;
        validate_nonempty("repo", &self.repo)?;
        if self.docs.is_empty() {
            return Err(registry_error(format!(
                "skill source `{}` must declare docs sources",
                self.id
            )));
        }
        for doc_url in &self.docs {
            validate_https_url(&self.id, "docs", doc_url)?;
        }
        validate_nonempty("reviewed_at", &self.reviewed_at)?;
        validate_nonempty("branch", &self.branch)?;
        if !self.trusted {
            return Err(registry_error(format!(
                "skill source `{}` must be marked trusted",
                self.id
            )));
        }
        let Some((_, _, expected_official)) = APPROVED_SOURCES
            .iter()
            .find(|(owner, repo, _)| self.owner == *owner && self.repo == *repo)
        else {
            return Err(registry_error(format!(
                "skill source `{}` is not an allowlisted repository",
                self.id
            )));
        };
        if self.official != *expected_official {
            return Err(registry_error(format!(
                "skill source `{}` official status does not match its allowlisted repository",
                self.id
            )));
        }
        validate_github_url_matches(self)?;
        if let Some(commit) = self.verified_commit.as_deref() {
            validate_commit_sha(&self.id, "verified_commit", commit)?;
        }
        if let Some(commit) = self.indexed_commit.as_deref() {
            validate_commit_sha(&self.id, "indexed_commit", commit)?;
        }
        match (&self.verified_commit, &self.indexed_commit) {
            (Some(_), Some(_)) => {
                return Err(registry_error(format!(
                    "skill source `{}` must not declare both verified_commit and indexed_commit",
                    self.id
                )));
            }
            (None, None) => {
                return Err(registry_error(format!(
                    "skill source `{}` must pin a verified_commit or indexed_commit",
                    self.id
                )));
            }
            _ => {}
        }
        if self.descriptor != SKILL_DESCRIPTOR {
            return Err(registry_error(format!(
                "skill source `{}` descriptor must be `{SKILL_DESCRIPTOR}`",
                self.id
            )));
        }
        if self.directories.is_empty() {
            return Err(registry_error(format!(
                "skill source `{}` must declare at least one discovery directory",
                self.id
            )));
        }
        for directory in &self.directories {
            directory.validate(self)?;
        }
        validate_path_list(&self.id, "preferred_paths", &self.preferred_paths, false)?;
        validate_path_list(&self.id, "excluded_skills", &self.excluded_skills, false)?;
        self.validate_indexed_skills()?;
        validate_selector_list(&self.id, "essential_skills", &self.essential_skills)?;
        for selector in &self.essential_skills {
            if !self
                .indexed_skills
                .iter()
                .any(|skill| skill.selector == *selector)
            {
                return Err(registry_error(format!(
                    "skill source `{}` essential skill `{selector}` is not indexed",
                    self.id
                )));
            }
        }
        Ok(())
    }

    fn validate_indexed_skills(&self) -> Result<()> {
        let mut selectors = HashSet::new();
        let mut paths = HashSet::new();
        for skill in &self.indexed_skills {
            validate_skill_selector(&self.id, &skill.selector)?;
            validate_install_name(&self.id, &skill.name)?;
            validate_relative_path(&self.id, &skill.path, false)?;
            if !selectors.insert(skill.selector.as_str()) {
                return Err(registry_error(format!(
                    "skill source `{}` contains duplicate selector `{}`",
                    self.id, skill.selector
                )));
            }
            if !paths.insert(skill.path.as_str()) {
                return Err(registry_error(format!(
                    "skill source `{}` contains duplicate indexed path `{}`",
                    self.id, skill.path
                )));
            }
            if self
                .excluded_skills
                .iter()
                .any(|excluded| excluded == &skill.path)
            {
                return Err(registry_error(format!(
                    "skill source `{}` path `{}` is both indexed and excluded",
                    self.id, skill.path
                )));
            }
            if !self.directories.iter().any(|directory| {
                directory.installable && path_is_within(&skill.path, &directory.path)
            }) {
                return Err(registry_error(format!(
                    "skill source `{}` indexed path `{}` is outside an installable directory",
                    self.id, skill.path
                )));
            }
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
}

impl SkillDirectory {
    fn validate(&self, source: &SkillSource) -> Result<()> {
        validate_nonempty("directories.source_url", &self.source_url)?;
        validate_https_url(&source.id, "directories.source_url", &self.source_url)?;
        validate_relative_path(&source.id, &self.path, true)?;
        validate_directory_source_url(source, self)?;
        if !self.verified {
            return Err(registry_error(format!(
                "skill source `{}` directory `{}` must be marked verified",
                source.id, self.path
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogSkill {
    pub selector: String,
    pub name: String,
    pub path: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillRegistryFile {
    #[serde(default)]
    sources: Vec<SkillSource>,
}

fn registry_error(reason: impl Into<String>) -> StackError {
    StackError::RegistryLoad {
        reason: reason.into(),
    }
}

fn validate_nonempty(field: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(registry_error(format!(
            "skill registry field `{field}` is empty"
        )));
    }
    Ok(())
}

fn validate_github_url_matches(source: &SkillSource) -> Result<()> {
    let expected = format!("{GITHUB_URL_PREFIX}{}/{}", source.owner, source.repo);
    if source.url.trim_end_matches('/') == expected {
        return Ok(());
    }
    Err(registry_error(format!(
        "skill source `{}` url must match owner/repo `{}/{}`",
        source.id, source.owner, source.repo
    )))
}

fn validate_https_url(source_id: &str, field: &str, value: &str) -> Result<()> {
    if value.starts_with("https://") {
        return Ok(());
    }
    Err(registry_error(format!(
        "skill source `{source_id}` {field} entries must be HTTPS URLs"
    )))
}

fn validate_directory_source_url(source: &SkillSource, directory: &SkillDirectory) -> Result<()> {
    let mut expected = format!(
        "{}/tree/{}",
        source.url.trim_end_matches('/'),
        source.branch
    );
    if !directory.path.is_empty() {
        expected.push('/');
        expected.push_str(&directory.path);
    }
    if directory.source_url.trim_end_matches('/') == expected {
        return Ok(());
    }
    Err(registry_error(format!(
        "skill source `{}` directory `{}` source_url must match source url, branch, and path",
        source.id, directory.path
    )))
}

fn validate_commit_sha(source_id: &str, field: &str, commit: &str) -> Result<()> {
    if commit.len() == 40 && commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok(());
    }
    Err(registry_error(format!(
        "skill source `{source_id}` {field} must be a full 40-character hex SHA"
    )))
}

fn validate_catalog_name(source_id: &str, field: &str, name: &str) -> Result<()> {
    if is_catalog_name(name) {
        return Ok(());
    }
    Err(registry_error(format!(
        "skill source `{source_id}` {field} contains invalid name `{name}`"
    )))
}

fn validate_selector_list(source_id: &str, field: &str, selectors: &[String]) -> Result<()> {
    let mut seen = HashSet::new();
    for selector in selectors {
        validate_skill_selector(source_id, selector)?;
        if !seen.insert(selector.as_str()) {
            return Err(registry_error(format!(
                "skill source `{source_id}` {field} contains duplicate selector `{selector}`"
            )));
        }
    }
    Ok(())
}

fn validate_skill_selector(source_id: &str, selector: &str) -> Result<()> {
    if !selector.is_empty() && selector.split('/').all(is_catalog_name) {
        return Ok(());
    }
    Err(registry_error(format!(
        "skill source `{source_id}` contains invalid skill selector `{selector}`"
    )))
}

// `:` is allowed because reviewed sources ship frontmatter names such as
// `cocounsel-legal:deep-research` that install verbatim.
fn validate_install_name(source_id: &str, name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.split('/').all(|segment| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b':'))
                && segment
                    .bytes()
                    .next()
                    .is_some_and(|byte| byte.is_ascii_alphanumeric())
                && segment
                    .bytes()
                    .next_back()
                    .is_some_and(|byte| byte.is_ascii_alphanumeric())
        });
    if valid {
        return Ok(());
    }
    Err(registry_error(format!(
        "skill source `{source_id}` contains unsafe install name `{name}`"
    )))
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

fn validate_path_list(
    source_id: &str,
    field: &str,
    paths: &[String],
    allow_empty: bool,
) -> Result<()> {
    let mut seen = HashSet::new();
    for path in paths {
        validate_relative_path(source_id, path, allow_empty)?;
        if !seen.insert(path.as_str()) {
            return Err(registry_error(format!(
                "skill source `{source_id}` {field} contains duplicate path `{path}`"
            )));
        }
    }
    Ok(())
}

fn validate_relative_path(source_id: &str, value: &str, allow_empty: bool) -> Result<()> {
    if value.is_empty() {
        return if allow_empty {
            Ok(())
        } else {
            Err(registry_error(format!(
                "skill source `{source_id}` path must not be empty"
            )))
        };
    }
    let path = Path::new(value);
    if path.is_absolute() {
        return Err(registry_error(format!(
            "skill source `{source_id}` path must be relative"
        )));
    }
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            Component::CurDir => {
                return Err(registry_error(format!(
                    "skill source `{source_id}` path must not contain `.`"
                )));
            }
            Component::ParentDir => {
                return Err(registry_error(format!(
                    "skill source `{source_id}` path must not contain `..`"
                )));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(registry_error(format!(
                    "skill source `{source_id}` path must be relative"
                )));
            }
        }
    }
    if value
        .split('/')
        .any(|segment| segment.is_empty() || segment == ".")
    {
        return Err(registry_error(format!(
            "skill source `{source_id}` path contains an invalid segment"
        )));
    }
    Ok(())
}

fn path_is_within(path: &str, root: &str) -> bool {
    root.is_empty() || path == root || path.starts_with(&format!("{root}/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_body() -> &'static str {
        r#"
[[sources]]
id = "openai-skills"
alias = "openai"
name = "OpenAI Agent Skills"
owner = "openai"
repo = "skills"
url = "https://github.com/openai/skills"
docs = ["https://github.com/openai/skills"]
official = true
trusted = true
reviewed_at = "2026-07-13"
branch = "main"
verified_commit = "0123456789abcdef0123456789abcdef01234567"
descriptor = "SKILL.md"
discovery = "direct"
essential_skills = ["repo-map"]
excluded_skills = []
preferred_paths = []
indexed_skills = [
  { selector = "repo-map", name = "repo-map", path = "skills/.curated/repo-map" },
]

[[sources.directories]]
path = "skills/.curated"
source_url = "https://github.com/openai/skills/tree/main/skills/.curated"
verified = true
installable = true
"#
    }

    #[test]
    fn embedded_catalog_has_all_reviewed_sources() {
        let catalog = SkillCatalog::load_embedded().expect("embedded catalog");
        assert_eq!(catalog.sources().len(), 7);
        for (id, alias) in [
            ("anthropic-skills", "anthropic"),
            ("openai-skills", "openai"),
            ("openai-plugins", "openai-plugins"),
            ("anthropic-legal", "anthropic-legal"),
            ("anthropic-finance", "anthropic-finance"),
            ("anthropic-knowledge-work", "anthropic-knowledge-work"),
            ("k-dense-scientific", "k-dense-scientific"),
        ] {
            assert!(catalog.lookup(id).is_some(), "missing {id}");
            assert_eq!(
                catalog.lookup_alias(alias).map(|source| source.id.as_str()),
                Some(id)
            );
        }
        let scientific = catalog
            .lookup("k-dense-scientific")
            .expect("scientific source");
        assert!(scientific.trusted);
        assert!(!scientific.official);
    }

    #[test]
    fn embedded_catalog_preserves_reviewed_curation() {
        let catalog = SkillCatalog::load_embedded().expect("embedded catalog");
        let plugins = catalog.lookup("openai-plugins").expect("plugin source");
        assert_eq!(
            plugins.essential_skills,
            ["gh-address-comments", "gh-fix-ci", "github", "yeet"]
        );
        assert!(
            plugins
                .excluded_skills
                .contains(&"plugins/zoom/skills/start".to_owned())
        );
        assert!(
            plugins
                .indexed_skills
                .iter()
                .all(|skill| !plugins.excluded_skills.contains(&skill.path))
        );
        assert!(plugins.indexed_skills.iter().any(|skill| {
            skill.selector == "stripe-best-practices"
                && skill.path == "plugins/stripe/skills/stripe-best-practices"
        }));

        let legal = catalog.lookup("anthropic-legal").expect("legal source");
        assert!(
            legal
                .excluded_skills
                .contains(&"commercial-legal/skills/customize".to_owned())
        );
        let knowledge = catalog
            .lookup("anthropic-knowledge-work")
            .expect("knowledge source");
        assert!(knowledge.indexed_skills.iter().any(|skill| {
            skill.selector == "contact-center/android" && skill.name == "contact-center/android"
        }));
        assert!(
            knowledge
                .excluded_skills
                .contains(&"productivity/skills/start".to_owned())
        );

        for (source_id, selector, path) in [
            ("anthropic-skills", "docx", "skills/docx"),
            (
                "openai-skills",
                "openai-docs",
                "skills/.curated/openai-docs",
            ),
            (
                "anthropic-legal",
                "ai-inventory",
                "ai-governance-legal/skills/ai-inventory",
            ),
            (
                "anthropic-finance",
                "3-statement-model",
                "plugins/vertical-plugins/financial-analysis/skills/3-statement-model",
            ),
            ("k-dense-scientific", "astropy", "skills/astropy"),
        ] {
            let source = catalog.lookup(source_id).expect("representative source");
            assert!(
                source
                    .indexed_skills
                    .iter()
                    .any(|skill| { skill.selector == selector && skill.path == path })
            );
        }
        assert!(
            catalog
                .lookup("anthropic-skills")
                .expect("anthropic source")
                .excluded_skills
                .is_empty()
        );
        assert!(
            catalog
                .lookup("openai-skills")
                .expect("openai source")
                .excluded_skills
                .is_empty()
        );
        assert!(
            catalog
                .lookup("anthropic-finance")
                .expect("finance source")
                .excluded_skills
                .is_empty()
        );
        assert!(
            catalog
                .lookup("k-dense-scientific")
                .expect("scientific source")
                .excluded_skills
                .is_empty()
        );
    }

    #[test]
    fn looks_up_sources_by_cli_alias() {
        let catalog = SkillCatalog::from_toml(valid_body()).expect("catalog");
        assert_eq!(
            catalog
                .lookup_alias("openai")
                .map(|source| source.id.as_str()),
            Some("openai-skills")
        );
    }

    #[test]
    fn accepts_repository_root_discovery_directory() {
        let body = valid_body().replace(
            "path = \"skills/.curated\"\nsource_url = \"https://github.com/openai/skills/tree/main/skills/.curated\"",
            "path = \"\"\nsource_url = \"https://github.com/openai/skills/tree/main\"",
        );
        SkillCatalog::from_toml(&body).expect("root directory accepted");
    }

    #[test]
    fn accepts_path_qualified_selector() {
        let body = valid_body()
            .replace("selector = \"repo-map\"", "selector = \"plugin/repo-map\"")
            .replace(
                "essential_skills = [\"repo-map\"]",
                "essential_skills = [\"plugin/repo-map\"]",
            );
        SkillCatalog::from_toml(&body).expect("qualified selector accepted");
    }

    #[test]
    fn rejects_duplicate_aliases() {
        let body = format!(
            "{}\n{}",
            valid_body(),
            valid_body().replace("id = \"openai-skills\"", "id = \"other-id\"")
        );
        assert_reason(
            SkillCatalog::from_toml(&body).expect_err("duplicate alias"),
            "duplicate skill source alias",
        );
    }

    #[test]
    fn rejects_unapproved_repository() {
        let body = valid_body().replace("owner = \"openai\"", "owner = \"example\"");
        assert_reason(
            SkillCatalog::from_toml(&body).expect_err("unapproved repo"),
            "not an allowlisted repository",
        );
    }

    #[test]
    fn rejects_wrong_official_status() {
        let body = valid_body().replace("official = true", "official = false");
        assert_reason(
            SkillCatalog::from_toml(&body).expect_err("wrong official status"),
            "official status",
        );
    }

    #[test]
    fn rejects_ambiguous_or_missing_commit_pin() {
        let both = valid_body().replace(
            "verified_commit = \"0123456789abcdef0123456789abcdef01234567\"",
            "verified_commit = \"0123456789abcdef0123456789abcdef01234567\"\nindexed_commit = \"89abcdef0123456789abcdef0123456789abcdef\"",
        );
        assert_reason(
            SkillCatalog::from_toml(&both).expect_err("two commit pins"),
            "must not declare both",
        );

        let missing = valid_body().replace(
            "verified_commit = \"0123456789abcdef0123456789abcdef01234567\"\n",
            "",
        );
        assert_reason(
            SkillCatalog::from_toml(&missing).expect_err("missing commit pin"),
            "must pin",
        );
    }

    #[test]
    fn rejects_untrusted_source() {
        let body = valid_body().replace("trusted = true", "trusted = false");
        assert_reason(
            SkillCatalog::from_toml(&body).expect_err("untrusted"),
            "must be marked trusted",
        );
    }

    #[test]
    fn rejects_essential_selector_missing_from_index() {
        let body = valid_body().replace(
            "essential_skills = [\"repo-map\"]",
            "essential_skills = [\"missing\"]",
        );
        assert_reason(
            SkillCatalog::from_toml(&body).expect_err("missing essential"),
            "is not indexed",
        );
    }

    #[test]
    fn rejects_indexed_and_excluded_overlap() {
        let body = valid_body().replace(
            "excluded_skills = []",
            "excluded_skills = [\"skills/.curated/repo-map\"]",
        );
        assert_reason(
            SkillCatalog::from_toml(&body).expect_err("overlap"),
            "both indexed and excluded",
        );
    }

    #[test]
    fn rejects_unsafe_paths_and_selectors() {
        for body in [
            valid_body().replace(
                "path = \"skills/.curated/repo-map\"",
                "path = \"../repo-map\"",
            ),
            valid_body().replace("selector = \"repo-map\"", "selector = \"../repo-map\""),
        ] {
            assert!(SkillCatalog::from_toml(&body).is_err());
        }
    }

    #[test]
    fn rejects_unsupported_descriptor() {
        let body = valid_body().replace("descriptor = \"SKILL.md\"", "descriptor = \"README.md\"");
        assert_reason(
            SkillCatalog::from_toml(&body).expect_err("descriptor"),
            "descriptor must be `SKILL.md`",
        );
    }

    fn assert_reason(error: StackError, expected: &str) {
        match error {
            StackError::RegistryLoad { reason } => assert!(reason.contains(expected), "{reason}"),
            other => panic!("expected RegistryLoad, got {other:?}"),
        }
    }
}
