use std::time::Duration;

use crate::cli::core::{
    CliMethod, OutputFormat, SessionAccess, daemon_base_url, daemon_request, local_daemon_request,
    print_json, resolve_session_access,
};
use crate::config::{self, AgentProvidersConfig, Config};
use crate::error::{Result, StackError};
use crate::fs_util::{acquire_agent_config_mutation_file_lock, home_dir};
use crate::runtime::agent::agent_headless_config::{
    OPENCODE_AGENT_ID, provision_agent_headless_config, provision_agent_headless_config_transition,
};
use crate::runtime::agent::native_config_import::{
    NativeConfigPathSnapshot, capture_native_config_snapshots, restore_native_config_snapshots,
};
use crate::runtime::agent::provider_keys::{
    agent_provider_id_for_provider_id, apply_catalog_mapped_agent_provider,
    resolve_agent_environment, target_uses_provider,
};
use crate::runtime::install::agent_registry::RegistryCatalog;
use crate::secrets::SecretStore;

use super::install::operator_registry_override;
use super::provider_migration::{
    migrate_legacy_provider_credentials, persist_migrated_catalog_then_config,
    prune_migrated_flat_secrets_with_candidates,
};
use super::provider_shared::{
    canonicalize_for_write, config_for_target, ensure_provider_settings,
    ensure_target_has_usable_credential, missing_provider_credential, parse_provider_list,
    provision_configured_targets, provision_configured_targets_with_transition,
    require_provider_in_active_set, resolve_configured_target_environments, sync_primary_agent,
    target_index, validate_alias, validate_provider_for_agent,
};
use super::set::{
    print_agent_set_effective_notice_for, provider_change_requires_restart,
    resolve_agent_model_value,
};

// Keep local catalog inspection responsive when the optional live daemon
// overlay is unavailable, matching the other CLI status probes.
const PROVIDER_STATUS_DAEMON_TIMEOUT: Duration = Duration::from_secs(2);

