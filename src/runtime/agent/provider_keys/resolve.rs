//! Provider resolution and application.
//!
//! Resolution turns the active provider set into the concrete environment the
//! agent process is launched with, reading structured credentials from the
//! secret store and falling back to legacy `[agent].env` refs. Application is
//! the inverse direction: writing a chosen provider back into canonical
//! `[agent]` config.

use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProviderSnapshot {
    pub provider_id: String,
    pub alias: Option<String>,
    pub revision: Option<String>,
    pub env_names: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAgentEnvironment {
    pub env: HashMap<String, String>,
    pub providers: Vec<ResolvedProviderSnapshot>,
}

pub fn effective_active_provider_ids(agent: &AgentConfig) -> Vec<String> {
    let mut active = agent
        .providers
        .as_ref()
        .map(|providers| providers.active.clone())
        .unwrap_or_else(|| {
            agent
                .provider
                .as_ref()
                .map(|provider| vec![provider.id.clone()])
                .unwrap_or_default()
        });
    if let Some(subagent_provider) = agent
        .subagent
        .as_ref()
        .filter(|subagent| !subagent.disabled)
        .and_then(|subagent| subagent.provider.as_ref())
        && !active
            .iter()
            .any(|provider_id| provider_id == &subagent_provider.id)
    {
        active.push(subagent_provider.id.clone());
    }
    active
}

pub fn target_uses_provider(agent: &AgentConfig, provider_id: &str) -> bool {
    effective_active_provider_ids(agent)
        .iter()
        .any(|active| active == provider_id)
        || agent
            .providers
            .as_ref()
            .is_some_and(|providers| providers.selected_aliases.contains_key(provider_id))
}

fn selected_alias_for<'a>(config: &'a Config, provider_id: &str) -> Option<&'a str> {
    config
        .agent
        .providers
        .as_ref()
        .and_then(|providers| providers.selected_aliases.get(provider_id))
        .map(String::as_str)
}

/// True when the credential catalog will inject `env_ref` for `provider_id` at
/// spawn time. This is the exact mirror of the injection logic in
/// [`resolve_agent_environment`]; the init secret gate relies on it so that a
/// passing gate always implies a resolvable spawn environment. Changing one
/// side without the other breaks that lockstep.
pub fn catalog_covers_env_ref(
    secrets: &SecretStore,
    agent_id: &str,
    provider_id: &str,
    selected_alias: Option<&str>,
    configured_api_key_ref: Option<&str>,
    env_ref: &str,
) -> bool {
    let Some(credentials) = secrets.provider_credential_set(provider_id) else {
        return false;
    };
    // A promoted set with no selected alias errors at resolve time, so it
    // must not satisfy the gate either.
    let Some((credential, _alias)) = credentials.selected(selected_alias) else {
        return false;
    };
    if !provider_id_is_known(provider_id) {
        return configured_api_key_ref == Some(env_ref) && credential.values.contains_key(env_ref);
    }
    if provider_uses_agent_native_auth(agent_id, provider_id) {
        return false;
    }
    let Some(canonical_primary) = env_var_for_provider_id(provider_id) else {
        return false;
    };
    if env_var_for_agent_provider_id(agent_id, provider_id) == Some(env_ref) {
        return credential.values.contains_key(canonical_primary);
    }
    let companion =
        companion_env_refs_for_agent_provider_id(agent_id, provider_id).contains(&env_ref);
    let optional =
        optional_env_refs_for_agent_provider_id(agent_id, provider_id).contains(&env_ref);
    (companion || optional) && credential.values.contains_key(env_ref)
}

/// The single satisfiability predicate for provider secret refs: the flat
/// secret store or the structured credential catalog can supply `env_ref`.
pub fn env_ref_is_satisfiable(
    secrets: &SecretStore,
    agent_id: &str,
    provider_id: &str,
    selected_alias: Option<&str>,
    configured_api_key_ref: Option<&str>,
    env_ref: &str,
) -> bool {
    secrets.contains(env_ref)
        || catalog_covers_env_ref(
            secrets,
            agent_id,
            provider_id,
            selected_alias,
            configured_api_key_ref,
            env_ref,
        )
}

/// Config-derived wrapper over [`env_ref_is_satisfiable`], deriving the agent
/// id, the selected credential alias, and the configured custom-provider
/// api-key ref from `config`. The custom lookup is agent-local (primary agent
/// and its subagent), matching what the injection below will actually use — a
/// custom provider declared only on another array target must not satisfy
/// here.
pub fn env_ref_is_satisfiable_for_config(
    config: &Config,
    secrets: &SecretStore,
    provider_id: &str,
    env_ref: &str,
) -> bool {
    env_ref_is_satisfiable(
        secrets,
        &config.agent.id,
        provider_id,
        selected_alias_for(config, provider_id),
        agent_custom_provider_api_key_ref(&config.agent, provider_id),
        env_ref,
    )
}

fn agent_custom_provider_api_key_ref<'a>(
    agent: &'a AgentConfig,
    provider_id: &str,
) -> Option<&'a str> {
    custom_provider_config(agent, provider_id).and_then(|provider| provider.api_key_ref.as_deref())
}

