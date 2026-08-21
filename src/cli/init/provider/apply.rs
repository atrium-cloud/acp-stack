//! Writing a chosen provider into `[agent.provider]`, and settling an already
//! configured one: filling in the default api-key ref, declaring the required
//! env refs, and collecting the secrets they name.

use super::*;

pub(crate) fn ensure_configured_provider_refs_for_init(
    args: &InitArgs,
    registry: &RegistryCatalog,
    config: &mut Config,
    config_path: &Path,
    secret_store: &mut SecretStore,
) -> Result<bool> {
    let Some(provider) = config.agent.provider.as_mut() else {
        return Ok(false);
    };
    let mut api_key_ref_changed = false;
    if provider.api_key_ref.is_none()
        && !provider_uses_agent_native_auth(&config.agent.id, &provider.id)
    {
        let entry = registry.lookup_required(&config.agent.id)?;
        let Some(default_api_key_ref) =
            env_var_for_agent_provider_id(&config.agent.id, &provider.id)
        else {
            return Err(StackError::AgentConfigProvision {
                path: config_path.to_path_buf(),
                reason: format!(
                    "{} provider `{}` is missing agent.provider.api_key_ref",
                    entry.name, provider.id
                ),
            });
        };
        provider.api_key_ref = Some(default_api_key_ref.to_owned());
        api_key_ref_changed = true;
    }
    let provider_id = provider.id.clone();
    let provider_api_key_ref = provider.api_key_ref.clone();
    let required_refs = required_env_refs_for_agent_provider_id(
        &config.agent.id,
        &provider_id,
        provider_api_key_ref.as_deref(),
    );
    let mut env_changed = false;
    for env_ref in &required_refs {
        if !crate::config::agent_env_declares(&config.agent.env, env_ref) {
            config.agent.env.push(env_ref.clone());
            env_changed = true;
        }
    }
    let env_before_reconcile = config.agent.env.clone();
    reconcile_kimi_lane_env_declarations(&mut config.agent);
    env_changed = env_changed || config.agent.env != env_before_reconcile;
    collect_missing_provider_refs(
        prompts_enabled(args),
        secret_store,
        config,
        Some(&provider_id),
        &required_refs,
    )?;
    Ok(env_changed || api_key_ref_changed)
}

pub(crate) fn apply_provider_to_config(
    args: &InitArgs,
    registry: &RegistryCatalog,
    config: &mut Config,
    config_path: &Path,
    provider_id: String,
) -> Result<Vec<String>> {
    let entry = registry.lookup_required(&config.agent.id)?;
    if !entry.set_provider {
        return Err(StackError::AgentConfigProvision {
            path: config_path.to_path_buf(),
            reason: format!(
                "{} does not support provider configuration during init",
                config.agent.name
            ),
        });
    }
    // Drop any stray root-level `agent.model` left over from a prior
    // `acps agent set --model` for a model-only agent before we switch
    // to a provider-based flow. Runtime selection prefers
    // `agent.model` over `agent.provider.model` (supervisor.rs), so
    // leaving the old root value in place would silently override the
    // new `--model` chosen during this init run.
    config.agent.model = None;
    if args.custom_provider {
        require_custom_provider_support(entry, config, config_path)?;
        // Reject before the custom-provider prompts run, so a hosted driver is
        // not asked for four values that can never be written.
        reject_reserved_custom_provider_id(&provider_id)?;
        return apply_custom_provider_to_config(args, config, config_path, provider_id);
    }
    if !provider_id_is_known(&provider_id) {
        return Err(StackError::InvalidParam {
            field: "provider",
            reason: format!("provider `{provider_id}` is not listed in provider/env mapping"),
        });
    }
    if !provider_id_supports_agent(&provider_id, &config.agent.id) {
        return Err(StackError::InvalidParam {
            field: "provider",
            reason: format!(
                "provider `{provider_id}` is not supported for agent `{}`",
                config.agent.id
            ),
        });
    }
    let default_api_key_ref = env_var_for_agent_provider_id(&config.agent.id, &provider_id);
    let native_auth = provider_uses_agent_native_auth(&config.agent.id, &provider_id);
    if default_api_key_ref.is_none() && !native_auth {
        require_custom_provider_support(entry, config, config_path)?;
        // A registry id with no mapping for this harness cannot be steered into
        // custom setup: the id stays reserved, so the config it would write is
        // rejected by validation. Point at the distinct-id route instead.
        return Err(StackError::AgentConfigProvision {
            path: config_path.to_path_buf(),
            reason: format!(
                "provider `{provider_id}` has no API-key env mapping for agent `{}`; configure it with `--custom-provider` under a distinct id such as `{provider_id}-1`",
                config.agent.id
            ),
        });
    }
    if native_auth && args.api_key_ref.is_some() {
        return Err(StackError::AgentConfigProvision {
            path: config_path.to_path_buf(),
            reason: format!(
                "{} provider `{provider_id}` uses agent-native auth; do not pass --api-key-ref",
                config.agent.name
            ),
        });
    }
    let api_key_ref = args
        .api_key_ref
        .clone()
        .or_else(|| default_api_key_ref.map(str::to_owned));

    let required_refs = required_env_refs_for_agent_provider_id(
        &config.agent.id,
        &provider_id,
        api_key_ref.as_deref(),
    );
    for env_ref in &required_refs {
        if !crate::config::agent_env_declares(&config.agent.env, env_ref) {
            config.agent.env.push(env_ref.clone());
        }
    }
    // Preserve the existing provider model only when the operator is
    // re-confirming the SAME provider id (e.g. re-running `acps init
    // --provider X` to refresh secrets or run resume). Switching to a
    // different provider implies the old model probably belongs to a
    // different catalog, so clear it; the subsequent model lane in
    // configure_model_and_mode_for_init will either write a validated
    // new value or follow L87 print-and-skip semantics.
    let preserved_model = match config.agent.provider.as_ref() {
        Some(existing) if existing.id == provider_id => existing.model.clone(),
        _ => None,
    };
    config.agent.provider = Some(AgentProviderConfig {
        id: provider_id,
        model: preserved_model,
        api_key_ref,
        custom: None,
    });
    reconcile_kimi_lane_env_declarations(&mut config.agent);
    Ok(required_refs)
}
