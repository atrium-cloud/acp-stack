//! Hand-curated catalog of ACP-speaking agents and their adapters.
//!
//! The embedded `data/agents.toml` is the runtime source of truth for
//! `acps agent install`. It supersedes the upstream
//! `cdn.agentclientprotocol.com/registry/v1/latest/registry.json` so the
//! runtime can make conservative support claims. The embedded catalog starts
//! with Goose, OpenCode, Cursor CLI, Amp, Pi, and Codex as verified headless
//! targets.
//! The schema supports entries that need both an ACP adapter and the upstream
//! harness it wraps.
//!
//! Operators can override entries or add private ones by placing a
//! `~/.config/acp-stack/agents.toml` file alongside the main config.
//! Override semantics are full-entry-by-id: an override with the same `id`
//! replaces the embedded entry; new `id`s are added.

use std::fs;
use std::path::{Component, Path};

use serde::Deserialize;

#[cfg(feature = "test-fixtures")]
use crate::dev_gates::{DEV_PLACEBO_REGISTRY_ENV, fixture_path};
use crate::error::{Result, StackError};

const EMBEDDED_REGISTRY: &str = include_str!("../../../data/agents.toml");
pub const LEGACY_PLACEHOLDER_AGENT_ID: &str = "placeholder";
#[cfg(feature = "test-fixtures")]
pub const DEV_PLACEBO_AGENT_ID: &str = "placebo";
#[cfg(feature = "test-fixtures")]
pub const DEV_PLACEBO_MODEL_OPTION: &str = "placebo-model";

