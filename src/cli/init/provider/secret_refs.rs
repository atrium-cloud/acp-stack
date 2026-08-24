//! Secret-ref collection and pending-credential checks for the init provider lanes.

use super::*;

/// The secret refs that must exist in the store for env var `var_name` to resolve:
/// the entry itself when bare, the template's inner refs for `VAR=template`, and
/// the var name itself when the env list does not declare it.
pub(crate) fn agent_env_secret_refs_for_var(env: &[String], var_name: &str) -> Vec<String> {
    env.iter()
        .find(|entry| crate::config::env_entry_var_name(entry) == var_name)
        .map(|entry| crate::config::env_entry_ref_names_lossy(entry))
        .unwrap_or_else(|| vec![var_name.to_owned()])
}

pub(crate) fn collect_prepared_secret_refs_for_init(
    args: &InitArgs,
    registry: &RegistryCatalog,
    config: &Config,
    config_path: &Path,
    secret_store: &mut SecretStore,
) -> Result<()> {
    validate_configured_provider_for_init(registry, config, config_path)?;
    if let Some(provider) = config.agent.provider.as_ref() {
        let provider_refs = required_env_refs_for_agent_provider_id(
            &config.agent.id,
            &provider.id,
            provider.api_key_ref.as_deref(),
        );
        if provider_refs
            .iter()
            .any(|name| !crate::config::agent_env_declares(&config.agent.env, name))
        {
            return Err(StackError::AgentConfigProvision {
                path: config_path.to_path_buf(),
                reason: "prepared native config provider omitted a required secret reference"
                    .to_owned(),
            });
        }
        let provider_required: BTreeSet<String> = provider_refs
            .iter()
            .flat_map(|name| agent_env_secret_refs_for_var(&config.agent.env, name))
            .collect();
        collect_missing_provider_refs(
            prompts_enabled(args),
            secret_store,
            config,
            Some(&provider.id),
            &provider_required.into_iter().collect::<Vec<_>>(),
        )?;
    }
    // MCP refs stay flat-store-gated with a hard failure: only provider api keys
    // ride the credential catalog, so the hosted soft-pass must not reach them.
    collect_missing_provider_refs(
        prompts_enabled(args),
        secret_store,
        config,
        None,
        &config_mcp_secret_refs(config)
            .into_iter()
            .collect::<Vec<_>>(),
    )
}

fn config_mcp_secret_refs(config: &Config) -> BTreeSet<String> {
    let mut refs = BTreeSet::new();
    for server in &config.mcp.servers {
        match server {
            crate::config::McpServerConfig::Stdio(server) => {
                refs.extend(
                    server
                        .env
                        .iter()
                        .flat_map(|entry| crate::config::env_entry_ref_names_lossy(entry)),
                );
            }
            crate::config::McpServerConfig::Http(server) => {
                refs.extend(
                    server
                        .headers
                        .iter()
                        .flat_map(|header| header.ref_names_lossy()),
                );
            }
        }
    }
    refs
}

/// Offer masked entry for declared MCP and S3 data-source refs the store lacks,
/// returning the ref names stored. A still-missing ref is never fatal here.
pub(crate) fn collect_declared_secret_refs_for_init(
    interactive: bool,
    config: &Config,
    secret_store: &mut SecretStore,
) -> Result<Vec<String>> {
    if !interactive {
        return Ok(Vec::new());
    }
    let mut declared_refs = config_mcp_secret_refs(config);
    for source in &config.workspace.data_sources {
        declared_refs.extend(source.access_key_ref.iter().cloned());
        declared_refs.extend(source.secret_key_ref.iter().cloned());
    }
    // Bare agent.env refs are handled by the provider flow; only `VAR=template`
    // inner refs would otherwise never be prompted.
    for entry in &config.agent.env {
        if crate::config::env_entry_var_name(entry) != entry.as_str() {
            declared_refs.extend(crate::config::env_entry_ref_names_lossy(entry));
        }
    }
    prompt_missing_declared_refs(interactive, &declared_refs, secret_store)
}

