use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use crate::config::{
    self, AgentCustomProviderConfig, AgentProviderConfig, Config, CustomProviderApi,
    DEFAULT_CUSTOM_MODEL_CONTEXT, DEFAULT_CUSTOM_MODEL_OUTPUT_MAX_TOKENS,
};
use crate::error::{Result, StackError};
use crate::fs_util::{atomic_write_owner_only, home_dir};
use crate::runtime::agent::acp_bridge::{
    AgentSessionConfigCategory, session_config_id_for_value, session_config_values,
    session_model_selection_for_value, session_model_values,
};
use crate::runtime::agent::agent_headless_config::provision_agent_headless_config;
use crate::runtime::agent::claude_code_provider_profiles::{
    CLAUDE_CODE_AGENT_ID, is_claude_code_profiled_provider, profile_for_provider_id,
};
use crate::runtime::agent::model_discovery::{
    fetch_session_config, resolve_advertised_model_value,
};
use crate::runtime::agent::provider_keys::{
    agent_provider_id_for_provider_id, env_refs_for_agent_id, env_var_for_agent_provider_id,
    optional_env_refs_for_agent_provider_id, provider_id_is_known, provider_id_supports_agent,
    provider_uses_agent_native_auth, required_env_refs_for_agent_provider_id,
};
use crate::runtime::install::agent_registry::{RegistryCatalog, RegistryEntry};

use super::AgentSetArgs;
use super::install::operator_registry_override;