pub(in crate::cli) fn run_target_provider_use(
    target_id: &str,
    provider_id: &str,
    model: Option<&str>,
    output: OutputFormat,
) -> Result<()> {
    let home = home_dir()?;
    let config_path = config::default_config_path()?;
    let _mutation = acquire_agent_config_mutation_file_lock(&config_path)?;
    let mut config = Config::load_from_path(&config_path)?;
    let mut secrets = SecretStore::open(&home)?;
    let previous_catalog = secrets.provider_credentials().clone();
    let migration = migrate_legacy_provider_credentials(&mut config, &mut secrets)?;
    let target_position = target_index(&config, target_id)?;
    let previous_target_config = config_for_target(&config, target_position);
    let agent_id = config.array.targets[target_position].agent.id.clone();
    let registry = RegistryCatalog::load_with_override(&operator_registry_override(&home))?;
    let entry = registry.lookup_required(&agent_id)?;
    if !entry.set_provider {
        return Err(StackError::InvalidParam {
            field: "provider",
            reason: format!("{} does not support mapped provider selection", entry.name),
        });
    }
    validate_provider_for_agent(provider_id, &agent_id)?;
    ensure_target_has_usable_credential(
        &config.array.targets[target_position].agent,
        provider_id,
        &secrets,
    )?;

    {
        let agent = &mut config.array.targets[target_position].agent;
        let preserve_model = agent
            .provider
            .as_ref()
            .filter(|provider| provider.id == provider_id)
            .and_then(|provider| provider.model.clone());
        apply_catalog_mapped_agent_provider(agent, provider_id, entry.multiple_active_providers)?;
        agent
            .provider
            .as_mut()
            .ok_or(StackError::MissingField {
                field: "agent.provider",
            })?
            .model = preserve_model;
    }
    sync_primary_agent(&mut config);

    if let Some(model) = model {
        let mut target_config = config_for_target(&config, target_position);
        let discovery_snapshots = if agent_id == OPENCODE_AGENT_ID {
            let path = home.join(".config").join("opencode").join("opencode.json");
            let snapshots = capture_native_config_snapshots(&[path], &home)?;
            if let Err(error) = provision_agent_headless_config(&target_config, &home) {
                return restore_after_model_discovery_failure(snapshots, &home, error);
            }
            Some(snapshots)
        } else {
            None
        };
        let native_provider = agent_provider_id_for_provider_id(&agent_id, provider_id);
        let resolved =
            match resolve_agent_model_value(&home, &target_config, native_provider, model) {
                Ok(resolved) => resolved,
                Err(error) => {
                    if let Some(snapshots) = discovery_snapshots {
                        return restore_after_model_discovery_failure(snapshots, &home, error);
                    }
                    return Err(error);
                }
            };
        config.array.targets[target_position]
            .agent
            .provider
            .as_mut()
            .ok_or(StackError::MissingField {
                field: "agent.provider",
            })?
            .model = Some(resolved);
        sync_primary_agent(&mut config);
        target_config = config_for_target(&config, target_position);
        resolve_agent_environment(&target_config, &secrets)?;
    }

    let (validated, canonical) = canonicalize_for_write(&config)?;
    let target_index = target_index(&validated, target_id)?;
    let target_config = config_for_target(&validated, target_index);
    resolve_agent_environment(&target_config, &secrets)?;
    let provisioned = if migration.changed {
        resolve_configured_target_environments(&validated, &secrets)?;
        provision_configured_targets_with_transition(
            &validated,
            target_id,
            &previous_target_config,
            &home,
        )?
    } else {
        provision_agent_headless_config_transition(&previous_target_config, &target_config, &home)?
    };
    persist_migrated_catalog_then_config(
        &mut secrets,
        previous_catalog,
        migration.changed,
        &config_path,
        &canonical,
    )?;
    prune_migrated_flat_secrets_with_candidates(
        &validated,
        &mut secrets,
        &migration.cleanup_candidates,
    )?;

    let agent_id = validated.array.targets[target_index].agent.id.clone();
    let restart_required = provider_change_requires_restart(&agent_id);
    let provider = validated.array.targets[target_index]
        .agent
        .provider
        .as_ref()
        .ok_or(StackError::MissingField {
            field: "agent.provider",
        })?;
    if output.is_json() {
        print_json(&serde_json::json!({
            "target_id": target_id,
            "provider": provider.id,
            "model": provider.model,
            "restart_required": restart_required,
            "provisioned": provisioned.iter().map(|item| item.path.display().to_string()).collect::<Vec<_>>(),
        }))?;
    } else {
        println!("target: {target_id}");
        println!("provider: {}", provider.id);
        if let Some(model) = provider.model.as_deref() {
            println!("model: {model}");
        }
        print_agent_set_effective_notice_for(Some(&agent_id));
    }
    Ok(())
}

