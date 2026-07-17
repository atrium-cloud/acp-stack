use std::collections::{BTreeMap, HashSet};
use std::io::{self, IsTerminal, Write};

use crate::config::{self, AgentConfig, AgentProvidersConfig, Config, is_valid_secret_ref_name};
use crate::error::{Result, StackError};
use crate::runtime::agent::agent_headless_config::{
    ProvisionedAgentConfig, provision_agent_headless_config,
    provision_agent_headless_config_transition,
};
use crate::runtime::agent::provider_keys::{
    effective_active_provider_ids, env_var_for_provider_id, provider_id_is_known,
    provider_id_supports_agent, provider_uses_agent_native_auth, resolve_agent_environment,
};
use crate::secrets::{ProviderCredential, ProviderCredentialSet, SecretStore};

pub(super) fn parse_provider_list(raw: &str) -> Result<Vec<String>> {
    if raw.trim().is_empty() {
        return Err(StackError::InvalidParam {
            field: "providers",
            reason: "pass a comma-separated provider list".to_owned(),
        });
    }
    let mut seen = HashSet::new();
    let mut providers = Vec::new();
    for value in raw.split(',') {
        let provider_id = value.trim();
        if provider_id.is_empty() {
            return Err(StackError::InvalidParam {
                field: "providers",
                reason: "provider ids must not be empty".to_owned(),
            });
        }
        if !seen.insert(provider_id.to_owned()) {
            return Err(StackError::InvalidParam {
                field: "providers",
                reason: format!("duplicate provider `{provider_id}`"),
            });
        }
        providers.push(provider_id.to_owned());
    }
    Ok(providers)
}

pub(super) fn validate_provider_for_agent(provider_id: &str, agent_id: &str) -> Result<()> {
    if !provider_id_is_known(provider_id) {
        return Err(StackError::InvalidParam {
            field: "provider",
            reason: format!("provider `{provider_id}` is not listed in provider/env mapping"),
        });
    }
    if !provider_id_supports_agent(provider_id, agent_id) {
        return Err(StackError::InvalidParam {
            field: "provider",
            reason: format!("provider `{provider_id}` is not supported for agent `{agent_id}`"),
        });
    }
    if env_var_for_provider_id(provider_id).is_none()
        && !provider_uses_agent_native_auth(agent_id, provider_id)
    {
        return Err(StackError::InvalidParam {
            field: "provider",
            reason: format!("provider `{provider_id}` does not use an acps-managed API key"),
        });
    }
    Ok(())
}

pub(super) fn validate_catalog_provider(provider_id: &str) -> Result<()> {
    if !provider_id_is_known(provider_id) {
        return Err(StackError::InvalidParam {
            field: "provider",
            reason: format!("provider `{provider_id}` is not listed in provider/env mapping"),
        });
    }
    if env_var_for_provider_id(provider_id).is_none() {
        return Err(StackError::InvalidParam {
            field: "provider",
            reason: format!("provider `{provider_id}` does not use an acps-managed API key"),
        });
    }
    Ok(())
}

pub(super) fn ensure_target_has_usable_credential(
    agent: &AgentConfig,
    provider_id: &str,
    secrets: &SecretStore,
) -> Result<()> {
    if provider_uses_agent_native_auth(&agent.id, provider_id) {
        return Ok(());
    }
    let credentials = secrets
        .provider_credential_set(provider_id)
        .ok_or_else(|| missing_provider_credential(provider_id))?;
    let selected_alias = agent
        .providers
        .as_ref()
        .and_then(|providers| providers.selected_aliases.get(provider_id))
        .map(String::as_str);
    if credentials.selected(selected_alias).is_none() {
        return Err(StackError::InvalidParam {
            field: "alias",
            reason: if credentials.is_promoted() && selected_alias.is_none() {
                format!("provider `{provider_id}` requires a selected alias for this target")
            } else {
                format!("provider `{provider_id}` selected alias does not exist")
            },
        });
    }
    Ok(())
}

pub(super) fn require_provider_in_active_set(
    providers: &[String],
    required: &str,
    lane: &'static str,
) -> Result<()> {
    if providers.iter().any(|provider| provider == required) {
        return Ok(());
    }
    Err(StackError::InvalidParam {
        field: "providers",
        reason: format!("active providers must include {lane} provider `{required}`"),
    })
}

pub(super) fn ensure_provider_settings(agent: &mut AgentConfig) -> &mut AgentProvidersConfig {
    if agent.providers.is_none() {
        let active = effective_active_provider_ids(agent);
        agent.providers = Some(AgentProvidersConfig {
            active,
            selected_aliases: BTreeMap::new(),
        });
    }
    agent
        .providers
        .get_or_insert_with(AgentProvidersConfig::default)
}