pub(super) fn run_agent_set(args: AgentSetArgs) -> Result<()> {
    let home = home_dir()?;
    let config_path = config::default_config_path()?;
    let mut config = Config::load_from_path(&config_path)?;
    let registry = RegistryCatalog::load_with_override(&operator_registry_override(&home))?;
    let entry = registry.lookup_required(&config.agent.id)?;
    if let Some(mode) = args.mode.clone() {
        return run_agent_mode_set(config, config_path, &home, args, entry, mode);
    }
    let Some(provider_id) = args.provider.clone() else {
        return run_agent_model_set(config, config_path, &home, args, entry);
    };
    if args.custom_provider {
        return run_agent_custom_provider_set(config, config_path, &home, args, entry, provider_id);
    }
    reject_custom_provider_args(&args)?;
    if !entry.set_provider {
        return Err(StackError::InvalidParam {
            field: "provider",
            reason: format!(
                "{} does not support provider configuration through `acps agent set`",
                entry.name
            ),
        });
    }
    if args.model.is_some() && !entry.set_model {
        return Err(StackError::AgentConfigProvision {
            path: config_path,
            reason: format!(
                "{} does not support model configuration through `acps agent set`",
                entry.name
            ),
        });
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
                "provider `{}` is not supported for agent `{}`",
                provider_id, config.agent.id
            ),
        });
    }
    if config.agent.id == "codex" && provider_id == "openai" {
        return run_codex_openai_set(&home, config, config_path, args, provider_id);
    }

    let default_api_key_ref =
        default_api_key_ref_for_agent_provider(&config.agent.id, &provider_id);
    let native_auth = provider_uses_agent_native_auth(&config.agent.id, &provider_id);
    if default_api_key_ref.is_none() && !native_auth {
        return Err(StackError::AgentConfigProvision {
            path: config_path,
            reason: format!(
                "provider `{}` has no API-key env mapping for agent `{}`",
                provider_id, config.agent.id
            ),
        });
    }
    if native_auth && args.api_key_ref.is_some() {
        return Err(StackError::AgentConfigProvision {
            path: config_path,
            reason: format!(
                "{} provider `{provider_id}` uses agent-native auth; do not pass --api-key-ref",
                entry.name
            ),
        });
    }

    let api_key_ref = args.api_key_ref.or(default_api_key_ref);
    if api_key_ref.is_none() && !native_auth {
        return Err(StackError::AgentConfigProvision {
            path: config_path.clone(),
            reason: format!(
                "provider `{provider_id}` has no default API-key env var; pass --api-key-ref"
            ),
        });
    }

    let required_env_refs = required_env_refs_for_agent_provider_id(
        &config.agent.id,
        &provider_id,
        api_key_ref.as_deref(),
    );
    for env_ref in &required_env_refs {
        if !config.agent.env.iter().any(|name| name == env_ref) {
            config.agent.env.push(env_ref.clone());
        }
    }
    config.agent.model = None;
    config.agent.provider = Some(AgentProviderConfig {
        id: provider_id.clone(),
        model: None,
        api_key_ref: api_key_ref.clone(),
        custom: None,
    });
    let Some(agent_provider_id) = agent_provider_id_for_provider_id(&config.agent.id, &provider_id)
    else {
        return Err(StackError::InvalidParam {
            field: "provider",
            reason: format!(
                "provider `{}` is not supported for agent `{}`",
                provider_id, config.agent.id
            ),
        });
    };
    let model = match args.model {
        Some(model) => Some(resolve_agent_model_value(
            &home,
            &config,
            Some(agent_provider_id),
            &model,
        )?),
        None if claude_code_provider_has_profile_default_model(&config) => None,
        None => {
            if !entry.set_model {
                return Err(StackError::AgentConfigProvision {
                    path: config_path,
                    reason: format!(
                        "{} does not support model configuration through `acps agent set`",
                        entry.name
                    ),
                });
            }
            let Some(model) = select_agent_session_config_value(
                &home,
                &config,
                AgentSessionConfigCategory::Model,
            )?
            else {
                return Ok(());
            };
            Some(model)
        }
    };
    if let Some(model) = model
        && let Some(provider) = config.agent.provider.as_mut()
    {
        provider.model = Some(model);
    }

    let canonical = config.to_canonical_toml()?;
    let config = config::load_config_from_str(&canonical)?;
    if let Some(model) = config
        .agent
        .provider
        .as_ref()
        .and_then(|provider| provider.model.as_deref())
    {
        validate_agent_model_if_required(&home, &config, model)?;
    }
    let provisioned = provision_agent_headless_config(&config, &home)?;
    atomic_write_owner_only(&config_path, canonical.as_bytes())?;

    print_agent_set_agent(&config);
    println!(
        "provider: {}",
        config.agent.provider.as_ref().expect("provider set").id
    );
    if let Some(model) = config
        .agent
        .provider
        .as_ref()
        .and_then(|provider| provider.model.as_deref())
    {
        println!("model: {model}");
    }
    if let Some(api_key_ref) = api_key_ref.as_deref() {
        println!("api_key_ref: {api_key_ref}");
    }
    if required_env_refs.len() > 1 {
        println!("required_env_refs: {}", required_env_refs.join(", "));
    }
    let optional = optional_env_refs_for_agent_provider_id(
        &config.agent.id,
        &config.agent.provider.as_ref().expect("provider set").id,
    );
    if !optional.is_empty() {
        println!("optional_env_refs: {}", optional.join(", "));
    }
    for item in provisioned {
        println!("{}: {}", item.label, item.path.display());
    }
    print_agent_set_effective_notice_for(Some(&config.agent.id));
    Ok(())
}

fn run_codex_openai_set(
    home: &Path,
    mut config: Config,
    config_path: PathBuf,
    args: AgentSetArgs,
    provider_id: String,
) -> Result<()> {
    if args.api_key_ref.is_some() {
        return Err(StackError::AgentConfigProvision {
            path: config_path,
            reason: "Codex OpenAI uses Codex-native auth; do not pass --api-key-ref".to_owned(),
        });
    }
    let Some(requested_model) = args.model else {
        return Err(StackError::InvalidParam {
            field: "model",
            reason: "pass --model <model-id> when setting Codex OpenAI provider".to_owned(),
        });
    };
    config.agent.model = None;
    config.agent.provider = Some(AgentProviderConfig {
        id: provider_id,
        model: Some(requested_model),
        api_key_ref: None,
        custom: None,
    });
    let canonical = config.to_canonical_toml()?;
    let mut config = config::load_config_from_str(&canonical)?;
    provision_agent_headless_config(&config, home)?;
    let requested_model = config
        .agent
        .provider
        .as_ref()
        .and_then(|provider| provider.model.as_deref())
        .expect("provider model set");
    let model = resolve_agent_model_value(home, &config, Some("openai"), requested_model)?;
    if let Some(provider) = config.agent.provider.as_mut() {
        provider.model = Some(model);
    }
    let canonical = config.to_canonical_toml()?;
    let config = config::load_config_from_str(&canonical)?;
    validate_agent_session_config_value(
        home,
        &config,
        AgentSessionConfigCategory::Model,
        config
            .agent
            .provider
            .as_ref()
            .and_then(|provider| provider.model.as_deref())
            .expect("provider model set"),
    )?;
    let provisioned = provision_agent_headless_config(&config, home)?;
    atomic_write_owner_only(&config_path, canonical.as_bytes())?;

    print_agent_set_agent(&config);
    println!(
        "provider: {}",
        config.agent.provider.as_ref().expect("provider set").id
    );
    if let Some(model) = config
        .agent
        .provider
        .as_ref()
        .and_then(|provider| provider.model.as_deref())
    {
        println!("model: {model}");
    }
    for item in provisioned {
        println!("{}: {}", item.label, item.path.display());
    }
    print_agent_set_effective_notice_for(Some(&config.agent.id));
    Ok(())
}