/// The api-key ref of a custom provider configured anywhere in the config:
/// the primary agent, its subagent, or any array target (and their
/// subagents). Managed-state apply uses this as the env contract for
/// provider ids outside the registry mapping.
pub fn configured_custom_provider_api_key_ref<'a>(
    config: &'a Config,
    provider_id: &str,
) -> Option<&'a str> {
    std::iter::once(&config.agent)
        .chain(config.array.targets.iter().map(|target| &target.agent))
        .find_map(|agent| {
            custom_provider_config(agent, provider_id)
                .and_then(|provider| provider.api_key_ref.as_deref())
        })
}

/// Env vars the credential catalog will inject at spawn for the active
/// provider set. Bare `[agent].env` refs for these vars are skipped during
/// resolution: the catalog is the authoritative rotation channel, so it wins
/// over a flat secret of the same name.
fn catalog_owned_env_vars(config: &Config, secrets: &SecretStore) -> BTreeSet<String> {
    let mut owned = BTreeSet::new();
    for provider_id in effective_active_provider_ids(&config.agent) {
        let configured_api_key_ref = agent_custom_provider_api_key_ref(&config.agent, &provider_id);
        let mut candidates: Vec<String> = Vec::new();
        if provider_id_is_known(&provider_id) {
            candidates.extend(
                env_var_for_agent_provider_id(&config.agent.id, &provider_id).map(str::to_owned),
            );
            candidates.extend(
                companion_env_refs_for_agent_provider_id(&config.agent.id, &provider_id)
                    .iter()
                    .map(|name| (*name).to_owned()),
            );
            candidates.extend(
                optional_env_refs_for_agent_provider_id(&config.agent.id, &provider_id)
                    .iter()
                    .map(|name| (*name).to_owned()),
            );
        } else {
            candidates.extend(configured_api_key_ref.map(str::to_owned));
        }
        for candidate in candidates {
            if catalog_covers_env_ref(
                secrets,
                &config.agent.id,
                &provider_id,
                selected_alias_for(config, &provider_id),
                configured_api_key_ref,
                &candidate,
            ) {
                owned.insert(candidate);
            }
        }
    }
    owned
}

pub fn resolve_agent_environment_without_secrets(
    config: &Config,
) -> Option<ResolvedAgentEnvironment> {
    if !config.agent.env.is_empty() {
        return None;
    }
    let mut providers = Vec::new();
    for provider_id in effective_active_provider_ids(&config.agent) {
        if !provider_id_is_known(&provider_id) {
            continue;
        }
        if !provider_uses_agent_native_auth(&config.agent.id, &provider_id) {
            return None;
        }
        providers.push(ResolvedProviderSnapshot {
            provider_id,
            alias: None,
            revision: None,
            env_names: Vec::new(),
        });
    }
    providers.sort_by(|left, right| left.provider_id.cmp(&right.provider_id));
    Some(ResolvedAgentEnvironment {
        env: HashMap::new(),
        providers,
    })
}