pub(super) fn ensure_credential_value_is_new(
    provider_id: &str,
    existing: &ProviderCredentialSet,
    candidate: &ProviderCredential,
) -> Result<()> {
    let primary = env_var_for_provider_id(provider_id).ok_or_else(|| StackError::InvalidParam {
        field: "provider",
        reason: format!("provider `{provider_id}` has no canonical API-key env var"),
    })?;
    let candidate_value =
        candidate
            .values
            .get(primary)
            .ok_or_else(|| StackError::SecretStorePlaintextInvalid {
                reason: format!("provider credential `{provider_id}` is missing `{primary}`"),
            })?;
    let is_duplicate = existing
        .sole
        .iter()
        .chain(existing.aliases.values())
        .any(|credential| credential.values.get(primary) == Some(candidate_value));
    if is_duplicate {
        return Err(StackError::InvalidParam {
            field: "credential",
            reason: format!("provider `{provider_id}` already contains that credential"),
        });
    }
    Ok(())
}

pub(super) fn resolve_alias(provided: Option<&str>, prompt: &str) -> Result<String> {
    let alias = match provided {
        Some(alias) => alias.to_owned(),
        None if io::stdin().is_terminal() => prompt_text(prompt)?,
        None => {
            return Err(StackError::InvalidParam {
                field: "alias",
                reason: format!("{prompt} is required in non-interactive mode"),
            });
        }
    };
    validate_alias(&alias)?;
    Ok(alias)
}

pub(super) fn validate_alias(alias: &str) -> Result<()> {
    if !is_valid_secret_ref_name(alias) {
        return Err(StackError::InvalidParam {
            field: "alias",
            reason: format!("`{alias}` must follow secret-reference identifier rules"),
        });
    }
    Ok(())
}

fn prompt_text(label: &str) -> Result<String> {
    print!("{label}: ");
    io::stdout()
        .flush()
        .map_err(|source| StackError::ServeIo { source })?;
    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .map_err(|source| StackError::ServeIo { source })?;
    Ok(input.trim().to_owned())
}

pub(super) fn target_index(config: &Config, target_id: &str) -> Result<usize> {
    config
        .array
        .targets
        .iter()
        .position(|target| target.id == target_id)
        .ok_or_else(|| StackError::InvalidParam {
            field: "target",
            reason: format!("unknown Array target `{target_id}`"),
        })
}

pub(super) fn config_for_target(config: &Config, target_index: usize) -> Config {
    let mut target_config = config.clone();
    target_config.agent = config.array.targets[target_index].agent.clone();
    target_config
}

pub(super) fn provision_configured_targets(
    config: &Config,
    home: &std::path::Path,
) -> Result<Vec<ProvisionedAgentConfig>> {
    let mut provisioned = Vec::new();
    for target_index in 0..config.array.targets.len() {
        provisioned.extend(provision_agent_headless_config(
            &config_for_target(config, target_index),
            home,
        )?);
    }
    Ok(provisioned)
}

pub(super) fn provision_configured_targets_with_transition(
    config: &Config,
    transitioned_target_id: &str,
    previous_target: &Config,
    home: &std::path::Path,
) -> Result<Vec<ProvisionedAgentConfig>> {
    let mut provisioned = Vec::new();
    for target_index in 0..config.array.targets.len() {
        let target = &config.array.targets[target_index];
        let target_config = config_for_target(config, target_index);
        if target.id == transitioned_target_id {
            provisioned.extend(provision_agent_headless_config_transition(
                previous_target,
                &target_config,
                home,
            )?);
        } else {
            provisioned.extend(provision_agent_headless_config(&target_config, home)?);
        }
    }
    Ok(provisioned)
}

pub(super) fn resolve_configured_target_environments(
    config: &Config,
    secrets: &SecretStore,
) -> Result<()> {
    for target_index in 0..config.array.targets.len() {
        resolve_agent_environment(&config_for_target(config, target_index), secrets)?;
    }
    Ok(())
}

pub(super) fn sync_primary_agent(config: &mut Config) {
    if let Some(primary) = config.array.primary_target() {
        config.agent = primary.agent.clone();
    }
}

pub(super) fn canonicalize_for_write(config: &Config) -> Result<(Config, String)> {
    let canonical = config.to_canonical_toml()?;
    let validated = config::load_config_from_str(&canonical)?;
    let canonical = validated.to_canonical_toml()?;
    Ok((validated, canonical))
}

pub(super) fn missing_provider_credential(provider_id: &str) -> StackError {
    StackError::InvalidParam {
        field: "provider",
        reason: format!(
            "provider `{provider_id}` has no credential; run `acps agent provider credential add {provider_id}`"
        ),
    }
}
