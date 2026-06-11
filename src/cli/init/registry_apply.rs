use crate::config::{CloudflareEdgeConfig, Config, DependencyEntry, EdgeConfig};
use crate::error::{Result, StackError};
use crate::runtime::agent::provider_keys::env_refs_for_agent_id;
use crate::runtime::install::agent_registry::{RegistryCatalog, RegistryEntry, RegistryKind};

use super::{
    CloudflaredDeploymentArg, EdgeExposureArg, EdgeProviderArg, InitArgs, prompt, prompts_enabled,
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

pub(super) fn select_agent_for_init<'a>(
    args: &InitArgs,
    registry: &'a RegistryCatalog,
) -> Result<Option<&'a RegistryEntry>> {
    if let Some(id) = &args.agent {
        return registry.lookup_required(id).map(Some);
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
    items.push((AgentChoice::Skip, "Skip".to_owned(), String::new()));
    let Some(choice) = prompt::searchable_select(prompts_enabled(args), "Agent", &items)? else {
        return Ok(None);
    };
    let AgentChoice::Id(id) = choice else {
        return Ok(None);
    };
    if let Some(entry) = registry.lookup(&id) {
        Ok(Some(entry))
    } else {
        Err(StackError::InvalidParam {
            field: "agent",
            reason: format!("selected registry agent `{id}` is unavailable"),
        })
    }
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

fn default_agent_env_refs(agent_id: &str) -> Vec<String> {
    env_refs_for_agent_id(agent_id)
        .into_iter()
        .map(str::to_owned)
        .collect()
}