fn run_agent_custom_provider_set(
    mut config: Config,
    config_path: PathBuf,
    home: &Path,
    args: AgentSetArgs,
    entry: &RegistryEntry,
    provider_id: String,
) -> Result<()> {
    if !entry.allow_custom_provider {
        return Err(StackError::InvalidParam {
            field: "custom-provider",
            reason: format!("{} does not support custom provider setup", entry.name),
        });
    }
    if !entry.allow_custom_model {
        return Err(StackError::InvalidParam {
            field: "custom-provider",
            reason: format!("{} does not support custom model setup", entry.name),
        });
    }
    let provider_name = required_custom_arg("provider-name", args.provider_name)?;
    let base_url = required_custom_arg("base-url", args.base_url)?;
    let api_key_ref = required_custom_arg("api-key-ref", args.api_key_ref)?;
    let model = required_custom_arg("model", args.model)?;
    let model_name = args.model_name.unwrap_or_else(|| model.clone());
    let api = parse_custom_provider_api(
        args.provider_api.as_deref(),
        default_custom_provider_api(&config.agent.id),
    )?;
    validate_custom_provider_api_for_agent(&config.agent.id, api, "provider-api")?;
    let context = parse_custom_token_limit(
        "context",
        args.context.as_deref(),
        DEFAULT_CUSTOM_MODEL_CONTEXT,
    )?;
    let output_max_tokens = parse_custom_token_limit(
        "output-max-tokens",
        args.output_max_tokens.as_deref(),
        DEFAULT_CUSTOM_MODEL_OUTPUT_MAX_TOKENS,
    )?;

    if !config.agent.env.iter().any(|name| name == &api_key_ref) {
        config.agent.env.push(api_key_ref.clone());
    }
    config.agent.model = None;
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
    let config = config::load_config_from_str(&canonical)?;
    let provisioned = provision_agent_headless_config(&config, home)?;
    atomic_write_owner_only(&config_path, canonical.as_bytes())?;

    print_agent_set_agent(&config);
    println!(
        "provider: {}",
        config.agent.provider.as_ref().expect("provider set").id
    );
    println!(
        "model: {}",
        config
            .agent
            .provider
            .as_ref()
            .and_then(|provider| provider.model.as_deref())
            .unwrap_or("")
    );
    println!("api_key_ref: {api_key_ref}");
    for item in provisioned {
        println!("{}: {}", item.label, item.path.display());
    }
    print_agent_set_effective_notice_for(Some(&config.agent.id));
    Ok(())
}

pub(in crate::cli) fn required_custom_arg(
    field: &'static str,
    value: Option<String>,
) -> Result<String> {
    value
        .filter(|value| !value.trim().is_empty() && value.trim().len() == value.len())
        .ok_or_else(|| StackError::InvalidParam {
            field,
            reason: format!("--{field} is required for --custom-provider"),
        })
}

