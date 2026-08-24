//! Resolving the active provider set into the agent's launch environment, and the
//! inverse: writing a chosen provider back into canonical `[agent]` config.

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

/// Which slot of a provider's managed credential set an env ref maps to. Shared by
/// [`catalog_covers_env_ref`] and [`push_delivers_env_ref_for_config`] so the two
/// cannot drift.
enum ProviderEnvRefSlot {
    CustomApiKey,
    /// Delivered by the push under the canonical store name `canonical_primary`.
    MappedPrimary {
        canonical_primary: &'static str,
    },
    MappedCompanion,
    Undeliverable,
}

/// Classify `env_ref` against `provider_id`'s managed credential set.
fn classify_provider_env_ref(
    agent_id: &str,
    provider_id: &str,
    configured_api_key_ref: Option<&str>,
    env_ref: &str,
) -> ProviderEnvRefSlot {
    if !provider_id_is_known(provider_id) {
        return if configured_api_key_ref == Some(env_ref) {
            ProviderEnvRefSlot::CustomApiKey
        } else {
            ProviderEnvRefSlot::Undeliverable
        };
    }
    if provider_uses_agent_native_auth(agent_id, provider_id) {
        return ProviderEnvRefSlot::Undeliverable;
    }
    let Some(canonical_primary) = env_var_for_provider_id(provider_id) else {
        return ProviderEnvRefSlot::Undeliverable;
    };
    if env_var_for_agent_provider_id(agent_id, provider_id) == Some(env_ref) {
        return ProviderEnvRefSlot::MappedPrimary { canonical_primary };
    }
    let companion =
        companion_env_refs_for_agent_provider_id(agent_id, provider_id).contains(&env_ref);
    let optional =
        optional_env_refs_for_agent_provider_id(agent_id, provider_id).contains(&env_ref);
    if companion || optional {
        ProviderEnvRefSlot::MappedCompanion
    } else {
        ProviderEnvRefSlot::Undeliverable
    }
}

/// True when the credential catalog will inject `env_ref` for `provider_id` at
/// spawn time. Must stay an exact mirror of the injection logic in
/// [`resolve_agent_environment`]: the init secret gate relies on a passing gate
/// implying a resolvable spawn environment.
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
    match classify_provider_env_ref(agent_id, provider_id, configured_api_key_ref, env_ref) {
        ProviderEnvRefSlot::CustomApiKey | ProviderEnvRefSlot::MappedCompanion => {
            credential.values.contains_key(env_ref)
        }
        ProviderEnvRefSlot::MappedPrimary { canonical_primary } => {
            credential.values.contains_key(canonical_primary)
        }
        ProviderEnvRefSlot::Undeliverable => false,
    }
}