/// MCP-only variant for the post-probe `mcp_configure` step, whose servers land
/// after the up-front declared-refs pass has already run.
pub(crate) fn collect_mcp_secret_refs_for_init(
    interactive: bool,
    config: &Config,
    secret_store: &mut SecretStore,
) -> Result<Vec<String>> {
    if !interactive {
        return Ok(Vec::new());
    }
    let declared_refs = config_mcp_secret_refs(config);
    prompt_missing_declared_refs(interactive, &declared_refs, secret_store)
}

fn prompt_missing_declared_refs(
    interactive: bool,
    declared_refs: &BTreeSet<String>,
    secret_store: &mut SecretStore,
) -> Result<Vec<String>> {
    let mut collected = Vec::new();
    for env_ref in declared_refs {
        if secret_store.contains(env_ref) {
            continue;
        }
        let Some(value) = prompt::password(
            prompt::HostedPromptKind::SecretRefValue,
            interactive,
            env_ref,
            false,
        )?
        else {
            continue;
        };
        let value = zeroize::Zeroizing::new(value);
        if !value.is_empty() {
            collected.push((env_ref.clone(), value));
        }
    }
    if collected.is_empty() {
        return Ok(Vec::new());
    }
    secret_store.set_many(
        collected
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str())),
    )?;
    Ok(collected.into_iter().map(|(name, _)| name).collect())
}

/// Custom providers the primary agent will launch with (its own and its enabled
/// subagent's) as `(provider_id, api_key_ref)`; other array targets are excluded.
fn configured_custom_providers(config: &Config) -> impl Iterator<Item = (&str, &str)> {
    let subagent_provider = config
        .agent
        .subagent
        .as_ref()
        .filter(|subagent| !subagent.disabled)
        .and_then(|subagent| subagent.provider.as_ref());
    config
        .agent
        .provider
        .as_ref()
        .into_iter()
        .chain(subagent_provider)
        .filter(|provider| provider.custom.is_some())
        .filter_map(|provider| {
            provider
                .api_key_ref
                .as_deref()
                .map(|api_key_ref| (provider.id.as_str(), api_key_ref))
        })
}

/// True when the primary agent's own provider is custom, in which case the init
/// model lane has nothing to discover (the model id is a literal no harness lists).
pub(crate) fn primary_provider_is_custom(config: &Config) -> bool {
    config
        .agent
        .provider
        .as_ref()
        .is_some_and(|provider| provider.custom.is_some())
}

/// The first configured custom provider whose api-key ref is in neither the flat
/// store nor the credential catalog. Every init lane that would spawn the agent
/// must check this, since hosted init soft-passes such a ref by design.
pub(crate) fn pending_custom_provider_credential(
    config: &Config,
    secrets: &SecretStore,
) -> Option<(String, String)> {
    configured_custom_providers(config)
        .find(|(provider_id, api_key_ref)| {
            !env_ref_is_satisfiable_for_config(config, secrets, provider_id, api_key_ref)
        })
        .map(|(provider_id, api_key_ref)| (provider_id.to_owned(), api_key_ref.to_owned()))
}

/// Whether a managed-state credential push can satisfy this provider's refs after
/// init. Agent-native-auth providers cannot: apply rejects their id and the catalog
/// never covers their refs, so those refs must be collected or fail at init.
fn provider_credentials_are_push_deliverable(config: &Config, provider_id: &str) -> bool {
    if configured_custom_providers(config).any(|(id, _)| id == provider_id) {
        return true;
    }
    !provider_uses_agent_native_auth(&config.agent.id, provider_id)
}

/// The first provider credential this init run is waiting on, custom or mapped.
/// A mapped provider's refs only count as pending under `defer_provider_credentials`
/// and only when the push can deliver them; otherwise they already hard-failed in
/// [`collect_missing_provider_refs`].
pub(crate) fn pending_deferred_provider_credential(
    config: &Config,
    secrets: &SecretStore,
) -> Option<(String, String)> {
    if let Some(pending) = pending_custom_provider_credential(config, secrets) {
        return Some(pending);
    }
    if !prompt::defer_provider_credentials() {
        return None;
    }
    let provider = config.agent.provider.as_ref()?;
    if provider.custom.is_some() || !provider_credentials_are_push_deliverable(config, &provider.id)
    {
        return None;
    }
    required_env_refs_for_agent_provider_id(
        &config.agent.id,
        &provider.id,
        provider.api_key_ref.as_deref(),
    )
    .iter()
    .flat_map(|name| agent_env_secret_refs_for_var(&config.agent.env, name))
    .find(|env_ref| {
        push_delivers_env_ref_for_config(config, &provider.id, env_ref)
            && !env_ref_is_satisfiable_for_config(config, secrets, &provider.id, env_ref)
    })
    .map(|env_ref| (provider.id.clone(), env_ref))
}