pub(in crate::cli) fn run_target_provider_set_active(
    target_id: &str,
    raw_providers: &str,
    output: OutputFormat,
) -> Result<()> {
    let home = home_dir()?;
    let config_path = config::default_config_path()?;
    let _mutation = acquire_agent_config_mutation_file_lock(&config_path)?;
    let mut config = Config::load_from_path(&config_path)?;
    let mut secrets = SecretStore::open(&home)?;
    let previous_catalog = secrets.provider_credentials().clone();
    let migration = migrate_legacy_provider_credentials(&mut config, &mut secrets)?;
    let target_position = target_index(&config, target_id)?;
    let agent_id = config.array.targets[target_position].agent.id.clone();
    let registry = RegistryCatalog::load_with_override(&operator_registry_override(&home))?;
    let entry = registry.lookup_required(&agent_id)?;
    if !entry.multiple_active_providers {
        return Err(StackError::InvalidParam {
            field: "providers",
            reason: format!(
                "{} does not support `provider set-active`; only OpenCode and Pi support it",
                entry.name
            ),
        });
    }
    let providers = parse_provider_list(raw_providers)?;
    for provider_id in &providers {
        validate_provider_for_agent(provider_id, &agent_id)?;
        ensure_target_has_usable_credential(
            &config.array.targets[target_position].agent,
            provider_id,
            &secrets,
        )?;
    }
    let agent = &config.array.targets[target_position].agent;
    let default_provider = agent.provider.as_ref().ok_or(StackError::MissingField {
        field: "agent.provider",
    })?;
    if default_provider.custom.is_some() {
        return Err(StackError::InvalidParam {
            field: "providers",
            reason: "custom providers do not participate in active provider sets".to_owned(),
        });
    }
    require_provider_in_active_set(&providers, &default_provider.id, "default")?;
    if let Some(subagent_provider) = agent
        .subagent
        .as_ref()
        .filter(|subagent| !subagent.disabled)
        .and_then(|subagent| subagent.provider.as_ref())
    {
        if subagent_provider.custom.is_some() {
            return Err(StackError::InvalidParam {
                field: "providers",
                reason: "custom subagent providers do not participate in active provider sets"
                    .to_owned(),
            });
        }
        require_provider_in_active_set(&providers, &subagent_provider.id, "subagent")?;
    }

    // Prune alias selections for providers dropped from the active set.
    // A retained selection keeps `target_uses_provider` true for a now-inactive
    // provider (provider_keys.rs), which would wrongly block deleting that
    // provider's credential and leave dead selection state in the config.
    let mut selected_aliases = config.array.targets[target_position]
        .agent
        .providers
        .as_ref()
        .map(|providers| providers.selected_aliases.clone())
        .unwrap_or_default();
    selected_aliases.retain(|provider_id, _| providers.contains(provider_id));
    config.array.targets[target_position].agent.providers = Some(AgentProvidersConfig {
        active: providers.clone(),
        selected_aliases,
    });
    sync_primary_agent(&mut config);
    let (validated, canonical) = canonicalize_for_write(&config)?;
    let target_index = target_index(&validated, target_id)?;
    let target_config = config_for_target(&validated, target_index);
    resolve_agent_environment(&target_config, &secrets)?;
    let provisioned = if migration.changed {
        resolve_configured_target_environments(&validated, &secrets)?;
        provision_configured_targets(&validated, &home)?
    } else {
        provision_agent_headless_config(&target_config, &home)?
    };
    persist_migrated_catalog_then_config(
        &mut secrets,
        previous_catalog,
        migration.changed,
        &config_path,
        &canonical,
    )?;
    prune_migrated_flat_secrets_with_candidates(
        &validated,
        &mut secrets,
        &migration.cleanup_candidates,
    )?;

    let agent_id = validated.array.targets[target_index].agent.id.clone();
    let restart_required = provider_change_requires_restart(&agent_id);
    if output.is_json() {
        print_json(&serde_json::json!({
            "target_id": target_id,
            "active": providers,
            "restart_required": restart_required,
            "provisioned": provisioned.iter().map(|item| item.path.display().to_string()).collect::<Vec<_>>(),
        }))?;
    } else {
        println!("target: {target_id}");
        println!("active: {}", providers.join(","));
        print_agent_set_effective_notice_for(Some(&agent_id));
    }
    Ok(())
}

