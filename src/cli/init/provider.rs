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

mod apply;
mod custom;
mod secret_refs;

pub(super) use self::apply::apply_provider_to_config;
use self::apply::ensure_configured_provider_refs_for_init;
use self::custom::{
    apply_custom_provider_to_config, default_init_custom_provider_api,
    parse_init_custom_provider_api, parse_init_custom_token_limit,
    reject_reserved_custom_provider_id, required_init_custom_value,
};
use self::secret_refs::agent_env_secret_refs_for_var;
pub(super) use self::secret_refs::{
    collect_declared_secret_refs_for_init, collect_mcp_secret_refs_for_init,
    collect_missing_provider_refs, collect_prepared_secret_refs_for_init,
    pending_custom_provider_credential, pending_provider_credential_reason,
    primary_provider_is_custom,
};

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
        require_custom_provider_support(entry, config, config_path)?;
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

/// The paired custom-provider / custom-model registry gates. Every lane that
/// can land in a custom-provider config runs both in this order, so the two
/// messages stay identical across preflight, apply, and validation.
fn require_custom_provider_support(
    entry: &crate::runtime::install::agent_registry::RegistryEntry,
    config: &Config,
    config_path: &Path,
) -> Result<()> {
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
        require_custom_provider_support(entry, config, config_path)?;
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

/// How a provider would fare if the operator picked it right now. The picker
/// needs both a grouping decision and a hint string; deriving them from one
/// classification keeps the two from disagreeing.
enum ProviderReadiness {
    /// Every required ref resolves and the mapping names an API-key ref.
    Ready,
    /// No API-key ref is needed because the harness authenticates natively.
    AgentNativeAuth,
    /// The harness lists the provider but the registry maps no API-key env var
    /// for it, and the id itself stays reserved, so the only route is a custom
    /// provider under a different id. Selecting the entry can now only produce
    /// that error.
    NeedsDistinctCustomId,
    /// Required refs that resolve from neither the store nor the catalog.
    MissingRefs(Vec<String>),
}

impl ProviderReadiness {
    fn is_available(&self) -> bool {
        matches!(self, Self::Ready | Self::AgentNativeAuth)
    }

    fn label(&self) -> String {
        match self {
            Self::Ready => "ready".to_owned(),
            Self::AgentNativeAuth => "agent-native auth".to_owned(),
            Self::NeedsDistinctCustomId => "needs a distinct custom id".to_owned(),
            Self::MissingRefs(refs) => format!("missing {}", refs.join(", ")),
        }
    }
}

fn provider_readiness(
    config: &Config,
    summary: &AgentProviderSummary,
    secret_store: &SecretStore,
) -> ProviderReadiness {
    let agent_id = config.agent.id.as_str();
    let required_refs =
        required_env_refs_for_agent_provider_id(agent_id, summary.id, summary.default_api_key_ref);
    let missing_refs: Vec<String> = required_refs
        .into_iter()
        .filter(|env_ref| {
            !env_ref_is_satisfiable_for_config(config, secret_store, summary.id, env_ref)
        })
        .collect();
    if !missing_refs.is_empty() {
        return ProviderReadiness::MissingRefs(missing_refs);
    }
    if summary.default_api_key_ref.is_some() {
        ProviderReadiness::Ready
    } else if provider_uses_agent_native_auth(agent_id, summary.id) {
        ProviderReadiness::AgentNativeAuth
    } else {
        ProviderReadiness::NeedsDistinctCustomId
    }
}

fn provider_has_available_secret_refs(
    config: &Config,
    summary: &AgentProviderSummary,
    secret_store: &SecretStore,
) -> bool {
    provider_readiness(config, summary, secret_store).is_available()
}

fn provider_readiness_label(
    config: &Config,
    summary: &AgentProviderSummary,
    secret_store: &SecretStore,
) -> String {
    provider_readiness(config, summary, secret_store).label()
}

#[cfg(test)]
mod tests;
