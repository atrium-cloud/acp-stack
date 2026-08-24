//! Hand-curated catalog of ACP-speaking agents and their adapters, embedded from
//! `data/agents.toml` and overridable via `~/.config/acp-stack/agents.toml`.

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
    /// Parse the binary-embedded registry.
    pub fn load_embedded() -> Result<Self> {
        Self::from_toml(EMBEDDED_REGISTRY)
    }

    /// Load the embedded registry, then layer the operator override file at
    /// `override_path` on top if it exists.
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
                acp_args: default_acp_args(),
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

    /// Whether the agent's native config has a per-provider endpoint field
    /// acp-stack can write; an agent absent from the catalog answers false.
    pub fn supports_provider_base_url(&self, id: &str) -> bool {
        self.lookup(id)
            .is_some_and(|entry| entry.set_provider_base_url)
    }

    /// Full-entry replacement by id; new ids are appended. Deliberately coarse:
    /// a partial-field merge would let an upstream rename silently keep an
    /// operator's stale harness.
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
            if let Some(sync_id) = entry.sync_id.as_deref()
                && (sync_id.is_empty() || sync_id.trim() != sync_id)
            {
                return Err(StackError::RegistryLoad {
                    reason: format!(
                        "agent `{}` sync_id is empty or has surrounding whitespace",
                        entry.id
                    ),
                });
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
    /// Catalog-declared kind. Lanes branching on this MUST resolve through
    /// [`effective_registry_entry`] first: an `[agent.adapter_override]` block
    /// rewrites the effective kind to `Adapter`.
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
    /// The agent's native config has a per-provider endpoint field acp-stack can write.
    #[serde(default)]
    pub set_provider_base_url: bool,
    #[serde(default)]
    pub allow_custom_model: bool,
    #[serde(default)]
    pub set_mode: bool,
    #[serde(default)]
    pub set_effort: bool,
    #[serde(default)]
    pub supports_agent_skills: bool,
    #[serde(default)]
    pub agent_skills_install_dir: Option<String>,
    /// Directory the harness discovers skills from when it differs from the
    /// shared install dir; each installed skill gets a symlink here.
    #[serde(default)]
    pub agent_skills_link_dir: Option<String>,
    #[serde(default)]
    pub subagents: bool,
    #[serde(default)]
    pub subagent_alias: Option<String>,
    /// Free auxiliary/subagent models exposed via `acps subagent free`. ORDER IS
    /// SIGNIFICANT: the first entry whose canonical env ref is present in
    /// `[agent].env` wins the env-fallback resolution.
    #[serde(default)]
    pub subagent_free_models: Vec<SubagentFreeModel>,
    /// Maintainer-only escape hatch for `sync-registry-check` when the upstream
    /// ACP registry index does not list this agent yet; no runtime effect.
    #[serde(default)]
    pub sync_exempt: bool,
    /// Upstream ACP registry id when it differs from the catalog id (which
    /// follows the installed binary name). Sync/fact-check binaries only.
    #[serde(default)]
    pub sync_id: Option<String>,
    #[serde(default)]
    pub stdio_framing: RegistryStdioFraming,
    #[serde(default)]
    pub website: Option<String>,
    #[serde(default)]
    pub github: Option<String>,
    #[serde(default)]
    pub support_doc: Option<String>,
    /// Default prompt for `acps agent test` / init testflight.
    #[serde(default)]
    pub testflight_prompt: Option<String>,
    /// Workspace-relative path the testflight prompt is expected to create, so
    /// the runtime can verify the agent did the work rather than hallucinating a reply.
    #[serde(default)]
    pub testflight_expect_fs: Option<String>,
    /// Catalog-declared adapter. Like `kind`, read through
    /// [`effective_registry_entry`] so operator overrides are honored.
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

/// Reject testflight FS paths that would escape `workspace.root` once joined:
/// absolute paths bypass it, `..` segments traverse out of it.
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
            timeout_secs: None,
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
        set_provider_base_url: false,
        allow_custom_model: false,
        set_mode: false,
        set_effort: false,
        supports_agent_skills: false,
        agent_skills_install_dir: None,
        agent_skills_link_dir: None,
        subagents: false,
        subagent_alias: None,
        subagent_free_models: Vec::new(),
        sync_exempt: false,
        sync_id: None,
        stdio_framing: RegistryStdioFraming::JsonLines,
        website: None,
        github: None,
        support_doc: Some("src/bin/placebo_agent/main.rs".to_owned()),
        testflight_prompt: None,
        testflight_expect_fs: None,
        adapter: None,
        harness: Some(HarnessSpec {
            id: placebo_path.to_owned(),
            acp_args: default_acp_args(),
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

/// Build a registry `AdapterSpec` from an operator `[agent.adapter_override]`
/// block, validated exactly as a curated catalog adapter would be.
pub fn adapter_spec_from_override(
    agent_id: &str,
    override_config: &crate::config::AgentAdapterOverrideConfig,
) -> Result<AdapterSpec> {
    let spec = AdapterSpec {
        id: override_config.command.trim().to_owned(),
        sync_id: None,
        github: override_config.github.clone(),
        install: InstallSet {
            provided_by: None,
            shell: override_config
                .install
                .shell
                .as_ref()
                .map(|shell| ShellInstall {
                    script: shell.script.clone(),
                    creates: shell.creates.clone(),
                    required_tools: shell.required_tools.clone(),
                    timeout_secs: shell.timeout_secs,
                }),
            npm: override_config.install.npm.as_ref().map(|npm| NpmInstall {
                package: npm.package.clone(),
                creates: npm.creates.clone(),
            }),
            github: override_config
                .install
                .github
                .as_ref()
                .map(|github| GithubInstall {
                    asset_pattern: github.asset_pattern.clone(),
                    archive: match github.archive {
                        crate::config::AgentAdapterOverrideArchiveKind::None => ArchiveKind::None,
                        crate::config::AgentAdapterOverrideArchiveKind::TarGz => ArchiveKind::TarGz,
                        crate::config::AgentAdapterOverrideArchiveKind::Zip => ArchiveKind::Zip,
                    },
                    archive_binary_name: github.archive_binary_name.clone(),
                    binary_name: github.binary_name.clone(),
                    checksums_asset: github.checksums_asset.clone(),
                    arch: ArchMap {
                        x86_64: github.arch.x86_64.clone(),
                        aarch64: github.arch.aarch64.clone(),
                    },
                }),
        },
        update: UpdateSet {
            apt: None,
            shell_rerun: override_config.update.shell_rerun,
        },
    };
    spec.validate(agent_id).map_err(|error| match error {
        StackError::RegistryLoad { reason } => StackError::RegistryLoad {
            reason: format!("[agent.adapter_override] {reason}"),
        },
        other => other,
    })?;
    Ok(spec)
}

/// Resolve the registry entry the install/update/version-check/metadata lanes
/// should drive for `agent`, applying any `[agent.adapter_override]` on top.
pub fn effective_registry_entry<'a>(
    entry: &'a RegistryEntry,
    agent: &crate::config::AgentConfig,
) -> Result<std::borrow::Cow<'a, RegistryEntry>> {
    let Some(override_config) = agent.adapter_override.as_ref() else {
        return Ok(std::borrow::Cow::Borrowed(entry));
    };
    if agent.id != entry.id {
        return Ok(std::borrow::Cow::Borrowed(entry));
    }
    let mut effective = entry.clone();
    effective.kind = RegistryKind::Adapter;
    effective.adapter = Some(adapter_spec_from_override(&entry.id, override_config)?);
    Ok(std::borrow::Cow::Owned(effective))
}

#[cfg(test)]
mod tests;
