//! Hand-curated catalog of ACP-speaking agents and their adapters.
//!
//! The embedded `data/agents.toml` is the runtime source of truth for
//! `acps agent install`. It supersedes the upstream
//! `cdn.agentclientprotocol.com/registry/v1/latest/registry.json` so the
//! runtime can make conservative support claims. The embedded catalog includes
//! Goose, OpenCode, Cursor CLI, Amp, Pi, Codex, Claude Code, and Kimi Code as
//! curated headless targets.
//! The schema supports entries that need both an ACP adapter and the upstream
//! harness it wraps.
//!
//! Operators can override entries or add private ones by placing a
//! `~/.config/acp-stack/agents.toml` file alongside the main config.
//! Override semantics are full-entry-by-id: an override with the same `id`
//! replaces the embedded entry; new `id`s are added.

mod specs;

use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

#[cfg(feature = "test-fixtures")]
use crate::dev_gates::{DEV_PLACEBO_REGISTRY_ENV, fixture_path};
use crate::error::{Result, StackError};

pub use self::specs::*;

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
                update: Default::default(),
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
                    Some(value) => {
                        validate_agent_skills_dir(&entry.id, "agent_skills_install_dir", value)?;
                    }
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
            if let Some(link_dir) = entry.agent_skills_link_dir.as_deref() {
                if !entry.supports_agent_skills {
                    return Err(StackError::RegistryLoad {
                        reason: format!(
                            "agent `{}` declares agent_skills_link_dir without supports_agent_skills",
                            entry.id
                        ),
                    });
                }
                validate_agent_skills_dir(&entry.id, "agent_skills_link_dir", link_dir)?;
                if let Some(install_dir) = entry.agent_skills_install_dir.as_deref() {
                    // Compare component-wise so spellings like `~/.agents//skills`
                    // or a trailing slash cannot disguise an equal or nested path.
                    let install_path: PathBuf =
                        Path::new(install_dir.trim()).components().collect();
                    let link_path: PathBuf = Path::new(link_dir.trim()).components().collect();
                    if link_path.starts_with(&install_path) || install_path.starts_with(&link_path)
                    {
                        return Err(StackError::RegistryLoad {
                            reason: format!(
                                "agent `{}` agent_skills_link_dir must differ from agent_skills_install_dir and neither may nest within the other",
                                entry.id
                            ),
                        });
                    }
                }
            }
            let harness = entry.harness.as_ref().expect("validated harness presence");
            harness.validate(&entry.id, entry.github.as_deref())?;
            if entry.kind == RegistryKind::Native && harness.install.is_provided_by_adapter() {
                return Err(StackError::RegistryLoad {
                    reason: format!(
                        "agent `{}` is kind=\"native\" but declares harness.install.provided_by = \"adapter\"",
                        entry.id
                    ),
                });
            }
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
    pub multiple_active_providers: bool,
    #[serde(default)]
    pub set_model: bool,
    #[serde(default)]
    pub allow_custom_provider: bool,
    #[serde(default)]
    pub allow_custom_model: bool,
    #[serde(default)]
    pub set_mode: bool,
    #[serde(default)]
    pub supports_agent_skills: bool,
    #[serde(default)]
    pub agent_skills_install_dir: Option<String>,
    /// Directory the harness actually discovers skills from when it differs
    /// from the shared install dir; each installed skill gets a symlink here
    /// (e.g. Claude Code only reads `~/.claude/skills`).
    #[serde(default)]
    pub agent_skills_link_dir: Option<String>,
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
    /// Real-prompt text sent during `acps agent test` / init testflight
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

fn validate_agent_skills_dir(agent_id: &str, field: &str, value: &str) -> Result<()> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(StackError::RegistryLoad {
            reason: format!("agent `{agent_id}` {field} is empty"),
        });
    }
    if !(trimmed.starts_with("~/") || Path::new(trimmed).is_absolute()) {
        return Err(StackError::RegistryLoad {
            reason: format!(
                "agent `{agent_id}` {field} `{trimmed}` must be absolute or start with `~/`"
            ),
        });
    }
    for component in Path::new(trimmed).components() {
        match component {
            Component::Normal(_) | Component::RootDir | Component::Prefix(_) => {}
            Component::CurDir | Component::ParentDir => {
                return Err(StackError::RegistryLoad {
                    reason: format!(
                        "agent `{agent_id}` {field} `{trimmed}` contains an unsafe path segment"
                    ),
                });
            }
        }
    }
    Ok(())
}

#[cfg(feature = "test-fixtures")]
fn development_placebo_install(placebo_path: &str) -> InstallSet {
    InstallSet {
        shell: Some(ShellInstall {
            script: format!("test -x {}", shell_quote_str(placebo_path)),
            creates: placebo_path.to_owned(),
            required_tools: Vec::new(),
        }),
        ..InstallSet::default()
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
        multiple_active_providers: false,
        set_model: false,
        allow_custom_provider: false,
        allow_custom_model: false,
        set_mode: false,
        supports_agent_skills: false,
        agent_skills_install_dir: None,
        agent_skills_link_dir: None,
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
            update: Default::default(),
        }),
    }
}

#[cfg(feature = "test-fixtures")]
fn shell_quote_str(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[derive(Debug, Deserialize)]
struct RegistryFile {
    #[serde(default)]
    agents: Vec<RegistryEntry>,
}

#[cfg(test)]
mod tests;
