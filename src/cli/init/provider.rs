use std::collections::BTreeSet;
use std::path::Path;

use crate::config::{
    self, AgentCustomProviderConfig, AgentProviderConfig, Config, CustomProviderApi,
    DEFAULT_CUSTOM_MODEL_CONTEXT, DEFAULT_CUSTOM_MODEL_OUTPUT_MAX_TOKENS,
};
use crate::error::{Result, StackError};
use crate::runtime::agent::provider_keys::{
    AgentProviderSummary, CLAUDE_CODE_AGENT_ID, env_ref_is_satisfiable_for_config,
    env_var_for_agent_provider_id, provider_id_is_known, provider_id_supports_agent,
    provider_uses_agent_native_auth, providers_for_agent, required_env_refs_for_agent_provider_id,
};
use crate::runtime::install::agent_registry::RegistryCatalog;
use crate::secrets::SecretStore;

use super::super::agent::validate_custom_provider_api_for_agent;
use super::{InitArgs, prompt, prompts_enabled};

pub(super) fn preflight_provider_for_init(
    args: &InitArgs,
    registry: &RegistryCatalog,
    config: &Config,
    config_path: &Path,
) -> Result<()> {
    let Some(provider_id) = args.provider.as_deref() else {
        return Ok(());
    };
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
    if args.custom_provider {
        if !entry.allow_custom_provider {
            return Err(StackError::AgentConfigProvision {
                path: config_path.to_path_buf(),
                reason: format!(
                    "{} does not support custom provider setup",
                    config.agent.name
                ),
            });
        }
        if !entry.allow_custom_model {
            return Err(StackError::AgentConfigProvision {
                path: config_path.to_path_buf(),
                reason: format!("{} does not support custom model setup", config.agent.name),
            });
        }
        reject_reserved_custom_provider_id(provider_id)?;
        if !prompts_enabled(args) {
            required_init_custom_value(
                prompt::HostedPromptKind::ProviderName,
                false,
                "provider-name",
                args.provider_name.clone(),
            )?;
            required_init_custom_value(
                prompt::HostedPromptKind::BaseUrl,
                false,
                "base-url",
                args.base_url.clone(),
            )?;
            required_init_custom_value(
                prompt::HostedPromptKind::ApiKeyRef,
                false,
                "api-key-ref",
                args.api_key_ref.clone(),
            )?;
            required_init_custom_value(
                prompt::HostedPromptKind::Model,
                false,
                "model",
                args.model.clone(),
            )?;
        }
        let api = parse_init_custom_provider_api(
            args.provider_api.as_deref(),
            default_init_custom_provider_api(&config.agent.id),
        )?;
        validate_custom_provider_api_for_agent(&config.agent.id, api, "provider-api")?;
        parse_init_custom_token_limit(
            "context",
            args.context.as_deref(),
            DEFAULT_CUSTOM_MODEL_CONTEXT,
        )?;
        parse_init_custom_token_limit(
            "output-max-tokens",
            args.output_max_tokens.as_deref(),
            DEFAULT_CUSTOM_MODEL_OUTPUT_MAX_TOKENS,
        )?;
        return Ok(());
    }
    if config.agent.id == "codex" && provider_id == "openai" && args.api_key_ref.is_some() {
        return Err(StackError::AgentConfigProvision {
            path: config_path.to_path_buf(),
            reason: "Codex OpenAI uses Codex-native auth; do not pass --api-key-ref".to_owned(),
        });
    }
    if provider_id_is_known(provider_id)
        && !provider_id_supports_agent(provider_id, &config.agent.id)
    {
        return Err(StackError::InvalidParam {
            field: "provider",
            reason: format!(
                "provider `{provider_id}` is not supported for agent `{}`",
                config.agent.id
            ),
        });
    }
    Ok(())
}

