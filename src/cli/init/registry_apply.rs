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

/// An operator-declared agent outside the embedded registry, modeled with `[agent]` plus the
/// `[agent.install]` shell escape hatch.
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

/// True when the config points at an unknown agent carrying an `[agent.install]` escape hatch,
/// which bypasses the registry-only gates.
pub(super) fn is_custom_agent(config: &Config, registry: &RegistryCatalog) -> bool {
    config.agent.install.is_some() && registry.lookup(&config.agent.id).is_none()
}

/// Assemble a custom-agent spec from the `--custom-agent-*` flags; `None` when none were passed.
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

/// Outcome of the `--adapter-override-*` flag family: designate an adapter, or clear a stored one.
pub(super) enum AdapterOverrideAction {
    Set(Box<crate::config::AgentAdapterOverrideConfig>),
    Clear,
}

/// Assemble the adapter-override action from init flags; exactly one install source is required.
/// The github-asset variant is import-only, reachable through `acps config import` / `--from-*`.
pub(super) fn resolve_adapter_override_action(
    args: &InitArgs,
) -> Result<Option<AdapterOverrideAction>> {
    if args.adapter_override_clear {
        return Ok(Some(AdapterOverrideAction::Clear));
    }
    let Some(raw_command) = args.adapter_override_command.as_deref() else {
        return Ok(None);
    };
    let command = raw_command.trim().to_owned();
    if command.is_empty() {
        return Err(StackError::MissingField {
            field: "--adapter-override-command",
        });
    }
    let creates = args
        .adapter_override_install_creates
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| command.clone());
    let install = match (
        args.adapter_override_install_npm.as_deref(),
        args.adapter_override_install_shell.as_deref(),
    ) {
        (Some(package), None) => crate::config::AgentAdapterOverrideInstall {
            shell: None,
            npm: Some(crate::config::AgentAdapterOverrideNpmInstall {
                package: package.trim().to_owned(),
                creates,
            }),
            github: None,
        },
        (None, Some(script)) => crate::config::AgentAdapterOverrideInstall {
            shell: Some(crate::config::AgentAdapterOverrideShellInstall {
                script: script.to_owned(),
                creates,
                required_tools: Vec::new(),
                timeout_secs: None,
            }),
            npm: None,
            github: None,
        },
        (None, None) => {
            return Err(StackError::MissingField {
                field: "--adapter-override-install-npm or --adapter-override-install-shell",
            });
        }
        // clap already rejects the pair; unreachable arm kept typed.
        (Some(_), Some(_)) => {
            return Err(StackError::InvalidParam {
                field: "--adapter-override-install-npm",
                reason: "cannot be combined with --adapter-override-install-shell".to_owned(),
            });
        }
    };
    Ok(Some(AdapterOverrideAction::Set(Box::new(
        crate::config::AgentAdapterOverrideConfig {
            command,
            args: args.adapter_override_arg.clone(),
            github: args
                .adapter_override_github
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
            install,
            update: Default::default(),
        },
    ))))
}

