use crate::config::{
    AgentAutoUpdateConfig, AgentInstallConfig, CloudflareEdgeConfig, Config,
    DEFAULT_AGENT_AUTO_UPDATE_FREQUENCY, DependencyEntry, EdgeConfig,
};
use crate::error::{Result, StackError};
use crate::runtime::agent::provider_keys::env_refs_for_agent_id;
use crate::runtime::install::agent_registry::{RegistryCatalog, RegistryEntry, RegistryKind};

use super::{
    CloudflaredDeploymentArg, EdgeExposureArg, EdgeProviderArg, InitArgs, STARTER_AGENT_ID, prompt,
    prompts_enabled,
};

pub(super) fn apply_edge_profile_to_config(args: &InitArgs, config: &mut Config) -> Result<bool> {
    let Some(edge) = args.edge else {
        return Ok(false);
    };
    match edge {
        EdgeProviderArg::Cloudflare => {}
    }
    if !matches!(args.exposure, Some(EdgeExposureArg::Tunnel)) {
        return Err(StackError::MissingField {
            field: "--exposure tunnel",
        });
    }
    let hostname = args
        .hostname
        .as_ref()
        .ok_or(StackError::MissingField {
            field: "--hostname",
        })?
        .trim()
        .to_owned();
    if args.cloudflare_mode.as_config_value() == "managed" {
        if args.cloudflare_api_token_ref.is_none() {
            return Err(StackError::MissingField {
                field: "--cloudflare-api-token-ref",
            });
        }
        if args.cloudflare_account_id_ref.is_none() {
            return Err(StackError::MissingField {
                field: "--cloudflare-account-id-ref",
            });
        }
    }
    let public_url = format!("https://{hostname}");
    config.api.bind = "127.0.0.1:7700".to_owned();
    config.api.public_url = Some(public_url.clone());
    config.security.http.allowed_origins = vec![public_url];
    config.security.http.trust_proxy_headers = true;
    config.security.http.trusted_proxies = vec!["127.0.0.1".to_owned(), "::1".to_owned()];
    config.edge = EdgeConfig {
        cloudflare: Some(CloudflareEdgeConfig {
            enabled: true,
            mode: args.cloudflare_mode.as_config_value().to_owned(),
            exposure: "tunnel".to_owned(),
            hostname,
            api_token_ref: args.cloudflare_api_token_ref.clone(),
            account_id_ref: args.cloudflare_account_id_ref.clone(),
            tunnel_name: Some("acp-stack".to_owned()),
            tunnel_id: None,
            cloudflared_deployment: args.cloudflared_deployment.as_config_value().to_owned(),
        }),
    };
    if matches!(args.cloudflared_deployment, CloudflaredDeploymentArg::Host)
        && !config
            .dependencies
            .commands
            .iter()
            .any(|entry| entry.name == "cloudflared")
    {
        config.dependencies.commands.push(DependencyEntry {
            name: "cloudflared".to_owned(),
            required: true,
            feature: Some("cloudflare-tunnel".to_owned()),
            install: None,
        });
    }
    Ok(true)
}

/// An operator-declared agent that is not in the embedded registry. It is
/// modeled entirely with existing config: `[agent].id/name/command/args` plus
/// the `[agent.install]` shell escape hatch. An adapter-backed custom agent
/// uses the same shape — `command`/`args` point at the adapter binary and
/// `install_shell` installs the harness and the adapter together.
#[derive(Debug, Clone)]
pub(super) struct CustomAgentSpec {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) command: String,
    pub(super) args: Vec<String>,
    pub(super) install_shell: String,
    pub(super) creates: String,
}

/// Outcome of agent selection: a curated registry agent, or a custom one.
pub(super) enum AgentSelection<'a> {
    Registry(&'a RegistryEntry),
    Custom(CustomAgentSpec),
}

/// True when the on-disk config points at an agent the registry does not know
/// but carries an `[agent.install]` escape hatch — i.e. an operator-declared
/// custom agent. Used to bypass the registry-only gates (install support
/// checks, provider/model auto-config) that only make sense for curated agents.
pub(super) fn is_custom_agent(config: &Config, registry: &RegistryCatalog) -> bool {
    config.agent.install.is_some() && registry.lookup(&config.agent.id).is_none()
}