pub(in crate::cli) fn run_target_provider_list_active(
    target_id: &str,
    array_route: bool,
    output: OutputFormat,
) -> Result<()> {
    let home = home_dir()?;
    let config = Config::load_from_default_path()?;
    let target_index = target_index(&config, target_id)?;
    let target_config = config_for_target(&config, target_index);
    let secrets = SecretStore::open(&home)?;
    let configured = resolve_agent_environment(&target_config, &secrets)?.providers;
    let daemon = query_provider_status(&config, array_route);
    let configured_json = configured
        .iter()
        .map(provider_snapshot_json)
        .collect::<Vec<_>>();
    let (loaded, restart_required, loaded_state, daemon_reason) = match daemon {
        Ok(data) => {
            let (loaded, restart_required) =
                extract_daemon_provider_state(&data, array_route, target_id);
            (loaded, restart_required, "known", None)
        }
        Err(reason) => (None, None, "unknown", Some(reason)),
    };
    let result = serde_json::json!({
        "target_id": target_id,
        "configured_providers": configured_json,
        "loaded_providers": loaded,
        "loaded_state": loaded_state,
        "provider_restart_required": restart_required,
        "daemon_unavailable_reason": daemon_reason,
    });
    if output.is_json() {
        print_json(&result)?;
        return Ok(());
    }
    println!("target: {target_id}");
    for provider in configured {
        println!(
            "configured: provider={} alias={} env={}",
            provider.provider_id,
            provider.alias.as_deref().unwrap_or("(aliasless)"),
            if provider.env_names.is_empty() {
                "(agent-native)".to_owned()
            } else {
                provider.env_names.join(",")
            }
        );
    }
    match loaded_state {
        "known" => {
            let loaded = loaded.and_then(|value| value.as_array().cloned());
            if loaded.as_ref().is_none_or(Vec::is_empty) {
                println!("loaded: none");
            } else if let Some(loaded) = loaded {
                for provider in loaded {
                    println!(
                        "loaded: provider={} alias={} env={}",
                        provider["provider_id"].as_str().unwrap_or("?"),
                        provider["alias"].as_str().unwrap_or("(aliasless)"),
                        provider["env_names"]
                            .as_array()
                            .map(|values| values
                                .iter()
                                .filter_map(|value| value.as_str())
                                .collect::<Vec<_>>()
                                .join(","))
                            .unwrap_or_default(),
                    );
                }
            }
            println!(
                "provider_restart_required: {}",
                restart_required.unwrap_or(false)
            );
        }
        _ => {
            println!("loaded: unknown");
            if let Some(reason) = daemon_reason {
                println!("daemon: unavailable ({reason})");
            }
        }
    }
    Ok(())
}

pub(in crate::cli) fn run_target_credential_select(
    target_id: &str,
    provider_id: &str,
    alias: &str,
    output: OutputFormat,
) -> Result<()> {
    validate_alias(alias)?;
    let home = home_dir()?;
    let config_path = config::default_config_path()?;
    let _mutation = acquire_agent_config_mutation_file_lock(&config_path)?;
    let mut config = Config::load_from_path(&config_path)?;
    let mut secrets = SecretStore::open(&home)?;
    let previous_catalog = secrets.provider_credentials().clone();
    let migration = migrate_legacy_provider_credentials(&mut config, &mut secrets)?;
    let target_position = target_index(&config, target_id)?;
    let agent_id = config.array.targets[target_position].agent.id.clone();
    validate_provider_for_agent(provider_id, &agent_id)?;
    let credentials = secrets
        .provider_credential_set(provider_id)
        .ok_or_else(|| missing_provider_credential(provider_id))?;
    if !credentials.is_promoted() {
        return Err(StackError::InvalidParam {
            field: "alias",
            reason: format!("provider `{provider_id}` has one aliasless credential"),
        });
    }
    if !credentials.aliases.contains_key(alias) {
        return Err(StackError::InvalidParam {
            field: "alias",
            reason: format!("provider `{provider_id}` has no alias `{alias}`"),
        });
    }
    ensure_provider_settings(&mut config.array.targets[target_position].agent)
        .selected_aliases
        .insert(provider_id.to_owned(), alias.to_owned());
    sync_primary_agent(&mut config);
    let (validated, canonical) = canonicalize_for_write(&config)?;
    let target_index = target_index(&validated, target_id)?;
    let target_config = config_for_target(&validated, target_index);
    if migration.changed {
        resolve_configured_target_environments(&validated, &secrets)?;
        provision_configured_targets(&validated, &home)?;
    } else if target_uses_provider(&target_config.agent, provider_id) {
        resolve_agent_environment(&target_config, &secrets)?;
        provision_agent_headless_config(&target_config, &home)?;
    }
    persist_migrated_catalog_then_config(
        &mut secrets,
        previous_catalog,
        migration.changed,
        &config_path,
        &canonical,
    )?;
    prune_migrated_flat_secrets_with_candidates(
        &validated,
        &mut secrets,
        &migration.cleanup_candidates,
    )?;
    let agent_id = validated.array.targets[target_index].agent.id.clone();
    let restart_required = provider_change_requires_restart(&agent_id);
    if output.is_json() {
        print_json(&serde_json::json!({
            "target_id": target_id,
            "provider": provider_id,
            "alias": alias,
            "restart_required": restart_required,
        }))?;
    } else {
        println!("target: {target_id}");
        println!("provider: {provider_id}");
        println!("alias: {alias}");
        print_agent_set_effective_notice_for(Some(&agent_id));
    }
    Ok(())
}