pub fn resolve_agent_environment(
    config: &Config,
    secrets: &SecretStore,
) -> Result<ResolvedAgentEnvironment> {
    let mut env = HashMap::with_capacity(config.agent.env.len());
    let mut owners: HashMap<String, Vec<String>> = HashMap::new();
    let catalog_owned = catalog_owned_env_vars(config, secrets);
    for entry in &config.agent.env {
        // A bare ref whose var the catalog injects is skipped: the catalog is
        // the managed rotation channel and wins over a flat secret of the same
        // name (a differing flat value would otherwise be a hard owner
        // conflict, not a precedence choice). Templated `VAR=...` entries are
        // explicit operator compositions and keep flat-store semantics.
        if crate::config::env_entry_var_name(entry) == entry.as_str()
            && catalog_owned.contains(entry.as_str())
        {
            continue;
        }
        let (var_name, value) = crate::config::resolve_env_entry("[agent].env", entry, secrets)
            .map_err(|error| remap_pending_provider_credential(config, entry, error))?;
        insert_resolved_env(&mut env, &mut owners, &var_name, value, "[agent].env")?;
    }

    let mut snapshots = Vec::new();
    for provider_id in effective_active_provider_ids(&config.agent) {
        if !provider_id_is_known(&provider_id) {
            // Custom (BYOK) providers inject their configured api-key ref from
            // the credential catalog when a managed credential exists, else
            // from the flat store via the `[agent].env` loop above. Genuinely
            // unknown ids are left out of the snapshot.
            if let Some(provider) = custom_provider_config(&config.agent, &provider_id) {
                let api_key_ref = provider.api_key_ref.as_deref();
                if let (Some(api_key_ref), Some(credentials)) =
                    (api_key_ref, secrets.provider_credential_set(&provider_id))
                {
                    let selected_alias = selected_alias_for(config, &provider_id);
                    let Some((credential, alias)) = credentials.selected(selected_alias) else {
                        // No CLI hint here: the catalog CLI commands reject
                        // non-mapped provider ids, so the mapped branch's
                        // `credential select` remediation would refuse to run.
                        let reason = if credentials.is_promoted() && selected_alias.is_none() {
                            format!(
                                "custom provider `{provider_id}` has backup credential aliases and none is selected in agent.providers.selected_aliases"
                            )
                        } else {
                            format!(
                                "selected credential alias for provider `{provider_id}` does not exist"
                            )
                        };
                        return Err(StackError::InvalidParam {
                            field: "agent.providers.selected_aliases",
                            reason,
                        });
                    };
                    let value = credential.values.get(api_key_ref).ok_or_else(|| {
                        StackError::SecretStorePlaintextInvalid {
                            reason: format!(
                                "provider credential `{provider_id}` is missing `{api_key_ref}`"
                            ),
                        }
                    })?;
                    insert_resolved_env(
                        &mut env,
                        &mut owners,
                        api_key_ref,
                        value.clone(),
                        &provider_id,
                    )?;
                    snapshots.push(ResolvedProviderSnapshot {
                        provider_id,
                        alias: alias.map(str::to_owned),
                        revision: Some(credential.revision.clone()),
                        env_names: vec![api_key_ref.to_owned()],
                    });
                    continue;
                }
                let mut env_names: Vec<String> = provider
                    .api_key_ref
                    .iter()
                    .filter(|name| env.contains_key(name.as_str()))
                    .cloned()
                    .collect();
                env_names.sort();
                env_names.dedup();
                snapshots.push(ResolvedProviderSnapshot {
                    provider_id,
                    alias: None,
                    revision: None,
                    env_names,
                });
            }
            continue;
        }
        if provider_uses_agent_native_auth(&config.agent.id, &provider_id) {
            let mut env_names =
                required_env_refs_for_agent_provider_id(&config.agent.id, &provider_id, None);
            env_names.extend(
                optional_env_refs_for_agent_provider_id(&config.agent.id, &provider_id)
                    .into_iter()
                    .map(str::to_owned),
            );
            env_names.retain(|env_name| env.contains_key(env_name));
            env_names.sort();
            env_names.dedup();
            snapshots.push(ResolvedProviderSnapshot {
                provider_id,
                alias: None,
                revision: None,
                env_names,
            });
            continue;
        }

        if let Some(credentials) = secrets.provider_credential_set(&provider_id) {
            let selected_alias = config
                .agent
                .providers
                .as_ref()
                .and_then(|providers| providers.selected_aliases.get(&provider_id))
                .map(String::as_str);
            let Some((credential, alias)) = credentials.selected(selected_alias) else {
                let reason = if credentials.is_promoted() && selected_alias.is_none() {
                    format!(
                        "provider `{provider_id}` has backup aliases; select one with `acps agent provider credential select {provider_id} <alias>`"
                    )
                } else {
                    format!("selected credential alias for provider `{provider_id}` does not exist")
                };
                return Err(StackError::InvalidParam {
                    field: "agent.providers.selected_aliases",
                    reason,
                });
            };
            let canonical_primary =
                env_var_for_provider_id(&provider_id).ok_or_else(|| StackError::InvalidParam {
                    field: "provider",
                    reason: format!("provider `{provider_id}` has no canonical API-key env var"),
                })?;
            let emitted_primary = env_var_for_agent_provider_id(&config.agent.id, &provider_id)
                .ok_or_else(|| StackError::InvalidParam {
                    field: "provider",
                    reason: format!(
                        "provider `{provider_id}` has no API-key env mapping for agent `{}`",
                        config.agent.id
                    ),
                })?;
            let primary_value = credential.values.get(canonical_primary).ok_or_else(|| {
                StackError::SecretStorePlaintextInvalid {
                    reason: format!(
                        "provider credential `{provider_id}` is missing `{canonical_primary}`"
                    ),
                }
            })?;
            let mut env_names = Vec::new();
            insert_resolved_env(
                &mut env,
                &mut owners,
                emitted_primary,
                primary_value.clone(),
                &provider_id,
            )?;
            env_names.push(emitted_primary.to_owned());

            for env_name in companion_env_refs_for_agent_provider_id(&config.agent.id, &provider_id)
            {
                let value = credential.values.get(env_name).ok_or_else(|| {
                    StackError::SecretStorePlaintextInvalid {
                        reason: format!(
                            "provider credential `{provider_id}` is missing required companion `{env_name}`"
                        ),
                    }
                })?;
                insert_resolved_env(&mut env, &mut owners, env_name, value.clone(), &provider_id)?;
                env_names.push(env_name.to_owned());
            }
            for env_name in optional_env_refs_for_agent_provider_id(&config.agent.id, &provider_id)
            {
                if let Some(value) = credential.values.get(env_name) {
                    insert_resolved_env(
                        &mut env,
                        &mut owners,
                        env_name,
                        value.clone(),
                        &provider_id,
                    )?;
                    env_names.push(env_name.to_owned());
                }
            }
            env_names.sort();
            env_names.dedup();
            snapshots.push(ResolvedProviderSnapshot {
                provider_id,
                alias: alias.map(str::to_owned),
                revision: Some(credential.revision.clone()),
                env_names,
            });
            continue;
        }

        let provider = legacy_provider_config(&config.agent, &provider_id).ok_or_else(|| {
            StackError::InvalidParam {
                field: "provider",
                reason: format!(
                    "provider `{provider_id}` has no credential; add one with `acps agent provider credential add {provider_id}`"
                ),
            }
        })?;
        let api_key_ref = provider.api_key_ref.as_deref().ok_or_else(|| {
            StackError::InvalidParam {
                field: "agent.provider.api_key_ref",
                reason: format!(
                    "provider `{provider_id}` has no structured credential or legacy API-key ref"
                ),
            }
        })?;
        let mut env_names = required_env_refs_for_agent_provider_id(
            &config.agent.id,
            &provider_id,
            Some(api_key_ref),
        );
        for env_name in &env_names {
            if !env.contains_key(env_name) {
                return Err(StackError::InvalidParam {
                    field: "agent.env",
                    reason: format!(
                        "provider `{provider_id}` requires configured secret ref `{env_name}`"
                    ),
                });
            }
        }
        env_names.sort();
        env_names.dedup();
        snapshots.push(ResolvedProviderSnapshot {
            provider_id,
            alias: None,
            revision: None,
            env_names,
        });
    }

    snapshots.sort_by(|left, right| left.provider_id.cmp(&right.provider_id));
    Ok(ResolvedAgentEnvironment {
        env,
        providers: snapshots,
    })
}

