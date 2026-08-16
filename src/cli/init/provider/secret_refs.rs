//! Secret-ref collection for the init provider lanes.
//!
//! Two directions live here: gathering the refs a config declares (provider
//! api keys, MCP env/header refs, S3 data-source keys) and prompting for the
//! ones the store lacks, plus the custom-provider credential checks the later
//! init lanes consult before they try to spawn the agent.

use super::*;

/// The secret refs that must exist in the store for env var `var_name` to
/// resolve: the entry itself when declared bare, the template's inner refs
/// when declared as `VAR=template`, and the var name as a plain ref when the
/// env list does not declare it at all (the pre-template behavior).
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
    // MCP refs stay flat-store-gated with a hard failure: only provider api
    // keys are delivered through the credential catalog, and the hosted
    // soft-pass above must not leak to them.
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

/// Offer masked entry for MCP env/header refs and S3 data-source key refs the
/// config declares but the store lacks. Unlike `collect_missing_provider_refs`
/// this never hard-fails on a still-missing ref: the ordinary init path has
/// always deferred MCP secrets to runtime health (and S3 refs to workspace
/// materialization), and a hosted backend may legitimately skip a prompt here
/// and push the secret through the API after init completes. Returns the ref
/// names actually stored.
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
    // Bare agent.env refs are handled by the provider flow; only the inner
    // refs of `VAR=template` entries would otherwise never be prompted.
    for entry in &config.agent.env {
        if crate::config::env_entry_var_name(entry) != entry.as_str() {
            declared_refs.extend(crate::config::env_entry_ref_names_lossy(entry));
        }
    }
    prompt_missing_declared_refs(interactive, &declared_refs, secret_store)
}

/// MCP-only variant for the post-probe `mcp_configure` step: servers added
/// there land after the up-front declared-refs pass above already ran, so
/// their env/header refs would otherwise never be prompted.
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

/// Custom providers the primary agent will actually launch with: its own
/// provider and its enabled subagent's, as `(provider_id, api_key_ref)`.
/// Providers declared only on another array target are excluded — the init
/// lanes only ever spawn the primary agent.
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

fn provider_is_configured_custom(config: &Config, provider_id: &str) -> bool {
    configured_custom_providers(config).any(|(id, _)| id == provider_id)
}

/// True when the primary agent's own provider is custom. The custom-provider
/// flow writes a literal model id that no harness advertises, so the init model
/// lane has nothing to discover. Derived from config rather than
/// `args.custom_provider` so a rerun over an existing custom-provider config
/// reaches the same conclusion.
pub(crate) fn primary_provider_is_custom(config: &Config) -> bool {
    config
        .agent
        .provider
        .as_ref()
        .is_some_and(|provider| provider.custom.is_some())
}

/// The first configured custom provider whose api-key ref is neither in the
/// flat secret store nor in the credential catalog. Hosted init soft-passes
/// such a ref expecting a managed credential push after init (see
/// [`collect_missing_provider_refs`]), so every init lane that would spawn the
/// agent has to check for it: the spawn would otherwise fail on an environment
/// that is pending by design. The check is keyed on the configured ref alone,
/// so it can fire for a hand-edited config whose ref never entered
/// `[agent].env` even though that spawn would resolve; init always writes the
/// ref into `[agent].env`, so the conservative skip only affects such configs.
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

/// Shared remediation for a custom provider whose credential has not landed
/// yet; mirrors the spawn-time wording in `remap_pending_provider_credential`
/// so an operator sees one story regardless of which layer catches it first.
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
    // With a provider context the ref is satisfiable from the flat store or
    // the structured credential catalog; without one (MCP refs on the
    // prepared-config path) only the flat store counts.
    let satisfiable = |store: &SecretStore, env_ref: &str| match provider_id {
        Some(provider_id) => env_ref_is_satisfiable_for_config(config, store, provider_id, env_ref),
        None => store.contains(env_ref),
    };
    if interactive {
        let mut collected = Vec::new();
        for env_ref in required_refs {
            if satisfiable(secret_store, env_ref) {
                continue;
            }
            // Masked entry via the wizard: a provider API key is a secret value;
            // echoing it to the terminal (and scrollback) would defeat the
            // encrypted store.
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
        if !satisfiable(secret_store, env_ref) {
            // A hosted backend answers the value prompt with null by design
            // (plaintext never rides an input frame) and pushes the credential
            // through the managed-state extension after init completes, so a
            // provider ref missing here is deferred rather than fatal. Scoped
            // to configured custom providers: their init lanes skip every
            // later agent spawn, whereas a mapped provider's model-discovery
            // spawn would still hard-fail on the missing ref and turn the
            // deferral into a worse-attributed failure.
            let custom_provider = provider_id
                .is_some_and(|provider_id| provider_is_configured_custom(config, provider_id));
            if custom_provider && prompt::hosted_driver_active() {
                prompt::emit_progress(format!(
                    "provider secret `{env_ref}` not present yet; expecting a managed credential push after init"
                ));
                tracing::warn!(
                    env_ref = %env_ref,
                    "provider secret missing at init; deferring to a managed credential push"
                );
                continue;
            }
            return Err(StackError::SecretNotFound {
                name: env_ref.clone(),
            });
        }
    }
    Ok(())
}