pub(super) fn configure_provider_for_init(
    args: &InitArgs,
    registry: &RegistryCatalog,
    config: &mut Config,
    config_path: &Path,
    secret_store: &mut SecretStore,
) -> Result<bool> {
    let Some(entry) = registry.lookup(&config.agent.id) else {
        return Ok(false);
    };
    if !entry.set_provider {
        if args.provider.is_some() {
            return Err(StackError::AgentConfigProvision {
                path: config_path.to_path_buf(),
                reason: format!(
                    "{} does not support provider configuration during init",
                    config.agent.name
                ),
            });
        }
        return Ok(config.agent.provider.take().is_some());
    }

    let Some(provider_id) = select_provider_for_init(args, registry, config, secret_store)? else {
        validate_configured_provider_for_init(registry, config, config_path)?;
        return ensure_configured_provider_refs_for_init(
            args,
            registry,
            config,
            config_path,
            secret_store,
        );
    };
    let required_refs = apply_provider_to_config(args, registry, config, config_path, provider_id)?;
    let configured_provider_id = config
        .agent
        .provider
        .as_ref()
        .map(|provider| provider.id.clone());
    collect_missing_provider_refs(
        prompts_enabled(args),
        secret_store,
        config,
        configured_provider_id.as_deref(),
        &required_refs,
    )?;
    Ok(true)
}

pub(super) fn configured_provider_refs_satisfied(
    registry: &RegistryCatalog,
    config: &Config,
    secret_store: &SecretStore,
) -> bool {
    let Some(entry) = registry.lookup(&config.agent.id) else {
        return true;
    };
    if !entry.set_provider {
        return config.agent.provider.is_none();
    }
    let Some(provider) = config.agent.provider.as_ref() else {
        return false;
    };
    if !configured_provider_shape_is_supported(&config.agent.id, entry, provider) {
        return false;
    };
    let required_refs = required_env_refs_for_agent_provider_id(
        &config.agent.id,
        &provider.id,
        provider.api_key_ref.as_deref(),
    );
    if provider.api_key_ref.is_none()
        && !provider_uses_agent_native_auth(&config.agent.id, &provider.id)
    {
        return false;
    }
    required_refs.iter().all(|env_ref| {
        crate::config::agent_env_declares(&config.agent.env, env_ref)
            && agent_env_secret_refs_for_var(&config.agent.env, env_ref)
                .iter()
                .all(|name| {
                    env_ref_is_satisfiable_for_config(config, secret_store, &provider.id, name)
                })
    })
}

/// The secret refs that must exist in the store for env var `var_name` to
/// resolve: the entry itself when declared bare, the template's inner refs
/// when declared as `VAR=template`, and the var name as a plain ref when the
/// env list does not declare it at all (the pre-template behavior).
fn agent_env_secret_refs_for_var(env: &[String], var_name: &str) -> Vec<String> {
    env.iter()
        .find(|entry| crate::config::env_entry_var_name(entry) == var_name)
        .map(|entry| crate::config::env_entry_ref_names_lossy(entry))
        .unwrap_or_else(|| vec![var_name.to_owned()])
}

