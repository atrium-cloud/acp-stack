use std::path::Path;

use crate::config::{
    self, AgentCustomProviderConfig, AgentProviderConfig, Config, CustomProviderApi,
    DEFAULT_CUSTOM_MODEL_CONTEXT, DEFAULT_CUSTOM_MODEL_OUTPUT_MAX_TOKENS,
};
use crate::error::{Result, StackError};
use crate::runtime::agent::provider_keys::{
    AgentProviderSummary, env_var_for_agent_provider_id, provider_id_is_known,
    provider_id_supports_agent, provider_uses_agent_native_auth, providers_for_agent,
    required_env_refs_for_provider_id,
};
use crate::runtime::install::agent_registry::RegistryCatalog;
use crate::secrets::SecretStore;

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
        if !prompts_enabled(args) {
            required_init_custom_value(false, "provider-name", args.provider_name.clone())?;
            required_init_custom_value(false, "base-url", args.base_url.clone())?;
            required_init_custom_value(false, "api-key-ref", args.api_key_ref.clone())?;
            required_init_custom_value(false, "model", args.model.clone())?;
        }
        let api = parse_init_custom_provider_api(
            args.provider_api.as_deref(),
            default_init_custom_provider_api(&config.agent.id),
        )?;
        if config.agent.id == "codex" && api != CustomProviderApi::Responses {
            return Err(StackError::InvalidParam {
                field: "provider-api",
                reason: "Codex custom providers only support responses".to_owned(),
            });
        }
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
    collect_missing_provider_refs(prompts_enabled(args), secret_store, &required_refs)?;
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
    let Some(api_key_ref) = provider.api_key_ref.as_deref() else {
        return provider_uses_agent_native_auth(&config.agent.id, &provider.id);
    };
    let required_refs = required_env_refs_for_provider_id(&provider.id, api_key_ref);
    required_refs.iter().all(|env_ref| {
        config.agent.env.iter().any(|name| name == env_ref) && secret_store.contains(env_ref)
    })
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
    let Some(api_key_ref) = provider.api_key_ref.clone() else {
        return Ok(false);
    };
    let required_refs = required_env_refs_for_provider_id(&provider.id, &api_key_ref);
    let mut env_changed = false;
    for env_ref in &required_refs {
        if !config.agent.env.iter().any(|name| name == env_ref) {
            config.agent.env.push(env_ref.clone());
            env_changed = true;
        }
    }
    collect_missing_provider_refs(prompts_enabled(args), secret_store, &required_refs)?;
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
        if config.agent.id == "codex"
            && provider
                .custom
                .as_ref()
                .is_some_and(|custom| custom.api != CustomProviderApi::Responses)
        {
            return Err(StackError::InvalidParam {
                field: "agent.provider.custom.api",
                reason: "Codex custom providers only support responses".to_owned(),
            });
        }
        return Ok(());
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
    if provider.custom.is_some() {
        return entry.allow_custom_provider
            && entry.allow_custom_model
            && (agent_id != "codex"
                || provider
                    .custom
                    .as_ref()
                    .is_some_and(|custom| custom.api == CustomProviderApi::Responses));
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
        let provider_id = prompt::text(interactive, "provider id", true)?
            .map(|id| id.trim().to_owned())
            .ok_or_else(|| StackError::InvalidParam {
                field: "--provider",
                reason: format!("{} requires a provider id", entry.name),
            })?;
        return Ok(Some(provider_id));
    }
    let (available, needs_input): (Vec<_>, Vec<_>) = providers.iter().partition(|summary| {
        provider_has_available_secret_refs(&config.agent.id, summary, secret_store)
    });
    // Ready providers first, then ones needing secret/custom setup; the hint
    // column carries the readiness label so the grouping survives without
    // separate headers. A trailing item accepts a free-form id for a provider
    // the embedded mapping pre-dates.
    #[derive(Clone, PartialEq, Eq)]
    enum ProviderChoice {
        Id(String),
        Custom,
    }
    let mut items: Vec<(ProviderChoice, String, String)> = Vec::new();
    for summary in available.iter().chain(needs_input.iter()) {
        items.push((
            ProviderChoice::Id(summary.id.to_owned()),
            format!("{} ({})", summary.name, summary.id),
            provider_readiness_label(&config.agent.id, summary, secret_store),
        ));
    }
    items.push((
        ProviderChoice::Custom,
        "enter a provider id manually".to_owned(),
        String::new(),
    ));
    match prompt::searchable_select(
        interactive,
        &format!("provider for {}", config.agent.id),
        &items,
    )? {
        None => Ok(None),
        Some(ProviderChoice::Id(id)) => Ok(Some(id)),
        Some(ProviderChoice::Custom) => {
            Ok(prompt::text(interactive, "provider id", true)?.map(|id| id.trim().to_owned()))
        }
    }
}