/// Read the live provider state back from a daemon status payload. For the
/// array route the per-target object is looked up by `id`; a missing target or
/// absent field yields `None` rather than an error, so the CLI degrades to the
/// configured-only view. Extracted for direct unit testing without a daemon.
pub(super) fn extract_daemon_provider_state(
    data: &serde_json::Value,
    array_route: bool,
    target_id: &str,
) -> (Option<serde_json::Value>, Option<bool>) {
    let provider_data = if array_route {
        data.get("targets")
            .and_then(serde_json::Value::as_array)
            .and_then(|targets| {
                targets
                    .iter()
                    .find(|target| target["id"].as_str() == Some(target_id))
            })
            .cloned()
            .unwrap_or(serde_json::Value::Null)
    } else {
        data.clone()
    };
    (
        provider_data.get("loaded_providers").cloned(),
        provider_data
            .get("provider_restart_required")
            .and_then(serde_json::Value::as_bool),
    )
}

fn provider_snapshot_json(
    provider: &crate::runtime::agent::provider_keys::ResolvedProviderSnapshot,
) -> serde_json::Value {
    serde_json::json!({
        "provider_id": provider.provider_id,
        "alias": provider.alias,
        "env_names": provider.env_names,
    })
}

fn query_provider_status(
    config: &Config,
    array_route: bool,
) -> std::result::Result<serde_json::Value, String> {
    let access = resolve_session_access(config, None).map_err(|error| error.public_message())?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("runtime unavailable: {error}"))?;
    let path = if array_route {
        "/v1/array/status"
    } else {
        "/v1/agent/status"
    };
    let request = async {
        match access {
            SessionAccess::Local => local_daemon_request(config, CliMethod::Get, path, None).await,
            SessionAccess::Bearer(session_key) => {
                let base_url = daemon_base_url(config.api.public_url.as_deref(), &config.api.bind)?;
                daemon_request(&base_url, CliMethod::Get, path, &session_key, None).await
            }
        }
    };
    runtime
        .block_on(tokio::time::timeout(
            PROVIDER_STATUS_DAEMON_TIMEOUT,
            request,
        ))
        .map_err(|_| "request timed out".to_owned())?
        .map(|body| body.get("data").cloned().unwrap_or(body))
        .map_err(|error| error.public_message())
}

fn restore_after_model_discovery_failure<T>(
    snapshots: Vec<NativeConfigPathSnapshot>,
    home: &std::path::Path,
    original: StackError,
) -> Result<T> {
    if let Err(restore_error) = restore_native_config_snapshots(&snapshots, home) {
        return Err(StackError::AgentConfigProvision {
            path: home.join(".config").join("opencode").join("opencode.json"),
            reason: format!(
                "model discovery failed: {original}; restoring OpenCode config also failed: {restore_error}"
            ),
        });
    }
    Err(original)
}