pub(super) fn collect_prepared_secret_refs_for_init(
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
pub(super) fn collect_declared_secret_refs_for_init(
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
pub(super) fn collect_mcp_secret_refs_for_init(
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

fn ensure_configured_provider_refs_for_init(
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
    collect_missing_provider_refs(
        prompts_enabled(args),
        secret_store,
        config,
        Some(&provider_id),
        &required_refs,
    )?;
    Ok(env_changed || api_key_ref_changed)
}

fn validate_configured_provider_for_init(
    registry: &RegistryCatalog,
    config: &Config,
    config_path: &Path,
) -> Result<()> {
    let Some(provider) = config.agent.provider.as_ref() else {
        return Ok(());
    };
    let entry = registry.lookup_required(&config.agent.id)?;
    if provider.custom.is_some() {
        if !entry.allow_custom_provider {
            return Err(StackError::AgentConfigProvision {
                path: config_path.to_path_buf(),
                reason: format!(
                    "{} does not support custom provider setup",
                    config.agent.name
                ),
            });
        }
        if !entry.allow_custom_model {
            return Err(StackError::AgentConfigProvision {
                path: config_path.to_path_buf(),
                reason: format!("{} does not support custom model setup", config.agent.name),
            });
        }
        if let Some(custom) = provider.custom.as_ref() {
            validate_custom_provider_api_for_agent(
                &config.agent.id,
                custom.api,
                "agent.provider.custom.api",
            )?;
        }
        return Ok(());
    }
    if provider_uses_agent_native_auth(&config.agent.id, &provider.id)
        && provider.api_key_ref.is_some()
    {
        return Err(StackError::InvalidParam {
            field: "agent.provider.api_key_ref",
            reason: format!(
                "{} provider `{}` uses agent-native auth",
                config.agent.name, provider.id
            ),
        });
    }
    if !provider_id_is_known(&provider.id) {
        return Err(StackError::InvalidParam {
            field: "agent.provider.id",
            reason: format!(
                "provider `{}` is not listed in provider/env mapping and has no [agent.provider.custom] block",
                provider.id
            ),
        });
    }
    if !provider_id_supports_agent(&provider.id, &config.agent.id) {
        return Err(StackError::InvalidParam {
            field: "agent.provider.id",
            reason: format!(
                "provider `{}` is not supported for agent `{}`",
                provider.id, config.agent.id
            ),
        });
    }
    if config.agent.id == "codex" && provider.id == "openai" && provider.api_key_ref.is_some() {
        return Err(StackError::AgentConfigProvision {
            path: config_path.to_path_buf(),
            reason: "Codex OpenAI uses Codex-native auth; remove agent.provider.api_key_ref"
                .to_owned(),
        });
    }
    Ok(())
}

fn configured_provider_shape_is_supported(
    agent_id: &str,
    entry: &crate::runtime::install::agent_registry::RegistryEntry,
    provider: &AgentProviderConfig,
) -> bool {
    if let Some(custom) = provider.custom.as_ref() {
        return entry.allow_custom_provider
            && entry.allow_custom_model
            && (agent_id != "codex" || custom.api == CustomProviderApi::Responses)
            && (agent_id != CLAUDE_CODE_AGENT_ID
                || custom.api == CustomProviderApi::AnthropicMessages)
            && (agent_id == CLAUDE_CODE_AGENT_ID
                || custom.api != CustomProviderApi::AnthropicMessages);
    }
    provider_id_is_known(&provider.id)
        && provider_id_supports_agent(&provider.id, agent_id)
        && !(agent_id == "codex" && provider.id == "openai" && provider.api_key_ref.is_some())
}

fn select_provider_for_init(
    args: &InitArgs,
    registry: &RegistryCatalog,
    config: &Config,
    secret_store: &SecretStore,
) -> Result<Option<String>> {
    if let Some(provider_id) = &args.provider {
        return Ok(Some(provider_id.clone()));
    }
    let Some(entry) = registry.lookup(&config.agent.id) else {
        return Ok(None);
    };
    if !entry.set_provider {
        return Ok(None);
    }
    if config.agent.provider.is_some() {
        return Ok(None);
    }
    let interactive = prompts_enabled(args);
    if !interactive {
        return Err(StackError::InvalidParam {
            field: "--provider",
            reason: format!(
                "{} supports provider configuration; pass --provider <id> or import config with [agent.provider]",
                entry.name,
            ),
        });
    }

    // Offline-curated picker. The compatibility list is the same source
    // `GET /v1/providers` uses, so the operator sees exactly the
    // providers that any other surface (CLI/API/UI) would offer for the
    // selected agent. Free-form id entry is still accepted at the
    // prompt so an operator can target a provider the embedded mapping
    // pre-dates without round-tripping through `acps agent set`.
    let providers = providers_for_agent(&config.agent.id);
    if providers.is_empty() {
        let provider_id = prompt::text(
            prompt::HostedPromptKind::ProviderId,
            interactive,
            "provider id",
            true,
        )?
        .map(|id| id.trim().to_owned())
        .ok_or_else(|| StackError::InvalidParam {
            field: "--provider",
            reason: format!("{} requires a provider id", entry.name),
        })?;
        return Ok(Some(provider_id));
    }
    let (available, needs_input): (Vec<_>, Vec<_>) = providers
        .iter()
        .partition(|summary| provider_has_available_secret_refs(config, summary, secret_store));
    // Ready providers first, then ones needing secret/custom setup; the hint
    // column carries the readiness label so the grouping survives without
    // separate headers. A trailing item accepts a free-form id for a provider
    // the embedded mapping pre-dates.
    #[derive(Clone, PartialEq, Eq)]
    enum ProviderChoice {
        Id(String),
        Custom,
    }
    let mut items: Vec<prompt::PromptItem<ProviderChoice>> = Vec::new();
    for summary in available.iter().chain(needs_input.iter()) {
        items.push(prompt::item(
            ProviderChoice::Id(summary.id.to_owned()),
            summary.id,
            format!("{} ({})", summary.name, summary.id),
            provider_readiness_label(config, summary, secret_store),
        ));
    }
    items.push(prompt::item(
        ProviderChoice::Custom,
        "__custom",
        "enter a provider id manually",
        "",
    ));
    match prompt::searchable_select(
        prompt::HostedPromptKind::ProviderId,
        interactive,
        &format!("provider for {}", config.agent.id),
        &items,
    )? {
        None => Ok(None),
        Some(ProviderChoice::Id(id)) => Ok(Some(id)),
        Some(ProviderChoice::Custom) => Ok(prompt::text(
            prompt::HostedPromptKind::ProviderId,
            interactive,
            "provider id",
            true,
        )?
        .map(|id| id.trim().to_owned())),
    }
}

fn provider_has_available_secret_refs(
    config: &Config,
    summary: &AgentProviderSummary,
    secret_store: &SecretStore,
) -> bool {
    let agent_id = config.agent.id.as_str();
    let required_refs =
        required_env_refs_for_agent_provider_id(agent_id, summary.id, summary.default_api_key_ref);
    if summary.default_api_key_ref.is_none()
        && !provider_uses_agent_native_auth(agent_id, summary.id)
    {
        return false;
    }
    required_refs
        .iter()
        .all(|env_ref| env_ref_is_satisfiable_for_config(config, secret_store, summary.id, env_ref))
}

fn provider_readiness_label(
    config: &Config,
    summary: &AgentProviderSummary,
    secret_store: &SecretStore,
) -> String {
    let agent_id = config.agent.id.as_str();
    let required_refs =
        required_env_refs_for_agent_provider_id(agent_id, summary.id, summary.default_api_key_ref);
    let missing_refs: Vec<_> = required_refs
        .iter()
        .filter(|env_ref| {
            !env_ref_is_satisfiable_for_config(config, secret_store, summary.id, env_ref)
        })
        .map(String::as_str)
        .collect();
    if missing_refs.is_empty() {
        if summary.default_api_key_ref.is_none()
            && provider_uses_agent_native_auth(agent_id, summary.id)
        {
            "agent-native auth".to_owned()
        } else if summary.default_api_key_ref.is_none() {
            // The harness lists the provider but the registry maps no API-key
            // env var for it, and the id itself stays reserved, so the only
            // route is a custom provider under a different id. Say so here:
            // selecting the entry can now only produce that error.
            "needs a distinct custom id".to_owned()
        } else {
            "ready".to_owned()
        }
    } else {
        format!("missing {}", missing_refs.join(", "))
    }
}

pub(super) fn apply_provider_to_config(
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
        if !entry.allow_custom_provider {
            return Err(StackError::AgentConfigProvision {
                path: config_path.to_path_buf(),
                reason: format!(
                    "{} does not support custom provider setup",
                    config.agent.name
                ),
            });
        }
        if !entry.allow_custom_model {
            return Err(StackError::AgentConfigProvision {
                path: config_path.to_path_buf(),
                reason: format!("{} does not support custom model setup", config.agent.name),
            });
        }
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
    if provider_id_is_known(&provider_id)
        && !provider_id_supports_agent(&provider_id, &config.agent.id)
    {
        return Err(StackError::InvalidParam {
            field: "provider",
            reason: format!(
                "provider `{provider_id}` is not supported for agent `{}`",
                config.agent.id
            ),
        });
    }
    if config.agent.id == "codex" && provider_id == "openai" {
        if args.api_key_ref.is_some() {
            return Err(StackError::AgentConfigProvision {
                path: config_path.to_path_buf(),
                reason: "Codex OpenAI uses Codex-native auth; do not pass --api-key-ref".to_owned(),
            });
        }
        // Mirror the preserve-on-same-provider semantics from the
        // generic branch — re-confirming codex+openai must not silently
        // drop a previously pinned model just because --model was
        // omitted on this rerun.
        let preserved_model = match config.agent.provider.as_ref() {
            Some(existing) if existing.id == provider_id => existing.model.clone(),
            _ => None,
        };
        config.agent.provider = Some(AgentProviderConfig {
            id: provider_id,
            model: preserved_model,
            api_key_ref: None,
            custom: None,
        });
        return Ok(Vec::new());
    }
    let default_api_key_ref = env_var_for_agent_provider_id(&config.agent.id, &provider_id);
    let native_auth = provider_uses_agent_native_auth(&config.agent.id, &provider_id);
    if default_api_key_ref.is_none() && !native_auth {
        if !entry.allow_custom_provider {
            return Err(StackError::AgentConfigProvision {
                path: config_path.to_path_buf(),
                reason: format!(
                    "{} does not support custom provider setup",
                    config.agent.name
                ),
            });
        }
        if !entry.allow_custom_model {
            return Err(StackError::AgentConfigProvision {
                path: config_path.to_path_buf(),
                reason: format!("{} does not support custom model setup", config.agent.name),
            });
        }
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
    Ok(required_refs)
}

fn apply_custom_provider_to_config(
    args: &InitArgs,
    config: &mut Config,
    config_path: &Path,
    provider_id: String,
) -> Result<Vec<String>> {
    let interactive = prompts_enabled(args);
    let provider_name = required_init_custom_value(
        prompt::HostedPromptKind::ProviderName,
        interactive,
        "provider-name",
        args.provider_name.clone(),
    )?;
    let base_url = required_init_custom_value(
        prompt::HostedPromptKind::BaseUrl,
        interactive,
        "base-url",
        args.base_url.clone(),
    )?;
    let api_key_ref = required_init_custom_value(
        prompt::HostedPromptKind::ApiKeyRef,
        interactive,
        "api-key-ref",
        args.api_key_ref.clone(),
    )?;
    let model = required_init_custom_value(
        prompt::HostedPromptKind::Model,
        interactive,
        "model",
        args.model.clone(),
    )?;
    let model_name = args.model_name.clone().unwrap_or_else(|| model.clone());
    let api = parse_init_custom_provider_api(
        args.provider_api.as_deref(),
        default_init_custom_provider_api(&config.agent.id),
    )?;
    validate_custom_provider_api_for_agent(&config.agent.id, api, "provider-api")?;
    let context = parse_init_custom_token_limit(
        "context",
        args.context.as_deref(),
        DEFAULT_CUSTOM_MODEL_CONTEXT,
    )?;
    let output_max_tokens = parse_init_custom_token_limit(
        "output-max-tokens",
        args.output_max_tokens.as_deref(),
        DEFAULT_CUSTOM_MODEL_OUTPUT_MAX_TOKENS,
    )?;
    if !crate::config::agent_env_declares(&config.agent.env, &api_key_ref) {
        config.agent.env.push(api_key_ref.clone());
    }
    config.agent.provider = Some(AgentProviderConfig {
        id: provider_id,
        model: Some(model),
        api_key_ref: Some(api_key_ref.clone()),
        custom: Some(AgentCustomProviderConfig {
            name: provider_name,
            base_url,
            api,
            model_name: Some(model_name),
            context,
            output_max_tokens,
        }),
    });
    let canonical = config.to_canonical_toml()?;
    let validated = config::load_config_from_str(&canonical)?;
    *config = validated;
    if config
        .agent
        .provider
        .as_ref()
        .and_then(|provider| provider.custom.as_ref())
        .is_none()
    {
        return Err(StackError::AgentConfigProvision {
            path: config_path.to_path_buf(),
            reason: "custom provider config was not retained".to_owned(),
        });
    }
    Ok(vec![api_key_ref])
}

fn reject_reserved_custom_provider_id(provider_id: &str) -> Result<()> {
    if provider_id_is_known(provider_id) {
        return Err(StackError::InvalidParam {
            field: "provider",
            reason: format!(
                "`{provider_id}` is reserved by the mapped-provider registry; choose a distinct custom id such as `{provider_id}-1`"
            ),
        });
    }
    Ok(())
}

fn required_init_custom_value(
    kind: prompt::HostedPromptKind,
    interactive: bool,
    field: &'static str,
    value: Option<String>,
) -> Result<String> {
    if let Some(value) = value
        && !value.trim().is_empty()
        && value.trim().len() == value.len()
    {
        return Ok(value);
    }
    let missing = || StackError::InvalidParam {
        field,
        reason: format!("--{field} is required for custom provider init"),
    };
    match prompt::text(kind, interactive, field, true)? {
        Some(answer) => {
            let answer = answer.trim().to_owned();
            if answer.is_empty() {
                Err(missing())
            } else {
                Ok(answer)
            }
        }
        None => Err(missing()),
    }
}

fn default_init_custom_provider_api(agent_id: &str) -> CustomProviderApi {
    if agent_id == "codex" {
        CustomProviderApi::Responses
    } else if agent_id == CLAUDE_CODE_AGENT_ID {
        CustomProviderApi::AnthropicMessages
    } else {
        CustomProviderApi::ChatCompletions
    }
}

fn parse_init_custom_provider_api(
    value: Option<&str>,
    default: CustomProviderApi,
) -> Result<CustomProviderApi> {
    match value {
        None => Ok(default),
        Some("chat-completions") => Ok(CustomProviderApi::ChatCompletions),
        Some("responses") => Ok(CustomProviderApi::Responses),
        Some("anthropic-messages") => Ok(CustomProviderApi::AnthropicMessages),
        Some(_) => Err(StackError::InvalidParam {
            field: "provider-api",
            reason: "must be `chat-completions`, `responses`, or `anthropic-messages`".to_owned(),
        }),
    }
}

fn parse_init_custom_token_limit(
    field: &'static str,
    value: Option<&str>,
    default: u64,
) -> Result<u64> {
    let Some(value) = value else {
        return Ok(default);
    };
    if value.contains(',') {
        return Err(StackError::InvalidParam {
            field,
            reason: "must be a plain integer without commas".to_owned(),
        });
    }
    let parsed = value.parse::<u64>().map_err(|_| StackError::InvalidParam {
        field,
        reason: "must be a positive integer".to_owned(),
    })?;
    if parsed == 0 {
        return Err(StackError::InvalidParam {
            field,
            reason: "must be greater than 0".to_owned(),
        });
    }
    Ok(parsed)
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
pub(super) fn primary_provider_is_custom(config: &Config) -> bool {
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
pub(super) fn pending_custom_provider_credential(
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
pub(super) fn pending_provider_credential_reason(provider_id: &str, api_key_ref: &str) -> String {
    format!(
        "provider `{provider_id}` has no credential yet: `{api_key_ref}` is not in the secret store and no managed credential has been applied; push one through the managed-state extension or run `acps secrets set {api_key_ref}`"
    )
}

pub(super) fn collect_missing_provider_refs(
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

#[cfg(test)]
mod tests;