fn legacy_provider_config<'a>(
    agent: &'a AgentConfig,
    provider_id: &str,
) -> Option<&'a AgentProviderConfig> {
    agent
        .provider
        .as_ref()
        .filter(|provider| provider.id == provider_id && provider.custom.is_none())
        .or_else(|| {
            agent
                .subagent
                .as_ref()
                .filter(|subagent| !subagent.disabled)
                .and_then(|subagent| subagent.provider.as_ref())
                .filter(|provider| provider.id == provider_id && provider.custom.is_none())
        })
}

fn custom_provider_config<'a>(
    agent: &'a AgentConfig,
    provider_id: &str,
) -> Option<&'a AgentProviderConfig> {
    agent
        .provider
        .as_ref()
        .filter(|provider| provider.id == provider_id && provider.custom.is_some())
        .or_else(|| {
            agent
                .subagent
                .as_ref()
                .filter(|subagent| !subagent.disabled)
                .and_then(|subagent| subagent.provider.as_ref())
                .filter(|provider| provider.id == provider_id && provider.custom.is_some())
        })
}

/// A hosted init soft-passes a custom provider's api-key ref expecting a
/// managed-state credential push after init; if the agent spawns before that
/// push lands, the raw `SecretNotFound` from the env loop would name the ref
/// but not the remediation. Name both.
fn remap_pending_provider_credential(
    config: &Config,
    entry: &str,
    error: StackError,
) -> StackError {
    if !matches!(error, StackError::SecretNotFound { .. })
        || crate::config::env_entry_var_name(entry) != entry
    {
        return error;
    }
    let pending_provider = effective_active_provider_ids(&config.agent)
        .into_iter()
        .filter(|provider_id| !provider_id_is_known(provider_id))
        .find(|provider_id| {
            agent_custom_provider_api_key_ref(&config.agent, provider_id) == Some(entry)
        });
    match pending_provider {
        Some(provider_id) => StackError::InvalidParam {
            field: "agent.provider.api_key_ref",
            reason: format!(
                "provider `{provider_id}` has no credential yet: `{entry}` is not in the secret store and no managed credential has been applied; push one through the managed-state extension or run `acps secrets set {entry}`"
            ),
        },
        None => error,
    }
}

fn insert_resolved_env(
    env: &mut HashMap<String, String>,
    owners: &mut HashMap<String, Vec<String>>,
    env_name: &str,
    value: String,
    owner: &str,
) -> Result<()> {
    if let Some(existing) = env.get(env_name) {
        if existing != &value {
            let mut conflict_owners = owners.get(env_name).cloned().unwrap_or_default();
            conflict_owners.push(owner.to_owned());
            conflict_owners.sort();
            conflict_owners.dedup();
            return Err(StackError::InvalidParam {
                field: "agent.providers.active",
                reason: format!(
                    "providers {} resolve different values for shared env `{env_name}`",
                    conflict_owners.join(", ")
                ),
            });
        }
        owners
            .entry(env_name.to_owned())
            .or_default()
            .push(owner.to_owned());
        return Ok(());
    }
    env.insert(env_name.to_owned(), value);
    owners.insert(env_name.to_owned(), vec![owner.to_owned()]);
    Ok(())
}

/// Apply one mapped provider to canonical Agent config. Init and native-config
/// import share this legacy-ref mutation; provider catalog commands use the
/// structured credential path above.
pub fn apply_mapped_agent_provider(
    config: &mut Config,
    provider_id: &str,
    requested_api_key_ref: Option<String>,
) -> Result<Vec<String>> {
    if !provider_id_is_known(provider_id)
        || !provider_id_supports_agent(provider_id, &config.agent.id)
    {
        return Err(StackError::InvalidParam {
            field: "provider",
            reason: format!(
                "provider `{provider_id}` is not supported for agent `{}`",
                config.agent.id
            ),
        });
    }
    let native_auth = provider_uses_agent_native_auth(&config.agent.id, provider_id);
    if native_auth && requested_api_key_ref.is_some() {
        return Err(StackError::InvalidParam {
            field: "api-key-ref",
            reason: format!(
                "provider `{provider_id}` uses agent-native auth and does not accept an API-key ref"
            ),
        });
    }
    let api_key_ref = requested_api_key_ref.or_else(|| {
        (!native_auth)
            .then(|| env_var_for_agent_provider_id(&config.agent.id, provider_id))
            .flatten()
            .map(str::to_owned)
    });
    if api_key_ref.is_none() && !native_auth {
        return Err(StackError::InvalidParam {
            field: "provider",
            reason: format!(
                "provider `{provider_id}` has no API-key env mapping for agent `{}`",
                config.agent.id
            ),
        });
    }
    let required_env_refs = required_env_refs_for_agent_provider_id(
        &config.agent.id,
        provider_id,
        api_key_ref.as_deref(),
    );
    for env_ref in &required_env_refs {
        if !crate::config::agent_env_declares(&config.agent.env, env_ref) {
            config.agent.env.push(env_ref.clone());
        }
    }
    config.agent.model = None;
    config.agent.provider = Some(AgentProviderConfig {
        id: provider_id.to_owned(),
        model: None,
        api_key_ref,
        custom: None,
    });
    if let Some(providers) = config.agent.providers.as_mut()
        && !providers.active.iter().any(|active| active == provider_id)
    {
        providers.active.push(provider_id.to_owned());
    }
    Ok(required_env_refs)
}