/// Assemble a custom-agent spec from the `--custom-agent-*` flags. Returns
/// `None` when no custom agent was requested. `--custom-agent-id` is the anchor
/// flag; `command` and `install` are mandatory, `name` defaults to the id and
/// `creates` defaults to the command.
pub(super) fn resolve_custom_agent_spec(args: &InitArgs) -> Result<Option<CustomAgentSpec>> {
    let Some(raw_id) = args.custom_agent_id.as_deref() else {
        return Ok(None);
    };
    let id = raw_id.trim().to_owned();
    validate_custom_agent_id(&id)?;
    let command = require_custom_flag(
        "--custom-agent-command",
        args.custom_agent_command.as_deref(),
    )?;
    let install_shell = require_custom_flag(
        "--custom-agent-install",
        args.custom_agent_install.as_deref(),
    )?;
    let name = args
        .custom_agent_name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| id.clone());
    let creates = args
        .custom_agent_creates
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| command.clone());
    Ok(Some(CustomAgentSpec {
        id,
        name,
        command,
        args: args.custom_agent_arg.clone(),
        install_shell,
        creates,
    }))
}

fn require_custom_flag(field: &'static str, value: Option<&str>) -> Result<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or(StackError::MissingField { field })
}

fn validate_custom_agent_id(id: &str) -> Result<()> {
    if id.is_empty() {
        return Err(StackError::MissingField {
            field: "--custom-agent-id",
        });
    }
    if id == STARTER_AGENT_ID {
        return Err(StackError::InvalidParam {
            field: "custom-agent-id",
            reason: format!("`{STARTER_AGENT_ID}` is reserved for the starter placeholder agent"),
        });
    }
    Ok(())
}

pub(super) fn reject_registry_id_for_custom_agent(
    id: &str,
    registry: &RegistryCatalog,
) -> Result<()> {
    if registry.lookup(id).is_some() {
        return Err(StackError::InvalidParam {
            field: "--custom-agent-id",
            reason: format!(
                "`{id}` is a supported registry agent; use `--agent {id}` to follow the supported agent setup flow"
            ),
        });
    }
    Ok(())
}

pub(super) fn select_agent_for_init<'a>(
    args: &InitArgs,
    registry: &'a RegistryCatalog,
) -> Result<Option<AgentSelection<'a>>> {
    if let Some(spec) = resolve_custom_agent_spec(args)? {
        reject_registry_id_for_custom_agent(&spec.id, registry)?;
        return Ok(Some(AgentSelection::Custom(spec)));
    }
    if let Some(id) = &args.agent {
        return registry
            .lookup_required(id)
            .map(|entry| Some(AgentSelection::Registry(entry)));
    }
    if !prompts_enabled(args) {
        return Ok(None);
    }
    let entries = registry.entries();
    if entries.is_empty() {
        return Ok(None);
    }

    #[derive(Clone, PartialEq, Eq)]
    enum AgentChoice {
        Id(String),
        Custom,
        Skip,
    }
    let mut items = entries
        .iter()
        .map(|entry| {
            (
                AgentChoice::Id(entry.id.clone()),
                format!("{} ({})", entry.name, entry.id),
                String::new(),
            )
        })
        .collect::<Vec<_>>();
    items.push((
        AgentChoice::Custom,
        "Custom agent…".to_owned(),
        "not in the registry".to_owned(),
    ));
    items.push((AgentChoice::Skip, "Skip".to_owned(), String::new()));
    let Some(choice) = prompt::searchable_select(prompts_enabled(args), "Agent", &items)? else {
        return Ok(None);
    };
    match choice {
        AgentChoice::Skip => Ok(None),
        AgentChoice::Custom => Ok(Some(AgentSelection::Custom(
            collect_custom_agent_interactively(registry)?,
        ))),
        AgentChoice::Id(id) => {
            if let Some(entry) = registry.lookup(&id) {
                Ok(Some(AgentSelection::Registry(entry)))
            } else {
                Err(StackError::InvalidParam {
                    field: "agent",
                    reason: format!("selected registry agent `{id}` is unavailable"),
                })
            }
        }
    }
}