/// Whether a managed-state credential push for `provider_id` would place `env_ref`
/// into the agent's spawn environment. Init may soft-pass a missing ref only when
/// this holds, or it would report success on a config the agent can never resolve.
pub fn push_delivers_env_ref_for_config(config: &Config, provider_id: &str, env_ref: &str) -> bool {
    matches!(
        classify_provider_env_ref(
            &config.agent.id,
            provider_id,
            agent_custom_provider_api_key_ref(&config.agent, provider_id),
            env_ref,
        ),
        ProviderEnvRefSlot::CustomApiKey
            | ProviderEnvRefSlot::MappedPrimary { .. }
            | ProviderEnvRefSlot::MappedCompanion
    )
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

/// Config-derived wrapper over [`env_ref_is_satisfiable`]. The custom lookup stays
/// agent-local: a custom provider declared only on another array target must not
/// satisfy here, because the injection below would not use it.
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

/// The api-key ref of a custom provider configured anywhere in the config, used by
/// managed-state apply as the env contract for ids outside the registry mapping.
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

/// Env vars the credential catalog will inject at spawn for the active provider
/// set; these win over a bare `[agent].env` ref of the same name.
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

/// Callers compare snapshots by value, so env-name ordering is normalized here
/// rather than at every construction site.
fn provider_snapshot(
    provider_id: String,
    alias: Option<&str>,
    revision: Option<&str>,
    mut env_names: Vec<String>,
) -> ResolvedProviderSnapshot {
    env_names.sort();
    env_names.dedup();
    ResolvedProviderSnapshot {
        provider_id,
        alias: alias.map(str::to_owned),
        revision: revision.map(str::to_owned),
        env_names,
    }
}

/// Both credential branches reject a missing selected alias identically; only the
/// promoted-set hint differs, since `credential select` refuses non-mapped ids.
fn missing_selected_alias_error(provider_id: &str, promoted_hint: Option<String>) -> StackError {
    let reason = promoted_hint.unwrap_or_else(|| {
        format!("selected credential alias for provider `{provider_id}` does not exist")
    });
    StackError::InvalidParam {
        field: "agent.providers.selected_aliases",
        reason,
    }
}

pub fn resolve_agent_environment(
    config: &Config,
    secrets: &SecretStore,
) -> Result<ResolvedAgentEnvironment> {
    let mut env = HashMap::with_capacity(config.agent.env.len());
    let mut owners: HashMap<String, Vec<String>> = HashMap::new();
    let catalog_owned = catalog_owned_env_vars(config, secrets);
    for entry in &config.agent.env {
        // A bare ref the catalog injects is skipped so the managed rotation channel
        // wins; templated `VAR=...` entries keep flat-store semantics.
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
            // Custom (BYOK) providers take their api-key ref from the catalog when a
            // managed credential exists, else from the `[agent].env` loop above.
            if let Some(provider) = custom_provider_config(&config.agent, &provider_id) {
                let api_key_ref = provider.api_key_ref.as_deref();
                if let (Some(api_key_ref), Some(credentials)) =
                    (api_key_ref, secrets.provider_credential_set(&provider_id))
                {
                    let selected_alias = selected_alias_for(config, &provider_id);
                    let Some((credential, alias)) = credentials.selected(selected_alias) else {
                        return Err(missing_selected_alias_error(
                            &provider_id,
                            (credentials.is_promoted() && selected_alias.is_none()).then(|| {
                                format!(
                                    "custom provider `{provider_id}` has backup credential aliases and none is selected in agent.providers.selected_aliases"
                                )
                            }),
                        ));
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
                    snapshots.push(provider_snapshot(
                        provider_id,
                        alias,
                        Some(&credential.revision),
                        vec![api_key_ref.to_owned()],
                    ));
                    continue;
                }
                let env_names: Vec<String> = provider
                    .api_key_ref
                    .iter()
                    .filter(|name| env.contains_key(name.as_str()))
                    .cloned()
                    .collect();
                snapshots.push(provider_snapshot(provider_id, None, None, env_names));
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
            snapshots.push(provider_snapshot(provider_id, None, None, env_names));
            continue;
        }

        if let Some(credentials) = secrets.provider_credential_set(&provider_id) {
            let selected_alias = selected_alias_for(config, &provider_id);
            let Some((credential, alias)) = credentials.selected(selected_alias) else {
                return Err(missing_selected_alias_error(
                    &provider_id,
                    (credentials.is_promoted() && selected_alias.is_none()).then(|| {
                        format!(
                            "provider `{provider_id}` has backup aliases; select one with `acps agent provider credential select {provider_id} <alias>`"
                        )
                    }),
                ));
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
            snapshots.push(provider_snapshot(
                provider_id,
                alias,
                Some(&credential.revision),
                env_names,
            ));
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
        let env_names = required_env_refs_for_agent_provider_id(
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
        snapshots.push(provider_snapshot(provider_id, None, None, env_names));
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

/// Restate a raw `SecretNotFound` for a provider ref that hosted init soft-passed,
/// so the spawn failure names the pending managed-credential push as remediation.
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
        .find(|provider_id| {
            if provider_id_is_known(provider_id) {
                legacy_provider_config(&config.agent, provider_id)
                    .and_then(|provider| provider.api_key_ref.as_deref())
                    == Some(entry)
            } else {
                agent_custom_provider_api_key_ref(&config.agent, provider_id) == Some(entry)
            }
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

/// Apply one mapped provider to canonical Agent config via legacy `[agent].env` refs.
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
    reconcile_kimi_lane_env_declarations(&mut config.agent);
    Ok(required_env_refs)
}

/// Drop kimi-lane env refs the active provider does not require: the launch env
/// resolves every `[agent].env` entry, so a leftover ref from a previously selected
/// Kimi lane fails the launch on a secret the active lane never uses.
pub fn reconcile_kimi_lane_env_declarations(agent: &mut AgentConfig) {
    if agent.id != crate::runtime::agent::acp_bridge::KIMI_CODE_AGENT_ID {
        return;
    }
    let lane_refs: BTreeSet<&'static str> = providers_for_agent(&agent.id)
        .into_iter()
        .filter_map(|summary| summary.default_api_key_ref)
        .collect();
    let active_ref = agent
        .provider
        .as_ref()
        .and_then(|provider| {
            provider.api_key_ref.clone().or_else(|| {
                env_var_for_agent_provider_id(&agent.id, &provider.id).map(str::to_owned)
            })
        })
        .unwrap_or_else(|| crate::runtime::agent::acp_bridge::KIMI_API_KEY_ENV.to_owned());
    agent.env.retain(|entry| {
        let name = crate::config::env_entry_var_name(entry);
        name == active_ref || !lane_refs.contains(name)
    });
    if !crate::config::agent_env_declares(&agent.env, &active_ref) {
        agent.env.push(active_ref);
    }
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
    reconcile_kimi_lane_env_declarations(agent);
    Ok(required_env_refs)
}

#[cfg(test)]
mod tests;