/// Apply a resolved adapter-override action onto the agent block, returning true when it changed.
pub(super) fn apply_adapter_override_action(
    config: &mut Config,
    action: &Option<AdapterOverrideAction>,
) -> bool {
    match action {
        Some(AdapterOverrideAction::Set(block)) => {
            let changed = config.agent.adapter_override.as_ref() != Some(block.as_ref());
            config.agent.adapter_override = Some(block.as_ref().clone());
            changed
        }
        Some(AdapterOverrideAction::Clear) => {
            let changed = config.agent.adapter_override.is_some();
            config.agent.adapter_override = None;
            changed
        }
        None => false,
    }
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
            prompt::item(
                AgentChoice::Id(entry.id.clone()),
                entry.id.clone(),
                format!("{} ({})", entry.name, entry.id),
                "",
            )
        })
        .collect::<Vec<_>>();
    items.push(prompt::item(
        AgentChoice::Custom,
        "__custom",
        "Custom agent",
        "not in the registry",
    ));
    items.push(prompt::item(AgentChoice::Skip, "__skip", "Skip", ""));
    let Some(choice) = prompt::searchable_select(
        prompt::HostedPromptKind::Agent,
        prompts_enabled(args),
        "Agent",
        &items,
    )?
    else {
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

/// Collect a custom agent definition interactively, reached only after picking "Custom agent" in a TTY.
fn collect_custom_agent_interactively(registry: &RegistryCatalog) -> Result<CustomAgentSpec> {
    let id = required_custom_text(
        prompt::HostedPromptKind::CustomAgentId,
        "custom agent id (e.g. my-agent)",
    )?;
    validate_custom_agent_id(&id)?;
    reject_registry_id_for_custom_agent(&id, registry)?;
    let name = prompt::text(
        prompt::HostedPromptKind::CustomAgentName,
        true,
        "display name (blank = id)",
        false,
    )?
    .map(|value| value.trim().to_owned())
    .filter(|value| !value.is_empty())
    .unwrap_or_else(|| id.clone());
    let command = required_custom_text(
        prompt::HostedPromptKind::CustomAgentCommand,
        "launch command (binary on PATH)",
    )?;
    let args = prompt::text(
        prompt::HostedPromptKind::CustomAgentArgs,
        true,
        "launch args (space-separated, blank = none)",
        false,
    )?
    .map(|line| {
        line.split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>()
    })
    .unwrap_or_default();
    let install_shell = required_custom_text(
        prompt::HostedPromptKind::CustomAgentInstallShell,
        "install shell command (installs harness + adapter)",
    )?;
    let creates = prompt::text(
        prompt::HostedPromptKind::CustomAgentCreates,
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

fn required_custom_text(kind: prompt::HostedPromptKind, prompt_text: &str) -> Result<String> {
    let value = prompt::text(kind, true, prompt_text, true)?.ok_or(StackError::MissingField {
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

/// Apply a custom agent to config, paralleling `apply_registry_entry_to_config`. `auto_update` stays
/// `None`: the managed updater only knows how to update registry agents.
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
        config.agent.effort = None;
        config.agent.config_options = Default::default();
        config.agent.provider = None;
        config.agent.providers = None;
        config.agent.auto_update = None;
    }
    config.agent.expected_sha256 = None;
    config.agent.restart = "on-crash".to_owned();
    config.agent.harness_version = None;
    config.agent.adapter = None;
    config.agent.adapter_override = None;
    config.agent.install = Some(AgentInstallConfig {
        install_type: "shell".to_owned(),
        creates: spec.creates.clone(),
        shell: Some(spec.install_shell.clone()),
    });
}

pub(super) fn apply_registry_entry_to_config(config: &mut Config, entry: &RegistryEntry) {
    // Re-confirming the SAME agent preserves provider/model/mode/effort/env so a bare re-run does
    // not drop a pinned model; switching agents clears them so they cannot poison the new launch.
    let agent_changed = config.agent.id != entry.id;
    config.agent.id = entry.id.clone();
    config.agent.name = entry.name.clone();
    config.agent.cwd = Some(config.workspace.root.clone());
    if agent_changed {
        config.agent.env = default_agent_env_refs(&entry.id);
        config.agent.mode = None;
        config.agent.model = None;
        config.agent.effort = None;
        config.agent.config_options = Default::default();
        config.agent.provider = None;
        config.agent.providers = None;
        config.agent.auto_update = default_supported_agent_auto_update();
        // The override designates an adapter for THIS agent; a different target must not inherit it.
        config.agent.adapter_override = None;
    } else if config.agent.auto_update.is_none() {
        config.agent.auto_update = default_supported_agent_auto_update();
    }
    config.agent.expected_sha256 = None;
    config.agent.restart = "on-crash".to_owned();
    config.agent.harness_version = None;
    config.agent.adapter = None;
    config.agent.install = None;

    apply_agent_launch_command(config, entry);
}

/// Write `agent.command`/`agent.args` from `[agent.adapter_override]` when set, else the curated
/// entry. Callers that mutate the override after `apply_registry_entry_to_config` MUST re-run this.
pub(super) fn apply_agent_launch_command(config: &mut Config, entry: &RegistryEntry) {
    if let Some(override_config) = config.agent.adapter_override.clone() {
        config.agent.command = override_config.command;
        config.agent.args = override_config.args;
        return;
    }
    match entry.kind {
        RegistryKind::Native => {
            let harness = entry.harness.as_ref().expect("validated registry harness");
            config.agent.command = harness.id.clone();
            config.agent.args = harness.acp_args.clone();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::load_config_from_str;

    fn valid_config() -> Config {
        load_config_from_str(include_str!(
            "../../../tests/fixtures/valid-opencode-stack.toml"
        ))
        .expect("fixture parses")
    }

    fn sample_override() -> crate::config::AgentAdapterOverrideConfig {
        crate::config::AgentAdapterOverrideConfig {
            command: "custom-acp".to_owned(),
            args: vec!["--verbose".to_owned()],
            github: None,
            install: crate::config::AgentAdapterOverrideInstall {
                shell: None,
                npm: Some(crate::config::AgentAdapterOverrideNpmInstall {
                    package: "custom-acp".to_owned(),
                    creates: "custom-acp".to_owned(),
                }),
                github: None,
            },
            update: Default::default(),
        }
    }

    #[test]
    fn same_agent_reapply_preserves_override_and_writes_its_command() {
        let mut config = valid_config();
        let registry = RegistryCatalog::load_embedded().expect("registry");
        let entry = registry.lookup("goose").expect("goose entry");
        config.agent.id = "goose".to_owned();
        config.agent.adapter_override = Some(sample_override());

        apply_registry_entry_to_config(&mut config, entry);

        assert!(config.agent.adapter_override.is_some());
        assert_eq!(config.agent.command, "custom-acp");
        assert_eq!(config.agent.args, ["--verbose"]);
        assert!(config.agent.install.is_none());
    }

    #[test]
    fn agent_change_clears_override_and_restores_registry_command() {
        let mut config = valid_config();
        let registry = RegistryCatalog::load_embedded().expect("registry");
        config.agent.id = "goose".to_owned();
        config.agent.adapter_override = Some(sample_override());
        let entry = registry.lookup("opencode").expect("opencode entry");

        apply_registry_entry_to_config(&mut config, entry);

        assert!(config.agent.adapter_override.is_none());
        assert_eq!(config.agent.command, "opencode");
        assert_eq!(config.agent.args, ["acp"]);
    }

    #[test]
    fn custom_agent_apply_clears_override() {
        let mut config = valid_config();
        config.agent.adapter_override = Some(sample_override());
        let spec = CustomAgentSpec {
            id: "my-agent".to_owned(),
            name: "My Agent".to_owned(),
            command: "my-agent".to_owned(),
            args: Vec::new(),
            install_shell: "true".to_owned(),
            creates: "my-agent".to_owned(),
        };

        apply_custom_agent_to_config(&mut config, &spec);

        assert!(config.agent.adapter_override.is_none());
        assert_eq!(config.agent.command, "my-agent");
    }
}