/// Collect a custom agent definition interactively. Only reached after the
/// operator explicitly picks "Custom agent…" in a TTY, so every field prompt is
/// interactive; `required` re-prompts on empty for the mandatory fields.
fn collect_custom_agent_interactively(registry: &RegistryCatalog) -> Result<CustomAgentSpec> {
    let id = required_custom_text("custom agent id (e.g. my-agent)")?;
    validate_custom_agent_id(&id)?;
    reject_registry_id_for_custom_agent(&id, registry)?;
    let name = prompt::text(true, "display name (blank = id)", false)?
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| id.clone());
    let command = required_custom_text("launch command (binary on PATH)")?;
    let args = prompt::text(true, "launch args (space-separated, blank = none)", false)?
        .map(|line| {
            line.split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let install_shell = required_custom_text("install shell command (installs harness + adapter)")?;
    let creates = prompt::text(
        true,
        "creates: path that must exist post-install (blank = command)",
        false,
    )?
    .map(|value| value.trim().to_owned())
    .filter(|value| !value.is_empty())
    .unwrap_or_else(|| command.clone());
    Ok(CustomAgentSpec {
        id,
        name,
        command,
        args,
        install_shell,
        creates,
    })
}

fn required_custom_text(prompt_text: &str) -> Result<String> {
    let value = prompt::text(true, prompt_text, true)?.ok_or(StackError::MissingField {
        field: "custom-agent field",
    })?;
    let value = value.trim().to_owned();
    if value.is_empty() {
        return Err(StackError::MissingField {
            field: "custom-agent field",
        });
    }
    Ok(value)
}

/// Apply a custom agent to config, paralleling `apply_registry_entry_to_config`.
/// Writes the launch command and the `[agent.install]` shell escape hatch, and
/// clears registry-derived fields. `auto_update` stays `None`: the managed
/// updater only knows how to update registry agents.
pub(super) fn apply_custom_agent_to_config(config: &mut Config, spec: &CustomAgentSpec) {
    let agent_changed = config.agent.id != spec.id;
    config.agent.id = spec.id.clone();
    config.agent.name = spec.name.clone();
    config.agent.command = spec.command.clone();
    config.agent.args = spec.args.clone();
    config.agent.cwd = Some(config.workspace.root.clone());
    if agent_changed {
        config.agent.env = Vec::new();
        config.agent.mode = None;
        config.agent.model = None;
        config.agent.provider = None;
        config.agent.auto_update = None;
    }
    config.agent.expected_sha256 = None;
    config.agent.restart = "on-crash".to_owned();
    config.agent.harness_version = None;
    config.agent.adapter = None;
    config.agent.install = Some(AgentInstallConfig {
        install_type: "shell".to_owned(),
        creates: spec.creates.clone(),
        shell: Some(spec.install_shell.clone()),
    });
}

pub(super) fn apply_registry_entry_to_config(config: &mut Config, entry: &RegistryEntry) {
    // When the operator re-confirms the SAME agent (e.g. `acps init
    // --agent X` again to refresh secrets or pick up registry changes
    // for the launch command), preserve provider/model/mode/env so a
    // bare re-run doesn't quietly drop a previously pinned model or
    // mode. When switching to a DIFFERENT agent, clear the agent
    // block so leftover provider/model/mode from the prior agent
    // can't poison the new launch context.
    let agent_changed = config.agent.id != entry.id;
    config.agent.id = entry.id.clone();
    config.agent.name = entry.name.clone();
    config.agent.cwd = Some(config.workspace.root.clone());
    if agent_changed {
        config.agent.env = default_agent_env_refs(&entry.id);
        config.agent.mode = None;
        config.agent.model = None;
        config.agent.provider = None;
        config.agent.auto_update = default_supported_agent_auto_update();
    } else if config.agent.auto_update.is_none() {
        config.agent.auto_update = default_supported_agent_auto_update();
    }
    config.agent.expected_sha256 = None;
    config.agent.restart = "on-crash".to_owned();
    config.agent.harness_version = None;
    config.agent.adapter = None;
    config.agent.install = None;

    match entry.kind {
        RegistryKind::Native => {
            let harness = entry.harness.as_ref().expect("validated registry harness");
            config.agent.command = harness.id.clone();
            config.agent.args = vec!["acp".to_owned()];
            #[cfg(feature = "test-fixtures")]
            if crate::runtime::install::agent_registry::development_placebo_registry_path()
                .is_some_and(|path| path.display().to_string() == harness.id)
            {
                config.agent.args.extend([
                    "--model-config-option".to_owned(),
                    crate::runtime::install::agent_registry::DEV_PLACEBO_MODEL_OPTION.to_owned(),
                ]);
            }
        }
        RegistryKind::Adapter => {
            let adapter = entry.adapter.as_ref().expect("validated registry adapter");
            config.agent.command = adapter.id.clone();
            config.agent.args = Vec::new();
        }
    }
}

fn default_supported_agent_auto_update() -> Option<AgentAutoUpdateConfig> {
    Some(AgentAutoUpdateConfig {
        enabled: true,
        frequency: DEFAULT_AGENT_AUTO_UPDATE_FREQUENCY.to_owned(),
    })
}

fn default_agent_env_refs(agent_id: &str) -> Vec<String> {
    env_refs_for_agent_id(agent_id)
        .into_iter()
        .map(str::to_owned)
        .collect()
}