/// Shared remediation text for a provider whose credential has not landed yet;
/// mirrors the spawn-time wording in `remap_pending_provider_credential`.
pub(crate) fn pending_provider_credential_reason(provider_id: &str, api_key_ref: &str) -> String {
    format!(
        "provider `{provider_id}` has no credential yet: `{api_key_ref}` is not in the secret store and no managed credential has been applied; push one through the managed-state extension or run `acps secrets set {api_key_ref}`"
    )
}

pub(crate) fn collect_missing_provider_refs(
    interactive: bool,
    secret_store: &mut SecretStore,
    config: &Config,
    provider_id: Option<&str>,
    required_refs: &[String],
) -> Result<()> {
    // Prompting, storing, and the satisfiability check must all target the refs
    // runtime actually resolves, or an answered prompt for `OPENAI_API_KEY=${MY_KEY}`
    // stores under `OPENAI_API_KEY`, passes validation, and still fails at start.
    let required_refs: Vec<String> = required_refs
        .iter()
        .flat_map(|name| agent_env_secret_refs_for_var(&config.agent.env, name))
        .collect();
    let required_refs = required_refs.as_slice();
    // With a provider context the ref may come from the flat store or the
    // credential catalog; without one, only the flat store counts.
    let satisfiable = |store: &SecretStore, env_ref: &str| match provider_id {
        Some(provider_id) => env_ref_is_satisfiable_for_config(config, store, provider_id, env_ref),
        None => store.contains(env_ref),
    };
    // Gated on the explicit declaration, not on the presence of a hosted driver:
    // a driven init that made no such promise keeps the prompt and the hard failure.
    let defer_declared = prompt::defer_provider_credentials();
    let defer_provider: Option<&str> = provider_id.filter(|provider_id| {
        defer_declared && provider_credentials_are_push_deliverable(config, provider_id)
    });
    // Per ref, not per provider: a `VAR=template` inner ref or a noncanonical
    // api-key alias is never push-deliverable and must fall through.
    let deferrable = |env_ref: &str| {
        defer_provider.is_some_and(|provider_id| {
            push_delivers_env_ref_for_config(config, provider_id, env_ref)
        })
    };
    if interactive {
        let mut collected = Vec::new();
        for env_ref in required_refs {
            if deferrable(env_ref) || satisfiable(secret_store, env_ref) {
                continue;
            }
            // Masked entry: echoing an API key to the terminal and its scrollback
            // would defeat the encrypted store.
            let Some(value) = prompt::password(
                prompt::HostedPromptKind::ProviderApiKeyValue,
                interactive,
                env_ref,
                true,
            )?
            else {
                continue;
            };
            let value = zeroize::Zeroizing::new(value);
            if !value.is_empty() {
                collected.push((env_ref.as_str(), value));
            }
        }
        secret_store.set_many(
            collected
                .iter()
                .map(|(name, value)| (*name, value.as_str())),
        )?;
    }
    for env_ref in required_refs {
        if satisfiable(secret_store, env_ref) {
            continue;
        }
        if deferrable(env_ref) {
            prompt::emit_progress(format!(
                "provider secret `{env_ref}` not present yet; expecting a managed credential push after init"
            ));
            tracing::warn!(
                env_ref = %env_ref,
                "provider secret missing at init; deferring to a managed credential push"
            );
            continue;
        }
        // Under a declared deferral, name why the push cannot cover this ref rather
        // than emit a bare "not found" that reads as a broken promise.
        match provider_id {
            Some(provider_id) if defer_declared => {
                return Err(StackError::ProviderSecretNotPushDeliverable {
                    provider_id: provider_id.to_owned(),
                    env_ref: env_ref.clone(),
                });
            }
            _ => {
                return Err(StackError::SecretNotFound {
                    name: env_ref.clone(),
                });
            }
        }
    }
    Ok(())
}