#[cfg(feature = "test-fixtures")]
pub fn development_placebo_registry_path() -> Option<std::path::PathBuf> {
    let path = fixture_path(DEV_PLACEBO_REGISTRY_ENV)?;
    path.is_file().then_some(path)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryCatalog {
    agents: Vec<RegistryEntry>,
}

impl RegistryCatalog {
    /// Parse the binary-embedded registry. Surfaced as a fallible call so
    /// the compile-time `include_str!` failure is the only way to ship an
    /// invalid registry; runtime parse failures bubble up as
    /// `StackError::RegistryLoad` for tests that swap in alternate TOML.
    pub fn load_embedded() -> Result<Self> {
        Self::from_toml(EMBEDDED_REGISTRY)
    }

    /// Load the embedded registry, then layer an operator override file on
    /// top if it exists at `override_path`. A missing override file is not
    /// an error — it is the common case for fresh installs.
    pub fn load_with_override(override_path: &Path) -> Result<Self> {
        let mut catalog = Self::load_embedded()?;
        #[cfg(feature = "test-fixtures")]
        catalog.apply_development_placebo_registry();
        if override_path.exists() {
            let body =
                fs::read_to_string(override_path).map_err(|source| StackError::RegistryLoad {
                    reason: format!(
                        "failed to read operator override {}: {source}",
                        override_path.display()
                    ),
                })?;
            let overlay = Self::from_toml(&body)?;
            catalog.merge(overlay);
        }
        Ok(catalog)
    }

    #[cfg(feature = "test-fixtures")]
    fn apply_development_placebo_registry(&mut self) {
        let Some(path) = development_placebo_registry_path() else {
            return;
        };
        let placebo_path = path.display().to_string();
        let install = development_placebo_install(&placebo_path);
        for entry in &mut self.agents {
            entry.kind = RegistryKind::Native;
            entry.github = None;
            entry.adapter = None;
            entry.harness = Some(HarnessSpec {
                id: placebo_path.clone(),
                install: install.clone(),
            });
        }
        self.merge(RegistryCatalog {
            agents: vec![development_placebo_entry(&placebo_path, install)],
        });
    }

    pub fn from_toml(body: &str) -> Result<Self> {
        let parsed: RegistryFile =
            toml::from_str(body).map_err(|source| StackError::RegistryLoad {
                reason: format!("registry TOML is invalid: {source}"),
            })?;
        let catalog = Self {
            agents: parsed.agents,
        };
        catalog.validate()?;
        Ok(catalog)
    }

    pub fn lookup(&self, id: &str) -> Option<&RegistryEntry> {
        self.agents.iter().find(|entry| entry.id == id)
    }

    pub fn lookup_required(&self, id: &str) -> Result<&RegistryEntry> {
        if id == LEGACY_PLACEHOLDER_AGENT_ID {
            return Err(StackError::AgentPlaceholderConfigured);
        }
        self.lookup(id)
            .ok_or_else(|| StackError::AgentRegistryMissing { id: id.to_owned() })
    }

    pub fn entries(&self) -> &[RegistryEntry] {
        &self.agents
    }

    /// Full-entry replacement by id; new ids are appended. The override file
    /// is intentionally coarse: a partial-field merge would invite drift
    /// where an upstream rename silently kept an operator's stale harness.
    fn merge(&mut self, overlay: RegistryCatalog) {
        for entry in overlay.agents {
            match self.agents.iter().position(|e| e.id == entry.id) {
                Some(idx) => self.agents[idx] = entry,
                None => self.agents.push(entry),
            }
        }
    }

    fn validate(&self) -> Result<()> {
        for entry in &self.agents {
            match entry.kind {
                RegistryKind::Native => {
                    if entry.adapter.is_some() {
                        return Err(StackError::RegistryLoad {
                            reason: format!(
                                "agent `{}` is kind=\"native\" but declares [agents.adapter]",
                                entry.id
                            ),
                        });
                    }
                }
                RegistryKind::Adapter => {}
            }
            if entry.harness.is_none() {
                return Err(StackError::RegistryLoad {
                    reason: format!("agent `{}` has no [agents.harness] block", entry.id),
                });
            }
            if entry.headless_compatible
                && entry
                    .support_doc
                    .as_deref()
                    .is_none_or(|value| value.trim().is_empty())
            {
                return Err(StackError::RegistryLoad {
                    reason: format!(
                        "agent `{}` is headless-compatible but has no support_doc",
                        entry.id
                    ),
                });
            }
            if let Some(github) = &entry.github {
                github_url_from_value(&entry.id, "github", github)?;
            }
            if let Some(expect) = entry.testflight_expect_fs.as_deref() {
                validate_testflight_expect_fs(&entry.id, expect)?;
            }
            if let Some(prompt) = entry.testflight_prompt.as_deref()
                && prompt.trim().is_empty()
            {
                return Err(StackError::RegistryLoad {
                    reason: format!("agent `{}` testflight_prompt is empty", entry.id),
                });
            }
            if entry.supports_agent_skills {
                match entry.agent_skills_install_dir.as_deref() {
                    Some(value) => validate_agent_skills_install_dir(&entry.id, value)?,
                    _ => {
                        return Err(StackError::RegistryLoad {
                            reason: format!(
                                "agent `{}` supports Agent Skills but has no agent_skills_install_dir",
                                entry.id
                            ),
                        });
                    }
                }
            }
            let harness = entry.harness.as_ref().expect("validated harness presence");
            harness.validate(&entry.id, entry.github.as_deref())?;
            if entry.kind == RegistryKind::Adapter {
                let adapter = entry
                    .adapter
                    .as_ref()
                    .ok_or_else(|| StackError::RegistryLoad {
                        reason: format!(
                            "agent `{}` is kind=\"adapter\" but has no [agents.adapter] block",
                            entry.id
                        ),
                    })?;
                adapter.validate(&entry.id)?;
            }
        }
        let mut seen = std::collections::HashSet::new();
        for entry in &self.agents {
            if !seen.insert(entry.id.as_str()) {
                return Err(StackError::RegistryLoad {
                    reason: format!("duplicate registry id `{}`", entry.id),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryEntry {
    pub id: String,
    pub name: String,
    pub kind: RegistryKind,
    #[serde(default)]
    pub headless_compatible: bool,
    #[serde(default)]
    pub set_provider: bool,
    #[serde(default)]
    pub set_model: bool,
    #[serde(default)]
    pub allow_custom_provider: bool,
    #[serde(default)]
    pub allow_custom_model: bool,
    #[serde(default)]
    pub set_mode: bool,
    #[serde(default)]
    pub supports_mcp: bool,
    #[serde(default)]
    pub supports_agent_skills: bool,
    #[serde(default)]
    pub agent_skills_install_dir: Option<String>,
    #[serde(default)]
    pub subagents: bool,
    #[serde(default)]
    pub subagent_alias: Option<String>,
    /// Free auxiliary/subagent models exposed via `acps subagent free`. Order
    /// is significant for env-fallback resolution: the first entry whose
    /// canonical env ref is present in `[agent].env` wins when no provider id
    /// or main api_key_ref directly matches.
    #[serde(default)]
    pub subagent_free_models: Vec<SubagentFreeModel>,
    #[serde(default)]
    pub stdio_framing: RegistryStdioFraming,
    #[serde(default)]
    pub website: Option<String>,
    #[serde(default)]
    pub github: Option<String>,
    #[serde(default)]
    pub support_doc: Option<String>,
    /// Real-prompt smoke text sent during `acps agent test` / init testflight
    /// when the operator did not pass `--prompt`. Should be deterministic and
    /// cheap; for filesystem-tool-capable agents it should ask the agent to
    /// create the `testflight_expect_fs` path so the runtime can verify the
    /// agent actually did the work and did not just hallucinate a reply.
    #[serde(default)]
    pub testflight_prompt: Option<String>,
    /// Workspace-relative path the testflight prompt is expected to create
    /// (or modify). `acps agent test` resolves this against `workspace.root`
    /// and asserts the file exists with non-zero size after the prompt
    /// completes. `None` means the testflight only verifies session/prompt
    /// completion; useful for agents that don't expose filesystem tools.
    #[serde(default)]
    pub testflight_expect_fs: Option<String>,
    #[serde(default)]
    pub adapter: Option<AdapterSpec>,
    pub harness: Option<HarnessSpec>,
}

impl RegistryEntry {
    pub fn ensure_supported(&self) -> Result<()> {
        if self.headless_compatible {
            Ok(())
        } else {
            Err(StackError::AgentUnsupported {
                name: self.name.clone(),
            })
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RegistryKind {
    Native,
    Adapter,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentFreeModel {
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum RegistryStdioFraming {
    #[default]
    JsonLines,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessSpec {
    pub id: String,
    pub install: InstallSet,
}

impl HarnessSpec {
    fn validate(&self, agent_id: &str, github: Option<&str>) -> Result<()> {
        validate_nonempty(agent_id, "harness.id", &self.id)?;
        self.install.validate(agent_id, "harness.install", github)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdapterSpec {
    pub id: String,
    #[serde(default)]
    pub github: Option<String>,
    pub install: InstallSet,
}

impl AdapterSpec {
    fn validate(&self, agent_id: &str) -> Result<()> {
        validate_nonempty(agent_id, "adapter.id", &self.id)?;
        if let Some(github) = &self.github {
            github_url_from_value(agent_id, "adapter.github", github)?;
        }
        self.install
            .validate(agent_id, "adapter.install", self.github.as_deref())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallSet {
    #[serde(default)]
    pub shell: Option<ShellInstall>,
    #[serde(default)]
    pub npm: Option<NpmInstall>,
    #[serde(default)]
    pub github: Option<GithubInstall>,
}

impl InstallSet {
    pub fn is_empty(&self) -> bool {
        self.shell.is_none() && self.npm.is_none() && self.github.is_none()
    }

    fn validate(&self, agent_id: &str, field: &str, github_url: Option<&str>) -> Result<()> {
        if self.is_empty() {
            return Err(StackError::RegistryLoad {
                reason: format!("agent `{agent_id}` has no [{field}.shell|npm|github] path"),
            });
        }
        if let Some(shell) = &self.shell {
            shell.validate(agent_id, &format!("{field}.shell"))?;
        }
        if let Some(npm) = &self.npm {
            npm.validate(agent_id, &format!("{field}.npm"))?;
        }
        if let Some(github) = &self.github {
            if github_url.is_none_or(|value| value.trim().is_empty()) {
                return Err(StackError::RegistryLoad {
                    reason: format!("agent `{agent_id}` {field}.github requires github URL"),
                });
            }
            github.validate(agent_id, &format!("{field}.github"))?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShellInstall {
    pub script: String,
    pub creates: String,
    #[serde(default)]
    pub required_tools: Vec<String>,
}

impl ShellInstall {
    fn validate(&self, agent_id: &str, field: &str) -> Result<()> {
        validate_nonempty(agent_id, &format!("{field}.script"), &self.script)?;
        validate_nonempty(agent_id, &format!("{field}.creates"), &self.creates)?;
        for tool in &self.required_tools {
            validate_required_tool(agent_id, &format!("{field}.required_tools"), tool)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NpmInstall {
    pub package: String,
    pub creates: String,
}

impl NpmInstall {
    fn validate(&self, agent_id: &str, field: &str) -> Result<()> {
        validate_nonempty(agent_id, &format!("{field}.package"), &self.package)?;
        validate_nonempty(agent_id, &format!("{field}.creates"), &self.creates)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubInstall {
    pub asset_pattern: String,
    pub archive: ArchiveKind,
    #[serde(default)]
    pub archive_binary_name: Option<String>,
    pub binary_name: String,
    #[serde(default)]
    pub checksums_asset: Option<String>,
    #[serde(default)]
    pub arch: ArchMap,
}

impl GithubInstall {
    fn validate(&self, agent_id: &str, field: &str) -> Result<()> {
        validate_nonempty(
            agent_id,
            &format!("{field}.asset_pattern"),
            &self.asset_pattern,
        )?;
        if let Some(archive_binary_name) = &self.archive_binary_name {
            validate_nonempty(
                agent_id,
                &format!("{field}.archive_binary_name"),
                archive_binary_name,
            )?;
        }
        validate_nonempty(agent_id, &format!("{field}.binary_name"), &self.binary_name)?;
        if self.asset_pattern.contains("{arch}")
            || self
                .archive_binary_name
                .as_deref()
                .is_some_and(|name| name.contains("{arch}"))
        {
            self.arch.validate(agent_id, field)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchMap {
    #[serde(default)]
    pub x86_64: Option<String>,
    #[serde(default)]
    pub aarch64: Option<String>,
}

impl ArchMap {
    pub fn token_for_host(&self) -> Option<&str> {
        match std::env::consts::ARCH {
            "x86_64" => self.x86_64.as_deref(),
            "aarch64" => self.aarch64.as_deref(),
            _ => None,
        }
    }

    fn validate(&self, agent_id: &str, field: &str) -> Result<()> {
        let Some(x86_64) = self.x86_64.as_deref() else {
            return Err(StackError::RegistryLoad {
                reason: format!("agent `{agent_id}` {field}.arch.x86_64 is required"),
            });
        };
        validate_nonempty(agent_id, &format!("{field}.arch.x86_64"), x86_64)?;
        let Some(aarch64) = self.aarch64.as_deref() else {
            return Err(StackError::RegistryLoad {
                reason: format!("agent `{agent_id}` {field}.arch.aarch64 is required"),
            });
        };
        validate_nonempty(agent_id, &format!("{field}.arch.aarch64"), aarch64)
    }
}

/// Reject registry-declared testflight FS paths that would escape the
/// workspace root. `acps agent test` joins this onto `workspace.root`, so an
/// absolute path or one containing `..` would either bypass the workspace
/// (absolute) or traverse outside it (`..`). The intended use is a stable
/// in-workspace marker like `.acp-stack-testflight.txt`.
fn validate_testflight_expect_fs(agent_id: &str, value: &str) -> Result<()> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(StackError::RegistryLoad {
            reason: format!("agent `{agent_id}` testflight_expect_fs is empty"),
        });
    }
    if std::path::Path::new(trimmed).is_absolute() {
        return Err(StackError::RegistryLoad {
            reason: format!(
                "agent `{agent_id}` testflight_expect_fs `{trimmed}` must be workspace-relative, not absolute"
            ),
        });
    }
    if trimmed.split('/').any(|segment| segment == "..") {
        return Err(StackError::RegistryLoad {
            reason: format!(
                "agent `{agent_id}` testflight_expect_fs `{trimmed}` may not contain `..` segments"
            ),
        });
    }
    Ok(())
}

fn validate_agent_skills_install_dir(agent_id: &str, value: &str) -> Result<()> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(StackError::RegistryLoad {
            reason: format!("agent `{agent_id}` agent_skills_install_dir is empty"),
        });
    }
    if !(trimmed.starts_with("~/") || Path::new(trimmed).is_absolute()) {
        return Err(StackError::RegistryLoad {
            reason: format!(
                "agent `{agent_id}` agent_skills_install_dir `{trimmed}` must be absolute or start with `~/`"
            ),
        });
    }
    for component in Path::new(trimmed).components() {
        match component {
            Component::Normal(_) | Component::RootDir | Component::Prefix(_) => {}
            Component::CurDir | Component::ParentDir => {
                return Err(StackError::RegistryLoad {
                    reason: format!(
                        "agent `{agent_id}` agent_skills_install_dir `{trimmed}` contains an unsafe path segment"
                    ),
                });
            }
        }
    }
    Ok(())
}

fn validate_nonempty(agent_id: &str, field: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        Err(StackError::RegistryLoad {
            reason: format!("agent `{agent_id}` {field} is empty"),
        })
    } else {
        Ok(())
    }
}

fn validate_required_tool(agent_id: &str, field: &str, value: &str) -> Result<()> {
    validate_nonempty(agent_id, field, value)?;
    if value.contains('/') {
        return Err(StackError::RegistryLoad {
            reason: format!("agent `{agent_id}` {field} entry `{value}` must be a command name"),
        });
    }
    Ok(())
}

pub fn github_repo_from_url(agent_id: &str, field: &str, url: &str) -> Result<String> {
    let rest = github_path_from_value(agent_id, field, url)?;
    let mut parts = rest.split('/').filter(|part| !part.is_empty());
    let owner = parts.next().ok_or_else(|| StackError::RegistryLoad {
        reason: format!("agent `{agent_id}` {field} has no owner"),
    })?;
    let repo = parts.next().ok_or_else(|| StackError::RegistryLoad {
        reason: format!("agent `{agent_id}` {field} has no repo"),
    })?;
    Ok(format!("{owner}/{repo}"))
}

pub fn github_url_from_value(agent_id: &str, field: &str, value: &str) -> Result<String> {
    let rest = github_path_from_value(agent_id, field, value)?;
    Ok(format!("https://github.com/{}", rest.trim_matches('/')))
}

fn github_path_from_value<'a>(agent_id: &str, field: &str, value: &'a str) -> Result<&'a str> {
    let value = value.trim();
    if value.is_empty() {
        return Err(StackError::RegistryLoad {
            reason: format!("agent `{agent_id}` {field} is empty"),
        });
    }
    if let Some(rest) = value.strip_prefix("https://github.com/") {
        return Ok(rest);
    }
    if value.starts_with("http://") || value.starts_with("https://") {
        return Err(StackError::RegistryLoad {
            reason: format!(
                "agent `{agent_id}` {field} must be a GitHub path or https://github.com/ URL"
            ),
        });
    }
    Ok(value)
}

#[cfg(feature = "test-fixtures")]
fn development_placebo_install(placebo_path: &str) -> InstallSet {
    InstallSet {
        shell: Some(ShellInstall {
            script: format!("test -x {}", shell_quote_str(placebo_path)),
            creates: placebo_path.to_owned(),
            required_tools: Vec::new(),
        }),
        npm: None,
        github: None,
    }
}

#[cfg(feature = "test-fixtures")]
fn development_placebo_entry(placebo_path: &str, install: InstallSet) -> RegistryEntry {
    RegistryEntry {
        id: DEV_PLACEBO_AGENT_ID.to_owned(),
        name: "Placebo Agent".to_owned(),
        kind: RegistryKind::Native,
        headless_compatible: true,
        set_provider: false,
        set_model: false,
        allow_custom_provider: false,
        allow_custom_model: false,
        set_mode: false,
        supports_mcp: true,
        supports_agent_skills: false,
        agent_skills_install_dir: None,
        subagents: false,
        subagent_alias: None,
        subagent_free_models: Vec::new(),
        stdio_framing: RegistryStdioFraming::JsonLines,
        website: None,
        github: None,
        support_doc: Some("src/bin/placebo_agent/main.rs".to_owned()),
        testflight_prompt: None,
        testflight_expect_fs: None,
        adapter: None,
        harness: Some(HarnessSpec {
            id: placebo_path.to_owned(),
            install,
        }),
    }
}

#[cfg(feature = "test-fixtures")]
fn shell_quote_str(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveKind {
    None,
    #[serde(rename = "tar.gz")]
    TarGz,
    Zip,
}

#[derive(Debug, Deserialize)]
struct RegistryFile {
    #[serde(default)]
    agents: Vec<RegistryEntry>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_registry_parses() {
        let catalog = RegistryCatalog::load_embedded().expect("embedded registry must parse");
        assert!(
            !catalog.entries().is_empty(),
            "embedded registry must have at least one entry"
        );
    }

    #[test]
    fn lookup_returns_matching_entry() {
        let catalog = RegistryCatalog::load_embedded().expect("registry");
        let opencode = catalog
            .lookup("opencode")
            .expect("opencode must be present in the embedded registry");
        assert_eq!(opencode.kind, RegistryKind::Native);
        assert!(opencode.headless_compatible);
        assert!(opencode.set_provider);
        assert!(opencode.set_model);
        assert!(opencode.allow_custom_provider);
        assert!(opencode.allow_custom_model);
        assert!(opencode.set_mode);
        assert!(opencode.supports_mcp);
        assert!(opencode.supports_agent_skills);
        assert_eq!(
            opencode.agent_skills_install_dir.as_deref(),
            Some("~/.agents/skills")
        );
        assert!(opencode.subagents);
        assert_eq!(opencode.subagent_alias.as_deref(), Some("small_model"));
        assert_eq!(
            opencode.support_doc.as_deref(),
            Some("docs/agents/opencode.md")
        );
    }

    #[test]
    fn lookup_returns_none_for_unknown_id() {
        let catalog = RegistryCatalog::load_embedded().expect("registry");
        assert!(catalog.lookup("does-not-exist").is_none());
    }

    #[test]
    fn lookup_required_rejects_legacy_placeholder_config() {
        let catalog = RegistryCatalog::load_embedded().expect("registry");
        assert!(matches!(
            catalog.lookup_required(LEGACY_PLACEHOLDER_AGENT_ID),
            Err(StackError::AgentPlaceholderConfigured)
        ));
    }

    #[test]
    fn embedded_registry_advertises_smoked_headless_support() {
        let catalog = RegistryCatalog::load_embedded().expect("registry");
        let supported: Vec<_> = catalog
            .entries()
            .iter()
            .filter(|entry| entry.headless_compatible)
            .map(|entry| entry.id.as_str())
            .collect();
        assert_eq!(
            supported,
            ["opencode", "cursor", "amp", "pi", "goose", "codex"]
        );
        for entry in catalog
            .entries()
            .iter()
            .filter(|entry| entry.headless_compatible)
        {
            assert!(
                entry.supports_mcp,
                "{} must advertise MCP support",
                entry.id
            );
            assert!(
                entry.supports_agent_skills,
                "{} must advertise Agent Skills support",
                entry.id
            );
            assert!(
                entry
                    .agent_skills_install_dir
                    .as_deref()
                    .is_some_and(|path| {
                        matches!(entry.id.as_str(), "amp" if path == "~/.config/agents/skills")
                            || (entry.id != "amp" && path == "~/.agents/skills")
                    }),
                "{} must declare the documented Agent Skills install directory",
                entry.id
            );
            assert_eq!(
                entry.testflight_expect_fs.as_deref(),
                Some(".acp-stack-testflight.txt"),
                "{} must declare filesystem smoke output",
                entry.id
            );
            let prompt = entry
                .testflight_prompt
                .as_deref()
                .unwrap_or_else(|| panic!("{} must declare a testflight prompt", entry.id));
            assert!(
                prompt.contains(".acp-stack-testflight.txt"),
                "{} prompt must mention smoke output path",
                entry.id
            );
        }
    }

    #[test]
    fn embedded_registry_contains_only_curated_examples() {
        let catalog = RegistryCatalog::load_embedded().expect("registry");
        let ids: Vec<_> = catalog
            .entries()
            .iter()
            .map(|entry| entry.id.as_str())
            .collect();
        assert_eq!(ids, ["opencode", "cursor", "amp", "pi", "goose", "codex"]);
        let cursor = catalog.lookup("cursor").expect("cursor entry exists");
        assert_eq!(cursor.kind, RegistryKind::Native);
        assert!(cursor.headless_compatible);
        assert_eq!(cursor.stdio_framing, RegistryStdioFraming::JsonLines);
        assert!(!cursor.set_provider);
        assert!(cursor.set_model);
        assert!(!cursor.allow_custom_provider);
        assert!(!cursor.allow_custom_model);
        assert!(cursor.set_mode);
        assert_eq!(cursor.support_doc.as_deref(), Some("docs/agents/cursor.md"));
        let amp = catalog.lookup("amp").expect("amp entry exists");
        assert_eq!(amp.kind, RegistryKind::Adapter);
        assert!(amp.headless_compatible);
        assert!(!amp.set_provider);
        assert!(!amp.set_model);
        assert!(!amp.allow_custom_provider);
        assert!(!amp.allow_custom_model);
        assert!(amp.set_mode);
        assert_eq!(
            amp.adapter.as_ref().map(|adapter| adapter.id.as_str()),
            Some("amp-acp")
        );
        assert_eq!(
            amp.adapter
                .as_ref()
                .and_then(|adapter| adapter.github.as_deref()),
            Some("tao12345666333/amp-acp")
        );
        assert_eq!(amp.support_doc.as_deref(), Some("docs/agents/amp.md"));
        let pi = catalog.lookup("pi").expect("pi entry exists");
        assert_eq!(pi.kind, RegistryKind::Adapter);
        assert!(pi.headless_compatible);
        assert!(pi.set_provider);
        assert!(pi.set_model);
        assert!(pi.allow_custom_provider);
        assert!(pi.allow_custom_model);
        assert!(!pi.set_mode);
        assert_eq!(pi.stdio_framing, RegistryStdioFraming::JsonLines);
        let goose = catalog.lookup("goose").expect("goose entry exists");
        assert_eq!(goose.kind, RegistryKind::Native);
        assert!(goose.headless_compatible);
        assert!(goose.set_provider);
        assert!(goose.set_model);
        assert!(goose.allow_custom_provider);
        assert!(goose.allow_custom_model);
        assert!(!goose.set_mode);
        assert_eq!(goose.stdio_framing, RegistryStdioFraming::JsonLines);
        assert_eq!(goose.support_doc.as_deref(), Some("docs/agents/goose.md"));
        let codex = catalog.lookup("codex").expect("codex entry exists");
        assert_eq!(codex.kind, RegistryKind::Adapter);
        assert!(codex.headless_compatible);
        assert!(codex.set_provider);
        assert!(codex.set_model);
        assert!(codex.allow_custom_provider);
        assert!(codex.allow_custom_model);
        assert!(codex.set_mode);
        assert_eq!(
            codex.adapter.as_ref().map(|adapter| adapter.id.as_str()),
            Some("codex-acp")
        );
        let codex_adapter_install = &codex.adapter.as_ref().expect("codex adapter").install;
        assert_eq!(
            codex_adapter_install
                .npm
                .as_ref()
                .map(|install| install.package.as_str()),
            Some("@zed-industries/codex-acp")
        );
        let codex_harness_github = codex
            .harness
            .as_ref()
            .and_then(|harness| harness.install.github.as_ref())
            .expect("codex harness github install");
        assert_eq!(
            codex_harness_github.archive_binary_name.as_deref(),
            Some("codex-{arch}-unknown-linux-musl")
        );
        assert_eq!(codex.support_doc.as_deref(), Some("docs/agents/codex.md"));
    }

    #[test]
    fn embedded_registry_uses_per_install_arch_maps() {
        let catalog = RegistryCatalog::load_embedded().expect("registry");
        let opencode = catalog.lookup("opencode").expect("opencode entry exists");
        let opencode_github = opencode
            .harness
            .as_ref()
            .and_then(|harness| harness.install.github.as_ref())
            .expect("opencode github install");
        assert_eq!(opencode_github.arch.x86_64.as_deref(), Some("x64"));
        assert_eq!(opencode_github.arch.aarch64.as_deref(), Some("arm64"));

        let amp = catalog.lookup("amp").expect("amp entry exists");
        let amp_github = amp
            .adapter
            .as_ref()
            .and_then(|adapter| adapter.install.github.as_ref())
            .expect("amp-acp github install");
        assert_eq!(amp_github.arch.x86_64.as_deref(), Some("x86_64"));
        assert_eq!(amp_github.arch.aarch64.as_deref(), Some("aarch64"));

        let codex = catalog.lookup("codex").expect("codex entry exists");
        let codex_github = codex
            .adapter
            .as_ref()
            .and_then(|adapter| adapter.install.github.as_ref())
            .expect("codex-acp github install");
        assert_eq!(codex_github.arch.x86_64.as_deref(), Some("x86_64"));
        assert_eq!(codex_github.arch.aarch64.as_deref(), Some("aarch64"));
    }

    #[test]
    fn validate_rejects_legacy_registry_fields() {
        let body = r#"
[[agents]]
id = "bad"
name = "Bad"
kind = "native"
homepage = "https://example.com"
headless_doc = "docs/agents/bad.md"
source_url = "https://example.com/install"
upstream_id = "bad-upstream"
adapter_install = { type = "npx", package = "bad" }

[agents.harness]
id = "bad"

[agents.harness.install.npm]
package = "bad"
creates = "bad"
"#;
        let err = RegistryCatalog::from_toml(body).expect_err("must reject old fields");
        match err {
            StackError::RegistryLoad { reason } => {
                assert!(
                    reason.contains("unknown field") || reason.contains("unexpected keys"),
                    "reason: {reason}"
                );
            }
            other => panic!("expected RegistryLoad, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_required_tool_paths() {
        let body = r#"
[[agents]]
id = "bad"
name = "Bad"
kind = "native"
headless_compatible = true
support_doc = "docs/agents/bad.md"

[agents.harness]
id = "bad"

[agents.harness.install.shell]
script = "true"
creates = "bad"
required_tools = ["/usr/bin/curl"]
"#;
        let err = RegistryCatalog::from_toml(body).expect_err("must reject tool path");
        match err {
            StackError::RegistryLoad { reason } => {
                assert!(
                    reason.contains("must be a command name"),
                    "reason: {reason}"
                );
            }
            other => panic!("expected RegistryLoad, got {other:?}"),
        }
    }

    #[test]
    fn github_values_accept_path_shorthand_and_derive_repo() {
        assert_eq!(
            github_repo_from_url(
                "pi",
                "github",
                "earendil-works/pi/tree/main/packages/coding-agent"
            )
            .expect("repo"),
            "earendil-works/pi"
        );
        assert_eq!(
            github_url_from_value(
                "pi",
                "github",
                "earendil-works/pi/tree/main/packages/coding-agent"
            )
            .expect("url"),
            "https://github.com/earendil-works/pi/tree/main/packages/coding-agent"
        );
        assert_eq!(
            github_repo_from_url(
                "amp",
                "adapter.github",
                "https://github.com/tao12345666333/amp-acp"
            )
            .expect("repo"),
            "tao12345666333/amp-acp"
        );
    }

    #[test]
    fn validate_rejects_adapter_without_harness() {
        let body = r#"
[[agents]]
id = "bad"
name = "Bad"
kind = "adapter"

[agents.adapter]
id = "bad-adapter"

[agents.adapter.install.npm]
package = "bad"
creates = "bad"
"#;
        let err =
            RegistryCatalog::from_toml(body).expect_err("must reject adapter without harness");
        match err {
            StackError::RegistryLoad { reason } => {
                assert!(reason.contains("[agents.harness]"), "reason: {reason}");
            }
            other => panic!("expected RegistryLoad, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_native_with_adapter_install() {
        let body = r#"
[[agents]]
id = "bad"
name = "Bad"
kind = "native"

[agents.harness]
id = "bad"

[agents.harness.install.npm]
package = "bad"
creates = "bad"

[agents.adapter]
id = "adapter"

[agents.adapter.install.npm]
package = "adapter"
creates = "adapter"
"#;
        let err = RegistryCatalog::from_toml(body).expect_err("must reject native with adapter");
        match err {
            StackError::RegistryLoad { reason } => {
                assert!(reason.contains("[agents.adapter]"), "reason: {reason}");
            }
            other => panic!("expected RegistryLoad, got {other:?}"),
        }
    }

    #[test]
    fn parses_optional_testflight_smoke_fields() {
        let body = r#"
[[agents]]
id = "smoke-agent"
name = "Smoke Agent"
kind = "native"
headless_compatible = true
support_doc = "docs/agents/smoke-agent.md"
testflight_prompt = "Create /workspace/.acp-stack-testflight.txt with text 'ok'"
testflight_expect_fs = ".acp-stack-testflight.txt"

[agents.harness]
id = "smoke-agent"

[agents.harness.install.npm]
package = "smoke-agent"
creates = "smoke-agent"
"#;
        let catalog = RegistryCatalog::from_toml(body).expect("registry should parse");
        let entry = catalog.lookup("smoke-agent").expect("entry exists");
        assert_eq!(
            entry.testflight_prompt.as_deref(),
            Some("Create /workspace/.acp-stack-testflight.txt with text 'ok'")
        );
        assert_eq!(
            entry.testflight_expect_fs.as_deref(),
            Some(".acp-stack-testflight.txt")
        );
    }

    #[test]
    fn validate_rejects_absolute_testflight_expect_fs() {
        let body = r#"
[[agents]]
id = "bad-expect"
name = "Bad Expect"
kind = "native"
headless_compatible = true
support_doc = "docs/agents/bad-expect.md"
testflight_expect_fs = "/etc/passwd"

[agents.harness]
id = "bad-expect"

[agents.harness.install.npm]
package = "bad-expect"
creates = "bad-expect"
"#;
        let err = RegistryCatalog::from_toml(body)
            .expect_err("absolute testflight_expect_fs must be rejected");
        match err {
            StackError::RegistryLoad { reason } => {
                assert!(
                    reason.contains("must be workspace-relative"),
                    "reason: {reason}"
                );
            }
            other => panic!("expected RegistryLoad, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_testflight_expect_fs_with_parent_segment() {
        let body = r#"
[[agents]]
id = "bad-expect"
name = "Bad Expect"
kind = "native"
headless_compatible = true
support_doc = "docs/agents/bad-expect.md"
testflight_expect_fs = "subdir/../escape.txt"

[agents.harness]
id = "bad-expect"

[agents.harness.install.npm]
package = "bad-expect"
creates = "bad-expect"
"#;
        let err = RegistryCatalog::from_toml(body)
            .expect_err("testflight_expect_fs with `..` must be rejected");
        match err {
            StackError::RegistryLoad { reason } => {
                assert!(reason.contains("`..`"), "reason: {reason}");
            }
            other => panic!("expected RegistryLoad, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_empty_testflight_prompt() {
        let body = r#"
[[agents]]
id = "bad-prompt"
name = "Bad Prompt"
kind = "native"
headless_compatible = true
support_doc = "docs/agents/bad-prompt.md"
testflight_prompt = "   "

[agents.harness]
id = "bad-prompt"

[agents.harness.install.npm]
package = "bad-prompt"
creates = "bad-prompt"
"#;
        let err =
            RegistryCatalog::from_toml(body).expect_err("empty testflight_prompt must be rejected");
        match err {
            StackError::RegistryLoad { reason } => {
                assert!(reason.contains("testflight_prompt"), "reason: {reason}");
            }
            other => panic!("expected RegistryLoad, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_duplicate_ids() {
        let body = r#"
[[agents]]
id = "dup"
name = "First"
kind = "native"

[agents.harness]
id = "first"

[agents.harness.install.npm]
package = "first"
creates = "first"

[[agents]]
id = "dup"
name = "Second"
kind = "native"

[agents.harness]
id = "second"

[agents.harness.install.npm]
package = "second"
creates = "second"
"#;
        let err = RegistryCatalog::from_toml(body).expect_err("must reject duplicate ids");
        match err {
            StackError::RegistryLoad { reason } => {
                assert!(reason.contains("duplicate"), "reason: {reason}");
            }
            other => panic!("expected RegistryLoad, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_headless_entry_without_doc() {
        let body = r#"
[[agents]]
id = "bad"
name = "Bad"
kind = "native"
headless_compatible = true

[agents.harness]
id = "bad"

[agents.harness.install.npm]
package = "bad"
creates = "bad"
"#;
        let err = RegistryCatalog::from_toml(body)
            .expect_err("must reject headless-compatible entry without doc");
        match err {
            StackError::RegistryLoad { reason } => {
                assert!(reason.contains("support_doc"), "reason: {reason}");
            }
            other => panic!("expected RegistryLoad, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_agent_skills_support_without_install_dir() {
        let body = r#"
[[agents]]
id = "bad-skills"
name = "Bad Skills"
kind = "native"
headless_compatible = true
supports_agent_skills = true
support_doc = "docs/agents/bad-skills.md"

[agents.harness]
id = "bad-skills"

[agents.harness.install.npm]
package = "bad-skills"
creates = "bad-skills"
"#;
        let err = RegistryCatalog::from_toml(body)
            .expect_err("skills support without install dir must be rejected");
        match err {
            StackError::RegistryLoad { reason } => {
                assert!(
                    reason.contains("agent_skills_install_dir"),
                    "reason: {reason}"
                );
            }
            other => panic!("expected RegistryLoad, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_invalid_agent_skills_install_dir() {
        let body = r#"
[[agents]]
id = "bad-skills"
name = "Bad Skills"
kind = "native"
headless_compatible = true
supports_agent_skills = true
agent_skills_install_dir = "relative/skills"
support_doc = "docs/agents/bad-skills.md"

[agents.harness]
id = "bad-skills"

[agents.harness.install.npm]
package = "bad-skills"
creates = "bad-skills"
"#;
        let err =
            RegistryCatalog::from_toml(body).expect_err("relative install dir must be rejected");
        match err {
            StackError::RegistryLoad { reason } => {
                assert!(reason.contains("must be absolute"), "reason: {reason}");
            }
            other => panic!("expected RegistryLoad, got {other:?}"),
        }
    }

    #[test]
    fn override_replaces_entry_by_id() {
        let base = RegistryCatalog::load_embedded().expect("registry");
        let overlay_body = r#"
[[agents]]
id = "opencode"
name = "OpenCode (private fork)"
kind = "native"
support_doc = "docs/agents/opencode.md"

[agents.harness]
id = "opencode"

[agents.harness.install.npm]
package = "@private/opencode"
creates = "opencode"
"#;
        let overlay = RegistryCatalog::from_toml(overlay_body).expect("overlay parses");
        let mut catalog = base;
        catalog.merge(overlay);
        let entry = catalog.lookup("opencode").expect("entry exists");
        assert_eq!(entry.kind, RegistryKind::Native);
        assert_eq!(entry.name, "OpenCode (private fork)");
    }
}