pub(in crate::cli) fn default_custom_provider_api(agent_id: &str) -> CustomProviderApi {
    if agent_id == "codex" {
        CustomProviderApi::Responses
    } else if agent_id == CLAUDE_CODE_AGENT_ID {
        CustomProviderApi::AnthropicMessages
    } else {
        CustomProviderApi::ChatCompletions
    }
}

pub(in crate::cli) fn parse_custom_provider_api(
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

pub(in crate::cli) fn validate_custom_provider_api_for_agent(
    agent_id: &str,
    api: CustomProviderApi,
    field: &'static str,
) -> Result<()> {
    if agent_id == "codex" && api != CustomProviderApi::Responses {
        return Err(StackError::InvalidParam {
            field,
            reason: "Codex custom providers only support responses".to_owned(),
        });
    }
    if agent_id == CLAUDE_CODE_AGENT_ID && api != CustomProviderApi::AnthropicMessages {
        return Err(StackError::InvalidParam {
            field,
            reason: "Claude Code custom providers only support anthropic-messages".to_owned(),
        });
    }
    if agent_id != CLAUDE_CODE_AGENT_ID && api == CustomProviderApi::AnthropicMessages {
        return Err(StackError::InvalidParam {
            field,
            reason: "anthropic-messages custom providers only support Claude Code".to_owned(),
        });
    }
    Ok(())
}

pub(in crate::cli) fn parse_custom_token_limit(
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

fn reject_custom_provider_args(args: &AgentSetArgs) -> Result<()> {
    if args.custom_provider
        || args.provider_name.is_some()
        || args.base_url.is_some()
        || args.provider_api.is_some()
        || args.model_name.is_some()
        || args.context.is_some()
        || args.output_max_tokens.is_some()
    {
        return Err(StackError::InvalidParam {
            field: "custom-provider",
            reason: "custom provider flags require --custom-provider".to_owned(),
        });
    }
    Ok(())
}

fn run_agent_model_set(
    mut config: Config,
    config_path: PathBuf,
    home: &Path,
    args: AgentSetArgs,
    entry: &RegistryEntry,
) -> Result<()> {
    reject_custom_provider_args(&args)?;
    if args.api_key_ref.is_some() {
        return Err(StackError::InvalidParam {
            field: "api-key-ref",
            reason: "--api-key-ref requires --provider".to_owned(),
        });
    }
    if !entry.set_model {
        return Err(StackError::AgentConfigProvision {
            path: config_path,
            reason: format!(
                "{} does not support model configuration through `acps agent set`",
                entry.name
            ),
        });
    }
    if entry.set_provider && config.agent.provider.is_none() {
        return Err(StackError::InvalidParam {
            field: "provider",
            reason: format!(
                "pass --provider <provider-id> when setting a model for {}",
                entry.name
            ),
        });
    }
    let Some(model) = args.model else {
        return Err(StackError::InvalidParam {
            field: "model",
            reason: "pass --model <model-id>, --provider <provider-id>, or --mode <mode>"
                .to_owned(),
        });
    };

    let required_env_refs = if let Some(provider) = config.agent.provider.as_ref() {
        required_env_refs_for_agent_provider_id(
            &config.agent.id,
            &provider.id,
            provider.api_key_ref.as_deref(),
        )
    } else {
        env_refs_for_agent_id(&config.agent.id)
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    for env_ref in &required_env_refs {
        if !config.agent.env.iter().any(|name| name == env_ref) {
            config.agent.env.push(env_ref.clone());
        }
    }
    let agent_provider_id = config
        .agent
        .provider
        .as_ref()
        .and_then(|provider| agent_provider_id_for_provider_id(&config.agent.id, &provider.id));
    let model = resolve_agent_model_value(home, &config, agent_provider_id, &model)?;
    if let Some(provider) = config.agent.provider.as_mut() {
        provider.model = Some(model);
        config.agent.model = None;
    } else {
        config.agent.model = Some(model);
    }

    let canonical = config.to_canonical_toml()?;
    let config = config::load_config_from_str(&canonical)?;
    let model_value = config
        .agent
        .provider
        .as_ref()
        .and_then(|provider| provider.model.as_deref())
        .or(config.agent.model.as_deref())
        .expect("agent model set");
    validate_agent_model_if_required(home, &config, model_value)?;
    let provisioned = provision_agent_headless_config(&config, home)?;
    atomic_write_owner_only(&config_path, canonical.as_bytes())?;

    print_agent_set_agent(&config);
    if let Some(provider) = config.agent.provider.as_ref() {
        println!("provider: {}", provider.id);
    }
    println!("model: {model_value}");
    if !required_env_refs.is_empty() {
        println!("required_env_refs: {}", required_env_refs.join(", "));
    }
    for item in provisioned {
        println!("{}: {}", item.label, item.path.display());
    }
    print_agent_set_effective_notice_for(Some(&config.agent.id));
    Ok(())
}

fn run_agent_mode_set(
    mut config: Config,
    config_path: PathBuf,
    home: &Path,
    args: AgentSetArgs,
    entry: &RegistryEntry,
    mode: String,
) -> Result<()> {
    reject_custom_provider_args(&args)?;
    if args.provider.is_some() || args.model.is_some() || args.api_key_ref.is_some() {
        return Err(StackError::InvalidParam {
            field: "mode",
            reason: "--mode cannot be combined with --provider, --model, or --api-key-ref"
                .to_owned(),
        });
    }
    if !entry.set_mode {
        return Err(StackError::AgentConfigProvision {
            path: config_path,
            reason: format!(
                "{} does not support mode configuration through `acps agent set`",
                entry.name
            ),
        });
    }
    config.agent.mode = Some(mode);
    let canonical = config.to_canonical_toml()?;
    let config = config::load_config_from_str(&canonical)?;
    let mode = config.agent.mode.as_deref().expect("mode set");
    validate_agent_session_config_value(home, &config, AgentSessionConfigCategory::Mode, mode)?;
    atomic_write_owner_only(&config_path, canonical.as_bytes())?;
    print_agent_set_agent(&config);
    println!("mode: {mode}");
    print_agent_set_effective_notice_for(Some(&config.agent.id));
    Ok(())
}

fn print_agent_set_agent(config: &Config) {
    println!("agent: {}", config.agent.id);
}

/// Effective-notice variant aware of the configured agent. Most agents
/// read provider/model from their on-disk config at process start, so a
/// running agent must be restarted through `POST /v1/agent/restart`
/// before the new settings take effect. Goose is the exception: clients
/// can switch model live via ACP `session/set_config_option`. When
/// `agent_id` is provided we surface the correct guidance; passing
/// `None` keeps the generic "new sessions" message for paths where the
/// agent id is not known to the caller.
pub(in crate::cli) fn print_agent_set_effective_notice_for(agent_id: Option<&str>) {
    match agent_id {
        Some("goose") => {
            println!(
                "model can be switched live via ACP session/set_config_option; \
                 other changes apply to new sessions"
            );
        }
        Some(_) => {
            println!(
                "settings take effect on new sessions; restart the supervised \
                 agent (`POST /v1/agent/restart`) to reload from disk"
            );
        }
        None => println!("settings will take effect on new sessions"),
    }
}

pub(in crate::cli) fn default_api_key_ref_for_agent_provider(
    agent_id: &str,
    provider_id: &str,
) -> Option<String> {
    if agent_id == "codex" && provider_id == "openai" {
        return None;
    }
    env_var_for_agent_provider_id(agent_id, provider_id).map(str::to_owned)
}

pub(in crate::cli) fn resolve_agent_model_value(
    home: &Path,
    config: &Config,
    provider_id: Option<&str>,
    model_id: &str,
) -> Result<String> {
    if claude_code_provider_model_is_explicit(config) {
        return Ok(model_id.to_owned());
    }
    let response = read_agent_new_session_response(home, config)?;
    resolve_advertised_model_value(&response, provider_id, model_id)
}

fn validate_agent_model_if_required(home: &Path, config: &Config, model_value: &str) -> Result<()> {
    if claude_code_provider_model_is_explicit(config) {
        return Ok(());
    }
    validate_agent_session_config_value(
        home,
        config,
        AgentSessionConfigCategory::Model,
        model_value,
    )
}

pub(in crate::cli) fn claude_code_provider_model_is_explicit(config: &Config) -> bool {
    if config.agent.id != CLAUDE_CODE_AGENT_ID {
        return false;
    }
    config.agent.provider.as_ref().is_some_and(|provider| {
        provider.custom.is_some() || is_claude_code_profiled_provider(&provider.id)
    })
}

pub(in crate::cli) fn model_values_for_cli_display(
    config: &Config,
    values: Vec<String>,
) -> Vec<String> {
    let Some(default_model) = claude_code_profile_default_model(config) else {
        return values;
    };
    let mut filtered = Vec::new();
    for value in values {
        if is_claude_code_builtin_model_alias(&value) {
            continue;
        }
        if !filtered.iter().any(|existing| existing == &value) {
            filtered.push(value);
        }
    }
    if !filtered.iter().any(|value| value == default_model) {
        filtered.insert(0, default_model.to_owned());
    }
    filtered
}

fn claude_code_profile_default_model(config: &Config) -> Option<&'static str> {
    if config.agent.id != CLAUDE_CODE_AGENT_ID {
        return None;
    }
    config
        .agent
        .provider
        .as_ref()
        .and_then(|provider| profile_for_provider_id(&provider.id))
        .and_then(|profile| profile.default_model.as_deref())
        .filter(|model| !model.trim().is_empty())
}

fn is_claude_code_builtin_model_alias(value: &str) -> bool {
    matches!(
        value.trim(),
        "best" | "default" | "fable" | "opus" | "sonnet" | "haiku"
    )
}

fn claude_code_provider_has_profile_default_model(config: &Config) -> bool {
    claude_code_profile_default_model(config).is_some()
}

fn select_agent_session_config_value(
    home: &Path,
    config: &Config,
    category: AgentSessionConfigCategory,
) -> Result<Option<String>> {
    let values = agent_session_config_values(home, config, category)?;

    if !io::stdin().is_terminal() {
        println!("available {} values:", category.id());
        for value in values {
            println!("{value}");
        }
        println!(
            "rerun with `--{} <{}>` to apply agent config",
            category.id(),
            category.id()
        );
        return Ok(None);
    }

    println!("available {} values:", category.id());
    for (index, value) in values.iter().enumerate() {
        println!("  {}. {value}", index + 1);
    }
    print!("Select {} number: ", category.id());
    io::stdout()
        .flush()
        .map_err(|source| StackError::ServeIo { source })?;
    let mut choice = String::new();
    io::stdin()
        .read_line(&mut choice)
        .map_err(|source| StackError::ServeIo { source })?;
    let index: usize = choice
        .trim()
        .parse()
        .map_err(|_| StackError::AgentConfigProvision {
            path: PathBuf::from("ACP session config options"),
            reason: format!("{} selection must be a number", category.id()),
        })?;
    values
        .get(index.saturating_sub(1))
        .cloned()
        .map(Some)
        .ok_or_else(|| StackError::AgentConfigProvision {
            path: PathBuf::from("ACP session config options"),
            reason: format!("{} selection `{index}` is out of range", category.id()),
        })
}

pub(in crate::cli) fn validate_agent_session_config_value(
    home: &Path,
    config: &Config,
    category: AgentSessionConfigCategory,
    value: &str,
) -> Result<()> {
    let response = read_agent_new_session_response(home, config)?;
    match category {
        AgentSessionConfigCategory::Model => {
            session_model_selection_for_value(&response, value).map(|_| ())
        }
        AgentSessionConfigCategory::Mode => {
            session_config_id_for_value(response.config_options.as_deref(), category, value)
                .map(|_| ())
        }
    }
}

fn agent_session_config_values(
    home: &Path,
    config: &Config,
    category: AgentSessionConfigCategory,
) -> Result<Vec<String>> {
    let response = read_agent_new_session_response(home, config)?;
    match category {
        AgentSessionConfigCategory::Model => session_model_values(&response)
            .map(|values| model_values_for_cli_display(config, values)),
        AgentSessionConfigCategory::Mode => {
            session_config_values(response.config_options.as_deref(), category)
        }
    }
}

fn read_agent_new_session_response(
    home: &Path,
    config: &Config,
) -> Result<agent_client_protocol::schema::v1::NewSessionResponse> {
    fetch_session_config(home, config)
}