/// Apply a mapped provider when credentials are supplied by the structured
/// catalog rather than legacy `[agent].env` references.
pub fn apply_catalog_mapped_agent_provider(
    agent: &mut AgentConfig,
    provider_id: &str,
    multiple_active_providers: bool,
) -> Result<Vec<String>> {
    if !provider_id_is_known(provider_id) || !provider_id_supports_agent(provider_id, &agent.id) {
        return Err(StackError::InvalidParam {
            field: "provider",
            reason: format!(
                "provider `{provider_id}` is not supported for agent `{}`",
                agent.id
            ),
        });
    }
    let native_auth = provider_uses_agent_native_auth(&agent.id, provider_id);
    if !native_auth && env_var_for_agent_provider_id(&agent.id, provider_id).is_none() {
        return Err(StackError::InvalidParam {
            field: "provider",
            reason: format!(
                "provider `{provider_id}` has no API-key env mapping for agent `{}`",
                agent.id
            ),
        });
    }
    let required_env_refs = if native_auth {
        required_env_refs_for_agent_provider_id(&agent.id, provider_id, None)
    } else {
        Vec::new()
    };
    for env_ref in &required_env_refs {
        if !crate::config::agent_env_declares(&agent.env, env_ref) {
            agent.env.push(env_ref.clone());
        }
    }
    agent.model = None;
    agent.provider = Some(AgentProviderConfig {
        id: provider_id.to_owned(),
        model: None,
        api_key_ref: None,
        custom: None,
    });
    if let Some(providers) = agent.providers.as_mut() {
        if multiple_active_providers {
            if !providers.active.iter().any(|active| active == provider_id) {
                providers.active.push(provider_id.to_owned());
            }
        } else {
            providers.active = vec![provider_id.to_owned()];
        }
    }
    Ok(required_env_refs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AgentProviderConfig, AgentProvidersConfig, load_config_from_str};
    use crate::secrets::{ProviderCredential, ProviderCredentialSet};
    use std::collections::BTreeMap;

    fn resolver_config(agent_id: &str) -> Config {
        load_config_from_str(&format!(
            r#"
[api]
bind = "127.0.0.1:7700"
public_url = "http://127.0.0.1:7700"
max_request_bytes = 104857600

[security.http]
max_request_bytes = 104857600
rate_limit_per_minute = 120
burst = 30
auth_failures_per_minute = 5
auth_block_duration = "15m"
allowed_origins = []
trust_proxy_headers = false

[workspace]
root = "/workspace"
uploads = "/workspace/uploads"
default_shell = "/bin/bash"
runtime_user = "acp"
max_file_bytes = 8388608

[logging]
level = "info"
local_retention_days = 30

[logging.supabase]
enabled = false
url = "https://example.supabase.co"
api_key_ref = "SUPABASE_SECRET_KEY"
schema = "acp_stack"

[agent]
id = "{agent_id}"
name = "Test Agent"
command = "{agent_id}"
args = []
cwd = "/workspace"
env = []
restart = "on-crash"
"#
        ))
        .expect("config parses")
    }

    fn mapped_provider(provider_id: &str, api_key_ref: Option<&str>) -> AgentProviderConfig {
        AgentProviderConfig {
            id: provider_id.to_owned(),
            model: None,
            api_key_ref: api_key_ref.map(str::to_owned),
            custom: None,
        }
    }

    fn credential(env_name: &str, value: &str) -> ProviderCredential {
        ProviderCredential::new(
            BTreeMap::from([(env_name.to_owned(), value.to_owned())]),
            BTreeMap::new(),
        )
    }

    fn catalog_store(
        catalog: BTreeMap<String, ProviderCredentialSet>,
    ) -> (tempfile::TempDir, SecretStore) {
        let home = tempfile::tempdir().expect("home");
        let mut store = SecretStore::open_or_create(home.path()).expect("secret store");
        store
            .replace_provider_credentials(catalog, &[])
            .expect("catalog");
        (home, store)
    }

    #[test]
    fn structured_provider_environment_resolves_selected_aliases() {
        let mut config = resolver_config("opencode");
        config.agent.provider = Some(mapped_provider("opencode-go", None));
        config.agent.providers = Some(AgentProvidersConfig {
            active: vec!["opencode-go".to_owned(), "openrouter".to_owned()],
            selected_aliases: BTreeMap::from([("opencode-go".to_owned(), "go_2".to_owned())]),
        });
        let (_home, store) = catalog_store(BTreeMap::from([
            (
                "opencode-go".to_owned(),
                ProviderCredentialSet::promoted(BTreeMap::from([(
                    "go_2".to_owned(),
                    credential("OPENCODE_API_KEY", "go-key"),
                )])),
            ),
            (
                "openrouter".to_owned(),
                ProviderCredentialSet::aliasless(credential("OPENROUTER_API_KEY", "router-key")),
            ),
        ]));

        let resolved = resolve_agent_environment(&config, &store).expect("resolve");

        assert_eq!(resolved.env["OPENCODE_API_KEY"], "go-key");
        assert_eq!(resolved.env["OPENROUTER_API_KEY"], "router-key");
        assert_eq!(resolved.providers.len(), 2);
        assert_eq!(resolved.providers[0].alias.as_deref(), Some("go_2"));
        assert!(
            resolved
                .providers
                .iter()
                .all(|provider| provider.revision.is_some())
        );
    }

    #[test]
    fn templated_agent_env_resolves_var_name_and_composed_value() {
        let mut config = resolver_config("opencode");
        config.agent.env = vec![
            "PLAIN_TOKEN".to_owned(),
            "AUTH_HEADER=Bearer ${RELAY_TOKEN}".to_owned(),
        ];
        let home = tempfile::tempdir().expect("home");
        let mut store = SecretStore::open_or_create(home.path()).expect("secret store");
        store
            .set_many([("PLAIN_TOKEN", "plain"), ("RELAY_TOKEN", "tok-1")])
            .expect("set secrets");

        let resolved = resolve_agent_environment(&config, &store).expect("resolve");

        assert_eq!(resolved.env["PLAIN_TOKEN"], "plain");
        assert_eq!(resolved.env["AUTH_HEADER"], "Bearer tok-1");
        assert!(!resolved.env.contains_key("RELAY_TOKEN"));
    }

    #[test]
    fn shared_env_deduplicates_equal_values_and_rejects_different_values() {
        let mut config = resolver_config("opencode");
        config.agent.provider = Some(mapped_provider("opencode", None));
        config.agent.providers = Some(AgentProvidersConfig {
            active: vec!["opencode".to_owned(), "opencode-go".to_owned()],
            selected_aliases: BTreeMap::new(),
        });
        let (_home, store) = catalog_store(BTreeMap::from([
            (
                "opencode".to_owned(),
                ProviderCredentialSet::aliasless(credential("OPENCODE_API_KEY", "shared")),
            ),
            (
                "opencode-go".to_owned(),
                ProviderCredentialSet::aliasless(credential("OPENCODE_API_KEY", "shared")),
            ),
        ]));
        let resolved = resolve_agent_environment(&config, &store).expect("equal values resolve");
        assert_eq!(resolved.env.len(), 1);
        assert_eq!(resolved.providers.len(), 2);

        let (_home, store) = catalog_store(BTreeMap::from([
            (
                "opencode".to_owned(),
                ProviderCredentialSet::aliasless(credential("OPENCODE_API_KEY", "first")),
            ),
            (
                "opencode-go".to_owned(),
                ProviderCredentialSet::aliasless(credential("OPENCODE_API_KEY", "second")),
            ),
        ]));
        let error = resolve_agent_environment(&config, &store).expect_err("collision");
        let message = error.to_string();
        assert!(message.contains("opencode"));
        assert!(message.contains("opencode-go"));
        assert!(message.contains("OPENCODE_API_KEY"));
        assert!(!message.contains("first"));
        assert!(!message.contains("second"));
    }

    #[test]
    fn legacy_flat_ref_remains_the_implicit_single_provider() {
        let mut config = resolver_config("opencode");
        config.agent.env.push("LEGACY_GO_KEY".to_owned());
        config.agent.provider = Some(mapped_provider("opencode-go", Some("LEGACY_GO_KEY")));
        let home = tempfile::tempdir().expect("home");
        let mut store = SecretStore::open_or_create(home.path()).expect("secret store");
        store.set("LEGACY_GO_KEY", "legacy").expect("legacy secret");

        let resolved = resolve_agent_environment(&config, &store).expect("resolve");

        assert_eq!(resolved.env["LEGACY_GO_KEY"], "legacy");
        assert_eq!(resolved.providers.len(), 1);
        assert_eq!(resolved.providers[0].provider_id, "opencode-go");
        assert!(resolved.providers[0].revision.is_none());

        config.agent.env.clear();
        let error = resolve_agent_environment(&config, &store).expect_err("missing legacy ref");
        assert!(error.to_string().contains("LEGACY_GO_KEY"));
    }

    #[test]
    fn secretless_resolution_skips_store_only_for_empty_or_native_auth_envs() {
        let config = resolver_config("amp");
        let resolved =
            resolve_agent_environment_without_secrets(&config).expect("empty environment");
        assert!(resolved.env.is_empty());
        assert!(resolved.providers.is_empty());

        let mut config = resolver_config("codex");
        config.agent.provider = Some(mapped_provider("openai", None));
        let resolved = resolve_agent_environment_without_secrets(&config).expect("native auth");
        assert_eq!(resolved.providers[0].provider_id, "openai");

        let mut config = resolver_config("opencode");
        config.agent.provider = Some(mapped_provider("opencode-go", None));
        assert!(resolve_agent_environment_without_secrets(&config).is_none());
    }

    #[test]
    fn native_auth_snapshot_reports_injected_profile_environment() {
        let mut config = resolver_config("claude-code");
        config.agent.provider = Some(mapped_provider("google-vertex-anthropic", None));
        config.agent.env = vec![
            "ANTHROPIC_VERTEX_PROJECT_ID".to_owned(),
            "CLOUD_ML_REGION".to_owned(),
        ];
        let home = tempfile::tempdir().expect("home");
        let mut store = SecretStore::open_or_create(home.path()).expect("secret store");
        store
            .set_many([
                ("ANTHROPIC_VERTEX_PROJECT_ID", "project"),
                ("CLOUD_ML_REGION", "region"),
            ])
            .expect("profile secrets");

        let resolved = resolve_agent_environment(&config, &store).expect("resolve native auth");

        assert_eq!(
            resolved.providers[0].env_names,
            ["ANTHROPIC_VERTEX_PROJECT_ID", "CLOUD_ML_REGION"]
        );
    }

    #[test]
    fn subagent_provider_structured_key_resolves_without_active_block() {
        // Guards the subagent-discovery-auth fix: once the subagent provider is
        // registered (which `configure_mapped_subagent` now does before model
        // discovery), its structured credential resolves into the probe env
        // even with no `[agent.providers]` active block.
        use crate::config::AgentSubagentConfig;
        let mut config = resolver_config("opencode");
        config.agent.provider = Some(mapped_provider("openai", None));
        config.agent.subagent = Some(AgentSubagentConfig {
            disabled: false,
            provider: Some(mapped_provider("opencode-go", None)),
        });
        let (_home, store) = catalog_store(BTreeMap::from([
            (
                "openai".to_owned(),
                ProviderCredentialSet::aliasless(credential("OPENAI_API_KEY", "openai-key")),
            ),
            (
                "opencode-go".to_owned(),
                ProviderCredentialSet::aliasless(credential("OPENCODE_API_KEY", "go-key")),
            ),
        ]));

        let resolved = resolve_agent_environment(&config, &store).expect("resolve");

        assert_eq!(resolved.env["OPENCODE_API_KEY"], "go-key");
        assert_eq!(resolved.env["OPENAI_API_KEY"], "openai-key");
        assert!(
            resolved
                .providers
                .iter()
                .any(|provider| provider.provider_id == "opencode-go")
        );
    }

    #[test]
    fn custom_provider_appears_in_resolved_snapshot() {
        let mut config = resolver_config("opencode");
        config.agent.env = vec!["CUSTOM_KEY".to_owned()];
        config.agent.provider = Some(custom_provider("my-custom", "CUSTOM_KEY"));
        let home = tempfile::tempdir().expect("home");
        let mut store = SecretStore::open_or_create(home.path()).expect("secret store");
        store.set("CUSTOM_KEY", "custom-secret").expect("flat key");

        let resolved = resolve_agent_environment(&config, &store).expect("resolve");

        let snapshot = resolved
            .providers
            .iter()
            .find(|provider| provider.provider_id == "my-custom")
            .expect("custom provider snapshot present");
        assert_eq!(snapshot.env_names, vec!["CUSTOM_KEY".to_owned()]);
        assert!(snapshot.revision.is_none());
        assert!(snapshot.alias.is_none());
    }

    fn custom_provider(provider_id: &str, api_key_ref: &str) -> AgentProviderConfig {
        use crate::config::{AgentCustomProviderConfig, CustomProviderApi};
        AgentProviderConfig {
            id: provider_id.to_owned(),
            model: None,
            api_key_ref: Some(api_key_ref.to_owned()),
            custom: Some(AgentCustomProviderConfig {
                name: "My Custom".to_owned(),
                base_url: "https://example.test/v1".to_owned(),
                api: CustomProviderApi::default(),
                model_name: None,
                context: 128_000,
                output_max_tokens: 8_192,
            }),
        }
    }

    #[test]
    fn satisfiability_predicate_mirrors_catalog_injection() {
        // opencode emits CLOUDFLARE_API_TOKEN while the canonical catalog key
        // is CLOUDFLARE_API_KEY; the emitted name must satisfy off the
        // canonical value, exactly as resolve injects it.
        let mut config = resolver_config("opencode");
        config.agent.provider = Some(mapped_provider("cloudflare-ai-gateway", None));
        let (_home, store) = catalog_store(BTreeMap::from([(
            "cloudflare-ai-gateway".to_owned(),
            ProviderCredentialSet::aliasless(credential("CLOUDFLARE_API_KEY", "cf-key")),
        )]));
        let emitted = env_var_for_agent_provider_id("opencode", "cloudflare-ai-gateway")
            .expect("emitted var");
        assert_eq!(emitted, "CLOUDFLARE_API_TOKEN");
        assert!(env_ref_is_satisfiable_for_config(
            &config,
            &store,
            "cloudflare-ai-gateway",
            emitted
        ));
        // Companion refs are keyed by their own names and absent here.
        assert!(!env_ref_is_satisfiable_for_config(
            &config,
            &store,
            "cloudflare-ai-gateway",
            "CLOUDFLARE_ACCOUNT_ID"
        ));
        assert!(!env_ref_is_satisfiable_for_config(
            &config,
            &store,
            "cloudflare-ai-gateway",
            "UNRELATED_KEY"
        ));

        // A promoted set with no selected alias errors at resolve time, so it
        // must not satisfy the gate.
        let (_home, store) = catalog_store(BTreeMap::from([(
            "opencode-go".to_owned(),
            ProviderCredentialSet::promoted(BTreeMap::from([(
                "go_1".to_owned(),
                credential("OPENCODE_API_KEY", "go-key"),
            )])),
        )]));
        let mut config = resolver_config("opencode");
        config.agent.provider = Some(mapped_provider("opencode-go", None));
        assert!(!env_ref_is_satisfiable_for_config(
            &config,
            &store,
            "opencode-go",
            "OPENCODE_API_KEY"
        ));
    }

    #[test]
    fn satisfiability_predicate_matches_custom_provider_api_key_ref_only() {
        let mut config = resolver_config("opencode");
        config.agent.provider = Some(custom_provider("my-custom", "CUSTOM_KEY"));
        let (_home, store) = catalog_store(BTreeMap::from([(
            "my-custom".to_owned(),
            ProviderCredentialSet::aliasless(credential("CUSTOM_KEY", "custom-secret")),
        )]));
        assert!(env_ref_is_satisfiable_for_config(
            &config,
            &store,
            "my-custom",
            "CUSTOM_KEY"
        ));
        assert!(!env_ref_is_satisfiable_for_config(
            &config,
            &store,
            "my-custom",
            "OTHER_KEY"
        ));

        // Credential keyed by a name other than the configured ref does not
        // satisfy — resolve would not inject it.
        let (_home, store) = catalog_store(BTreeMap::from([(
            "my-custom".to_owned(),
            ProviderCredentialSet::aliasless(credential("OTHER_KEY", "custom-secret")),
        )]));
        assert!(!env_ref_is_satisfiable_for_config(
            &config,
            &store,
            "my-custom",
            "CUSTOM_KEY"
        ));
    }

    #[test]
    fn custom_provider_resolves_catalog_credential_with_revision() {
        let mut config = resolver_config("opencode");
        config.agent.env = vec!["CUSTOM_KEY".to_owned()];
        config.agent.provider = Some(custom_provider("my-custom", "CUSTOM_KEY"));
        let (_home, store) = catalog_store(BTreeMap::from([(
            "my-custom".to_owned(),
            ProviderCredentialSet::aliasless(credential("CUSTOM_KEY", "catalog-secret")),
        )]));

        let resolved = resolve_agent_environment(&config, &store).expect("resolve");

        assert_eq!(resolved.env["CUSTOM_KEY"], "catalog-secret");
        let snapshot = resolved
            .providers
            .iter()
            .find(|provider| provider.provider_id == "my-custom")
            .expect("custom provider snapshot present");
        assert_eq!(snapshot.env_names, vec!["CUSTOM_KEY".to_owned()]);
        assert!(snapshot.revision.is_some());
    }

    #[test]
    fn catalog_credential_shadows_bare_agent_env_ref() {
        // Regression: a catalog-only credential with the ref still declared in
        // `agent.env` (as init writes it) must not fail on the flat store.
        let mut config = resolver_config("opencode");
        config.agent.env = vec!["OPENCODE_API_KEY".to_owned()];
        config.agent.provider = Some(mapped_provider("opencode-go", None));
        let (_home, store) = catalog_store(BTreeMap::from([(
            "opencode-go".to_owned(),
            ProviderCredentialSet::aliasless(credential("OPENCODE_API_KEY", "catalog-key")),
        )]));
        let resolved = resolve_agent_environment(&config, &store).expect("resolve");
        assert_eq!(resolved.env["OPENCODE_API_KEY"], "catalog-key");

        // Precedence: with a differing flat secret present, the catalog value
        // wins and there is no owner conflict.
        let (_home, mut store) = catalog_store(BTreeMap::from([(
            "opencode-go".to_owned(),
            ProviderCredentialSet::aliasless(credential("OPENCODE_API_KEY", "rotated-key")),
        )]));
        store
            .set("OPENCODE_API_KEY", "stale-flat-key")
            .expect("flat");
        let resolved = resolve_agent_environment(&config, &store).expect("resolve");
        assert_eq!(resolved.env["OPENCODE_API_KEY"], "rotated-key");
    }

    #[test]
    fn templated_agent_env_keeps_flat_semantics_when_catalog_covers_var() {
        let mut config = resolver_config("opencode");
        config.agent.env = vec!["OPENCODE_API_KEY=prefix-${RELAY_TOKEN}".to_owned()];
        config.agent.provider = Some(mapped_provider("opencode-go", None));
        let (_home, mut store) = catalog_store(BTreeMap::from([(
            "opencode-go".to_owned(),
            ProviderCredentialSet::aliasless(credential("OPENCODE_API_KEY", "prefix-tok-1")),
        )]));
        store.set("RELAY_TOKEN", "tok-1").expect("flat");
        let resolved = resolve_agent_environment(&config, &store).expect("resolve");
        // The template composed the same value; a differing template value
        // would surface as an owner conflict, which is the intended guard.
        assert_eq!(resolved.env["OPENCODE_API_KEY"], "prefix-tok-1");
    }

    #[test]
    fn custom_provider_without_any_credential_reports_pending_error() {
        let mut config = resolver_config("opencode");
        config.agent.env = vec!["CUSTOM_KEY".to_owned()];
        config.agent.provider = Some(custom_provider("my-custom", "CUSTOM_KEY"));
        let home = tempfile::tempdir().expect("home");
        let store = SecretStore::open_or_create(home.path()).expect("secret store");

        let error = resolve_agent_environment(&config, &store).expect_err("pending");
        let message = error.to_string();
        assert!(message.contains("my-custom"));
        assert!(message.contains("CUSTOM_KEY"));
        assert!(message.contains("managed"));
    }
}