fn provider_has_available_secret_refs(
    agent_id: &str,
    summary: &AgentProviderSummary,
    secret_store: &SecretStore,
) -> bool {
    let Some(api_key_ref) = summary.default_api_key_ref else {
        return provider_uses_agent_native_auth(agent_id, summary.id);
    };
    required_env_refs_for_provider_id(summary.id, api_key_ref)
        .iter()
        .all(|env_ref| secret_store.contains(env_ref))
}

fn provider_readiness_label(
    agent_id: &str,
    summary: &AgentProviderSummary,
    secret_store: &SecretStore,
) -> String {
    let Some(api_key_ref) = summary.default_api_key_ref else {
        return if provider_uses_agent_native_auth(agent_id, summary.id) {
            "agent-native auth".to_owned()
        } else {
            "custom provider setup required".to_owned()
        };
    };

    let required_refs = required_env_refs_for_provider_id(summary.id, api_key_ref);
    let missing_refs: Vec<_> = required_refs
        .iter()
        .filter(|env_ref| !secret_store.contains(env_ref))
        .map(String::as_str)
        .collect();
    if missing_refs.is_empty() {
        "ready".to_owned()
    } else {
        format!("missing {}", missing_refs.join(", "))
    }
}

fn apply_provider_to_config(
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
    if default_api_key_ref.is_none() {
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
        if !args.custom_provider
            && !confirm_custom_provider_setup(prompts_enabled(args), &provider_id)?
        {
            return Err(StackError::AgentConfigProvision {
                path: config_path.to_path_buf(),
                reason: format!(
                    "provider `{provider_id}` has no API-key env mapping for agent `{}`",
                    config.agent.id
                ),
            });
        }
        return apply_custom_provider_to_config(args, config, config_path, provider_id);
    }
    let api_key_ref = args
        .api_key_ref
        .clone()
        .or_else(|| default_api_key_ref.map(str::to_owned))
        .expect("default API-key ref checked");

    let required_refs = required_env_refs_for_provider_id(&provider_id, &api_key_ref);
    for env_ref in &required_refs {
        if !config.agent.env.iter().any(|name| name == env_ref) {
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
        api_key_ref: Some(api_key_ref),
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
    let provider_name =
        required_init_custom_value(interactive, "provider-name", args.provider_name.clone())?;
    let base_url = required_init_custom_value(interactive, "base-url", args.base_url.clone())?;
    let api_key_ref =
        required_init_custom_value(interactive, "api-key-ref", args.api_key_ref.clone())?;
    let model = required_init_custom_value(interactive, "model", args.model.clone())?;
    let model_name = args.model_name.clone().unwrap_or_else(|| model.clone());
    let api = parse_init_custom_provider_api(
        args.provider_api.as_deref(),
        default_init_custom_provider_api(&config.agent.id),
    )?;
    if config.agent.id == "codex" && api != CustomProviderApi::Responses {
        return Err(StackError::InvalidParam {
            field: "provider-api",
            reason: "Codex custom providers only support responses".to_owned(),
        });
    }
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
    if !config.agent.env.iter().any(|name| name == &api_key_ref) {
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

fn confirm_custom_provider_setup(interactive: bool, provider_id: &str) -> Result<bool> {
    prompt::confirm(
        interactive,
        &format!(
            "provider `{provider_id}` has no default API-key env mapping; configure it as a custom provider?"
        ),
        false,
    )
}

fn required_init_custom_value(
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
    match prompt::text(interactive, field, true)? {
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
        Some(_) => Err(StackError::InvalidParam {
            field: "provider-api",
            reason: "must be `chat-completions` or `responses`".to_owned(),
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

fn collect_missing_provider_refs(
    interactive: bool,
    secret_store: &mut SecretStore,
    required_refs: &[String],
) -> Result<()> {
    if interactive {
        let mut collected = Vec::new();
        for env_ref in required_refs {
            if secret_store.contains(env_ref) {
                continue;
            }
            // Masked entry via the wizard: a provider API key is a secret value;
            // echoing it to the terminal (and scrollback) would defeat the
            // encrypted store.
            let Some(value) = prompt::password(interactive, env_ref)? else {
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
        if !secret_store.contains(env_ref) {
            return Err(StackError::SecretNotFound {
                name: env_ref.clone(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary_for(agent_id: &str, provider_id: &str) -> AgentProviderSummary {
        providers_for_agent(agent_id)
            .into_iter()
            .find(|summary| summary.id == provider_id)
            .unwrap_or_else(|| panic!("{agent_id}/{provider_id} summary should exist"))
    }

    #[test]
    fn provider_readiness_reports_missing_default_secret_ref() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let secret_store = SecretStore::open_or_create(tempdir.path()).expect("secret store");
        let summary = summary_for("opencode", "openai");

        assert_eq!(
            provider_readiness_label("opencode", &summary, &secret_store),
            "missing OPENAI_API_KEY"
        );
    }

    #[test]
    fn provider_readiness_reports_present_default_secret_ref() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mut secret_store = SecretStore::open_or_create(tempdir.path()).expect("secret store");
        secret_store
            .set_many([("OPENAI_API_KEY", "test-openai-key")])
            .expect("secret should be stored");
        let summary = summary_for("opencode", "openai");

        assert_eq!(
            provider_readiness_label("opencode", &summary, &secret_store),
            "ready"
        );
        assert!(provider_has_available_secret_refs(
            "opencode",
            &summary,
            &secret_store
        ));
    }

    #[test]
    fn provider_readiness_reports_missing_companion_secret_refs() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mut secret_store = SecretStore::open_or_create(tempdir.path()).expect("secret store");
        secret_store
            .set_many([("CLOUDFLARE_API_TOKEN", "test-cloudflare-token")])
            .expect("secret should be stored");
        let summary = summary_for("opencode", "cloudflare-ai-gateway");

        assert_eq!(
            provider_readiness_label("opencode", &summary, &secret_store),
            "missing CLOUDFLARE_ACCOUNT_ID, CLOUDFLARE_GATEWAY_ID"
        );
        assert!(!provider_has_available_secret_refs(
            "opencode",
            &summary,
            &secret_store
        ));
    }

    #[test]
    fn provider_readiness_reports_custom_setup_for_provider_without_default_ref() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let secret_store = SecretStore::open_or_create(tempdir.path()).expect("secret store");
        let summary = summary_for("opencode", "helicone");

        assert_eq!(
            provider_readiness_label("opencode", &summary, &secret_store),
            "custom provider setup required"
        );
    }

    #[test]
    fn provider_readiness_reports_native_auth_only_for_known_native_auth_provider() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let secret_store = SecretStore::open_or_create(tempdir.path()).expect("secret store");
        let summary = summary_for("codex", "openai");

        assert_eq!(
            provider_readiness_label("codex", &summary, &secret_store),
            "agent-native auth"
        );
    }

    #[test]
    fn provider_readiness_label_reports_ready_with_secret() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mut secret_store = SecretStore::open_or_create(tempdir.path()).expect("secret store");
        secret_store
            .set_many([("OPENAI_API_KEY", "test-openai-key")])
            .expect("secret should be stored");
        let summary = summary_for("opencode", "openai");

        assert_eq!(
            provider_readiness_label("opencode", &summary, &secret_store),
            "ready"
        );
    }
}
