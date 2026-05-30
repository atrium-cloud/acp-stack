use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use crate::config::{
    self, AgentCustomProviderConfig, AgentProviderConfig, Config, CustomProviderApi,
    DEFAULT_CUSTOM_MODEL_CONTEXT, DEFAULT_CUSTOM_MODEL_OUTPUT_MAX_TOKENS,
};
use crate::error::{Result, StackError};
use crate::runtime::agent::provider_keys::{
    env_var_for_agent_provider_id, provider_id_is_known, provider_id_supports_agent,
    providers_for_agent, required_env_refs_for_provider_id,
};
use crate::runtime::install::agent_registry::RegistryCatalog;
use crate::secrets::SecretStore;

use super::InitArgs;

pub(super) fn configure_provider_for_init(
    args: &InitArgs,
    registry: &RegistryCatalog,
    config: &mut Config,
    config_path: &Path,
    secret_store: &mut SecretStore,
) -> Result<bool> {
    let Some(provider_id) = select_provider_for_init(args, registry, config)? else {
        return Ok(false);
    };
    let required_refs = apply_provider_to_config(args, registry, config, config_path, provider_id)?;
    collect_missing_provider_refs(secret_store, &required_refs)?;
    Ok(true)
}

fn select_provider_for_init(
    args: &InitArgs,
    registry: &RegistryCatalog,
    config: &Config,
) -> Result<Option<String>> {
    if let Some(provider_id) = &args.provider {
        return Ok(Some(provider_id.clone()));
    }
    if !io::stdin().is_terminal() {
        return Ok(None);
    }
    let Some(entry) = registry.lookup(&config.agent.id) else {
        return Ok(None);
    };
    if !entry.set_provider {
        return Ok(None);
    }

    // Offline-curated picker. The compatibility list is the same source
    // `GET /v1/providers` uses, so the operator sees exactly the
    // providers that any other surface (CLI/API/UI) would offer for the
    // selected agent. Free-form id entry is still accepted at the
    // prompt so an operator can target a provider the embedded mapping
    // pre-dates without round-tripping through `acps agent set`.
    let providers = providers_for_agent(&config.agent.id);
    if providers.is_empty() {
        println!(
            "no providers in data/providers.toml advertise compatibility with agent `{}`; \
             pass --provider <id> to skip the picker",
            config.agent.id
        );
        return Ok(None);
    }
    println!("providers for {}:", config.agent.id);
    for (index, summary) in providers.iter().enumerate() {
        println!("  {}. {} ({})", index + 1, summary.name, summary.id);
    }
    print!("select provider [number or id, blank to skip]: ");
    io::stdout()
        .flush()
        .map_err(|source| StackError::ConfigWrite {
            path: PathBuf::from("stdout"),
            source,
        })?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(|source| StackError::ConfigRead {
            path: PathBuf::from("stdin"),
            source,
        })?;
    let answer = answer.trim();
    if answer.is_empty() {
        return Ok(None);
    }
    if let Ok(index) = answer.parse::<usize>() {
        // 1-indexed picker; explicitly reject `0` so `saturating_sub`
        // can't silently fold it to the first entry.
        if index == 0 {
            return Err(StackError::InvalidParam {
                field: "provider",
                reason: format!(
                    "provider selection `{answer}` is out of range (expected 1..={})",
                    providers.len()
                ),
            });
        }
        let Some(summary) = providers.get(index - 1) else {
            return Err(StackError::InvalidParam {
                field: "provider",
                reason: format!(
                    "provider selection `{answer}` is out of range (expected 1..={})",
                    providers.len()
                ),
            });
        };
        return Ok(Some(summary.id.to_owned()));
    }
    Ok(Some(answer.to_owned()))
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
        if !args.custom_provider && !confirm_custom_provider_setup(&provider_id)? {
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
    let provider_name = required_init_custom_value("provider-name", args.provider_name.clone())?;
    let base_url = required_init_custom_value("base-url", args.base_url.clone())?;
    let api_key_ref = required_init_custom_value("api-key-ref", args.api_key_ref.clone())?;
    let model = required_init_custom_value("model", args.model.clone())?;
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

fn confirm_custom_provider_setup(provider_id: &str) -> Result<bool> {
    if !io::stdin().is_terminal() {
        return Ok(false);
    }
    print!(
        "provider `{provider_id}` has no default API-key env mapping; configure it as a custom provider? [y/N]: "
    );
    io::stdout()
        .flush()
        .map_err(|source| StackError::ConfigWrite {
            path: PathBuf::from("stdout"),
            source,
        })?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(|source| StackError::ConfigRead {
            path: PathBuf::from("stdin"),
            source,
        })?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "YES" | "Yes"))
}

fn required_init_custom_value(field: &'static str, value: Option<String>) -> Result<String> {
    if let Some(value) = value
        && !value.trim().is_empty()
        && value.trim().len() == value.len()
    {
        return Ok(value);
    }
    if !io::stdin().is_terminal() {
        return Err(StackError::InvalidParam {
            field,
            reason: format!("--{field} is required for custom provider init"),
        });
    }
    print!("{field}: ");
    io::stdout()
        .flush()
        .map_err(|source| StackError::ConfigWrite {
            path: PathBuf::from("stdout"),
            source,
        })?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(|source| StackError::ConfigRead {
            path: PathBuf::from("stdin"),
            source,
        })?;
    let answer = answer.trim().to_owned();
    if answer.is_empty() {
        return Err(StackError::InvalidParam {
            field,
            reason: format!("--{field} is required for custom provider init"),
        });
    }
    Ok(answer)
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
    secret_store: &mut SecretStore,
    required_refs: &[String],
) -> Result<()> {
    if io::stdin().is_terminal() {
        let mut collected = Vec::new();
        for env_ref in required_refs {
            if secret_store.contains(env_ref) {
                continue;
            }
            print!("{env_ref}: ");
            io::stdout()
                .flush()
                .map_err(|source| StackError::ConfigWrite {
                    path: PathBuf::from("stdout"),
                    source,
                })?;
            let mut value = String::new();
            io::stdin()
                .read_line(&mut value)
                .map_err(|source| StackError::StdinRead { source })?;
            let value = value.trim_end_matches(['\n', '\r']).to_owned();
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
