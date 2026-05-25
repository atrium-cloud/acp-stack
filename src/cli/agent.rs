use crate::config::{
    self, AgentCustomProviderConfig, AgentProviderConfig, Config, CustomProviderApi,
    DEFAULT_CUSTOM_MODEL_CONTEXT, DEFAULT_CUSTOM_MODEL_OUTPUT_MAX_TOKENS,
};
use crate::error::{Result, StackError};
use crate::fs_util::{
    atomic_write_owner_only, create_dir_owner_only, home_dir, parent_dir, pre_create_owner_only,
    set_owner_only_file,
};
use crate::runtime::agent::acp_bridge::{
    AcpBridge, AgentSessionConfigCategory, AgentSessionModelSelection, SessionEventSink,
    session_config_id_for_value, session_config_values, session_model_selection_for_value,
    session_model_values,
};
use crate::runtime::agent::agent_headless_config::provision_agent_headless_config;
use crate::runtime::agent::provider_keys::{
    agent_provider_id_for_provider_id, env_refs_for_agent_id, env_var_for_agent_provider_id,
    optional_env_refs_for_provider_id, provider_id_is_known, provider_id_supports_agent,
    required_env_refs_for_provider_id,
};
use crate::runtime::install::agent_installer::{
    STEP_ADAPTER, STEP_HARNESS, STEP_INSTALL, install_resolved, run_installer,
};
use crate::runtime::install::agent_registry::RegistryCatalog;
use crate::runtime::install::agent_registry::{RegistryEntry, RegistryKind};
use crate::secrets::SecretStore;
use crate::state::{StateStore, default_state_path};
use agent_client_protocol::schema::{ContentBlock, PromptRequest, StopReason, TextContent};
use clap::{Args, Subcommand};
use std::collections::HashMap;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::Notify;

use super::core::daemon_base_url;

const ACP_CONFIG_OPTIONS_FIXTURE_ENV: &str = "ACP_STACK_AGENT_CONFIG_OPTIONS_PATH";
const ACP_NEW_SESSION_RESPONSE_FIXTURE_ENV: &str = "ACP_STACK_AGENT_NEW_SESSION_RESPONSE_PATH";
const DEFAULT_AGENT_TEST_PROMPT: &str =
    "Reply with exactly this text and nothing else: acp-stack test ok";
const DEFAULT_AGENT_TEST_TIMEOUT: &str = "60s";
const DEFAULT_AGENT_TEST_PROGRESS_TIMEOUT: &str = "30s";

#[derive(Debug, Subcommand)]
pub enum AgentCommand {
    /// Install the configured ACP agent or adapter.
    Install,
    /// Ask the running daemon to start the configured agent.
    Start,
    /// Ask the running daemon to stop the configured agent.
    Stop,
    /// Print the latest persisted agent state from SQLite.
    Status,
    /// Report whether the installed managed harness/adapter is stale against upstream.
    Check,
    /// Start the configured agent and send a real ACP prompt.
    Test(AgentTestArgs),
    /// Set the provider id, model, and API-key ref used by generated agent config.
    Set(AgentSetArgs),
}

#[derive(Debug, Args)]
pub struct AgentTestArgs {
    /// Prompt text to send. Defaults to a minimal compatibility prompt.
    #[arg(long)]
    prompt: Option<String>,
    /// Maximum time to wait for the prompt request to finish.
    #[arg(long, default_value = DEFAULT_AGENT_TEST_TIMEOUT)]
    timeout: String,
    /// Maximum time to wait for either progress or terminal prompt completion.
    #[arg(long = "progress-timeout", default_value = DEFAULT_AGENT_TEST_PROGRESS_TIMEOUT)]
    progress_timeout: String,
}

#[derive(Debug, Args)]
pub struct AgentSetArgs {
    /// Configure a provider/model outside the embedded provider mapping.
    #[arg(long)]
    custom_provider: bool,
    /// Provider id, such as opencode-go, openai, or anthropic.
    #[arg(long)]
    provider: Option<String>,
    /// Display name for a custom provider.
    #[arg(long = "provider-name")]
    provider_name: Option<String>,
    /// Base URL for a custom provider.
    #[arg(long = "base-url")]
    base_url: Option<String>,
    /// API family for a custom provider: chat-completions or responses.
    #[arg(long = "provider-api")]
    provider_api: Option<String>,
    /// Provider-qualified model id or model pattern.
    #[arg(long)]
    model: Option<String>,
    /// Display name for a custom model.
    #[arg(long = "model-name")]
    model_name: Option<String>,
    /// Context window in tokens for a custom model.
    #[arg(long)]
    context: Option<String>,
    /// Maximum output tokens for a custom model.
    #[arg(long = "output-max-tokens")]
    output_max_tokens: Option<String>,
    /// Agent session mode for agents that expose mode as an ACP config option.
    #[arg(long)]
    mode: Option<String>,
    /// Secret ref to inject for this provider. Defaults from provider metadata.
    #[arg(long)]
    api_key_ref: Option<String>,
}

pub(super) fn run_agent_command(command: AgentCommand) -> Result<()> {
    match command {
        AgentCommand::Install => run_agent_install(),
        AgentCommand::Start => run_agent_daemon_post("/v1/agent/start", "start"),
        AgentCommand::Stop => run_agent_daemon_post("/v1/agent/stop", "stop"),
        AgentCommand::Status => run_agent_status(),
        AgentCommand::Check => run_agent_check(),
        AgentCommand::Test(args) => run_agent_test(args),
        AgentCommand::Set(args) => run_agent_set(args),
    }
}

fn run_agent_set(args: AgentSetArgs) -> Result<()> {
    let home = home_dir()?;
    let config_path = config::default_config_path()?;
    let mut config = Config::load_from_path(&config_path)?;
    let registry = RegistryCatalog::load_with_override(&operator_registry_override(&home))?;
    let entry =
        registry
            .lookup(&config.agent.id)
            .ok_or_else(|| StackError::AgentRegistryMissing {
                id: config.agent.id.clone(),
            })?;
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
    if default_api_key_ref.is_none() {
        return Err(StackError::AgentConfigProvision {
            path: config_path,
            reason: format!(
                "provider `{}` has no API-key env mapping for agent `{}`",
                provider_id, config.agent.id
            ),
        });
    }

    let api_key_ref = args.api_key_ref.or(default_api_key_ref).ok_or_else(|| {
        StackError::AgentConfigProvision {
            path: config_path.clone(),
            reason: format!(
                "provider `{provider_id}` has no default API-key env var; pass --api-key-ref"
            ),
        }
    })?;

    let required_env_refs = required_env_refs_for_provider_id(&provider_id, &api_key_ref);
    for env_ref in &required_env_refs {
        if !config.agent.env.iter().any(|name| name == env_ref) {
            config.agent.env.push(env_ref.clone());
        }
    }
    config.agent.model = None;
    config.agent.provider = Some(AgentProviderConfig {
        id: provider_id.clone(),
        model: None,
        api_key_ref: Some(api_key_ref.clone()),
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
        Some(model) => resolve_agent_model_value(&home, &config, Some(agent_provider_id), &model)?,
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
            model
        }
    };
    if let Some(provider) = config.agent.provider.as_mut() {
        provider.model = Some(model);
    }

    let canonical = config.to_canonical_toml()?;
    let config = config::load_config_from_str(&canonical)?;
    validate_agent_session_config_value(
        &home,
        &config,
        AgentSessionConfigCategory::Model,
        config
            .agent
            .provider
            .as_ref()
            .and_then(|provider| provider.model.as_deref())
            .expect("provider model set"),
    )?;
    let provisioned = provision_agent_headless_config(&config, &home)?;
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
    if required_env_refs.len() > 1 {
        println!("required_env_refs: {}", required_env_refs.join(", "));
    }
    let optional = optional_env_refs_for_provider_id(
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
    if config.agent.id == "codex" && api != CustomProviderApi::Responses {
        return Err(StackError::InvalidParam {
            field: "provider-api",
            reason: "Codex custom providers only support responses".to_owned(),
        });
    }
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

fn required_custom_arg(field: &'static str, value: Option<String>) -> Result<String> {
    value
        .filter(|value| !value.trim().is_empty() && value.trim().len() == value.len())
        .ok_or_else(|| StackError::InvalidParam {
            field,
            reason: format!("--{field} is required for --custom-provider"),
        })
}

fn default_custom_provider_api(agent_id: &str) -> CustomProviderApi {
    if agent_id == "codex" {
        CustomProviderApi::Responses
    } else {
        CustomProviderApi::ChatCompletions
    }
}

fn parse_custom_provider_api(
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

fn parse_custom_token_limit(field: &'static str, value: Option<&str>, default: u64) -> Result<u64> {
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
    if entry.set_provider {
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

    let required_env_refs = env_refs_for_agent_id(&config.agent.id)
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    for env_ref in &required_env_refs {
        if !config.agent.env.iter().any(|name| name == env_ref) {
            config.agent.env.push(env_ref.clone());
        }
    }
    config.agent.provider = None;
    let model = resolve_agent_model_value(home, &config, None, &model)?;
    config.agent.model = Some(model);

    let canonical = config.to_canonical_toml()?;
    let config = config::load_config_from_str(&canonical)?;
    validate_agent_session_config_value(
        home,
        &config,
        AgentSessionConfigCategory::Model,
        config.agent.model.as_deref().expect("agent model set"),
    )?;
    let provisioned = provision_agent_headless_config(&config, home)?;
    atomic_write_owner_only(&config_path, canonical.as_bytes())?;

    print_agent_set_agent(&config);
    println!("model: {}", config.agent.model.as_deref().unwrap_or(""));
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
fn print_agent_set_effective_notice_for(agent_id: Option<&str>) {
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

fn default_api_key_ref_for_agent_provider(agent_id: &str, provider_id: &str) -> Option<String> {
    if agent_id == "codex" && provider_id == "openai" {
        return None;
    }
    env_var_for_agent_provider_id(agent_id, provider_id).map(str::to_owned)
}

fn resolve_agent_model_value(
    home: &Path,
    config: &Config,
    provider_id: Option<&str>,
    model_id: &str,
) -> Result<String> {
    let response = read_agent_new_session_response(home, config)?;
    if session_model_selection_for_value(&response, model_id).is_ok() {
        return Ok(model_id.to_owned());
    }
    let values = session_model_values(&response)?;
    if let Some(provider_id) = provider_id {
        let provider_qualified = format!("{provider_id}/{model_id}");
        if values.iter().any(|value| value == &provider_qualified)
            && session_model_selection_for_value(&response, &provider_qualified).is_ok()
        {
            return Ok(provider_qualified);
        }
    }
    let mut base_matches = values
        .iter()
        .filter(|value| advertised_model_base_matches(value, provider_id, model_id))
        .cloned()
        .collect::<Vec<_>>();
    base_matches.sort();
    base_matches.dedup();
    if base_matches.len() == 1
        && session_model_selection_for_value(&response, &base_matches[0]).is_ok()
    {
        return Ok(base_matches.remove(0));
    }
    session_model_selection_for_value(&response, model_id).map(|_| model_id.to_owned())
}

fn advertised_model_base_matches(value: &str, provider_id: Option<&str>, model_id: &str) -> bool {
    let base = value.split_once('[').map_or(value, |(base, _)| base);
    if let Some((provider, model)) = base.split_once('/') {
        return provider_id.is_none_or(|provider_id| provider == provider_id) && model == model_id;
    }
    base == model_id
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

fn validate_agent_session_config_value(
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
        AgentSessionConfigCategory::Model => session_model_values(&response),
        AgentSessionConfigCategory::Mode => {
            session_config_values(response.config_options.as_deref(), category)
        }
    }
}

fn read_agent_new_session_response(
    home: &Path,
    config: &Config,
) -> Result<agent_client_protocol::schema::NewSessionResponse> {
    if let Some(path) = std::env::var_os(ACP_CONFIG_OPTIONS_FIXTURE_ENV) {
        let path = PathBuf::from(path);
        let body = std::fs::read_to_string(&path).map_err(|source| StackError::ConfigRead {
            path: path.clone(),
            source,
        })?;
        let options: Vec<agent_client_protocol::schema::SessionConfigOption> =
            serde_json::from_str(&body).map_err(|source| StackError::AgentConfigProvision {
                path,
                reason: format!("ACP session config options fixture is invalid: {source}"),
            })?;
        return Ok(
            agent_client_protocol::schema::NewSessionResponse::new("fixture")
                .config_options(options),
        );
    }

    if let Some(path) = std::env::var_os(ACP_NEW_SESSION_RESPONSE_FIXTURE_ENV) {
        let path = PathBuf::from(path);
        let body = std::fs::read_to_string(&path).map_err(|source| StackError::ConfigRead {
            path: path.clone(),
            source,
        })?;
        return serde_json::from_str(&body).map_err(|source| StackError::AgentConfigProvision {
            path,
            reason: format!("ACP session/new fixture is invalid: {source}"),
        });
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|source| StackError::ServeIo { source })?;
    let env = resolve_agent_env_for_cli(home, config)?;
    let cwd = config
        .agent
        .cwd
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&config.workspace.root));

    runtime.block_on(async move {
        let bridge = AcpBridge::spawn(
            &config.agent,
            env,
            cwd.clone(),
            Arc::new(NoopSessionEventSink),
            None,
        )
        .await?;
        let response = bridge.new_session(cwd, Vec::new()).await;
        let shutdown = bridge.shutdown().await;
        let response = response?;
        shutdown?;
        Ok(response)
    })
}

struct NoopSessionEventSink;

impl SessionEventSink for NoopSessionEventSink {
    fn append<'a>(
        &'a self,
        _session_id: &'a str,
        _kind: &'a str,
        _payload_json: &'a str,
    ) -> futures::future::BoxFuture<'a, ()> {
        Box::pin(async {})
    }
}

struct AgentTestSessionEventSink {
    updates: AtomicUsize,
    notify: Notify,
}

impl AgentTestSessionEventSink {
    fn new() -> Self {
        Self {
            updates: AtomicUsize::new(0),
            notify: Notify::new(),
        }
    }

    fn update_count(&self) -> usize {
        self.updates.load(Ordering::SeqCst)
    }

    async fn wait_for_update_after(&self, observed_updates: usize) {
        loop {
            if self.update_count() > observed_updates {
                return;
            }
            self.notify.notified().await;
        }
    }
}

impl SessionEventSink for AgentTestSessionEventSink {
    fn append<'a>(
        &'a self,
        _session_id: &'a str,
        kind: &'a str,
        _payload_json: &'a str,
    ) -> futures::future::BoxFuture<'a, ()> {
        Box::pin(async move {
            if kind == "session.update" {
                self.updates.fetch_add(1, Ordering::SeqCst);
                self.notify.notify_waiters();
            }
        })
    }
}

struct AgentTestReport {
    session_id: String,
    stop_reason: StopReason,
    updates: usize,
}

/// Run a real-prompt testflight at the tail of `acps init`. Uses the registry
/// entry's `testflight_prompt` if present (else the default) and verifies the
/// declared `testflight_expect_fs` artifact post-prompt. Surfaces the same
/// "ok / session_id / stop_reason / updates / fs_smoke" lines as
/// `acps agent test` so the operator sees consistent output regardless of
/// which entry point they used.
pub(super) fn run_init_testflight(
    home: &Path,
    config: &Config,
    registry: &RegistryCatalog,
) -> Result<()> {
    let args = AgentTestArgs {
        prompt: None,
        timeout: DEFAULT_AGENT_TEST_TIMEOUT.to_owned(),
        progress_timeout: DEFAULT_AGENT_TEST_PROGRESS_TIMEOUT.to_owned(),
    };
    run_agent_test_with(home, config, registry, args)
}

fn run_agent_test(args: AgentTestArgs) -> Result<()> {
    let home = home_dir()?;
    let config = Config::load_from_default_path()?;
    let registry = RegistryCatalog::load_with_override(&operator_registry_override(&home))?;
    run_agent_test_with(&home, &config, &registry, args)
}

fn run_agent_test_with(
    home: &Path,
    config: &Config,
    registry: &RegistryCatalog,
    args: AgentTestArgs,
) -> Result<()> {
    let entry =
        registry
            .lookup(&config.agent.id)
            .ok_or_else(|| StackError::AgentRegistryMissing {
                id: config.agent.id.clone(),
            })?;
    entry.ensure_supported()?;

    let prompt_source = if args.prompt.is_some() {
        AgentTestPromptSource::CliFlag
    } else if entry.testflight_prompt.is_some() {
        AgentTestPromptSource::Registry
    } else {
        AgentTestPromptSource::Default
    };
    let prompt = args
        .prompt
        .clone()
        .or_else(|| entry.testflight_prompt.clone())
        .unwrap_or_else(|| DEFAULT_AGENT_TEST_PROMPT.to_owned());
    let expect_fs = match prompt_source {
        AgentTestPromptSource::Registry => entry.testflight_expect_fs.clone(),
        AgentTestPromptSource::CliFlag | AgentTestPromptSource::Default => None,
    };
    let workspace_root = PathBuf::from(&config.workspace.root);
    let timeout = parse_agent_test_duration("agent test --timeout", &args.timeout)?;
    let progress_timeout =
        parse_agent_test_duration("agent test --progress-timeout", &args.progress_timeout)?;
    let env = resolve_agent_env_for_cli(home, config)?;
    let cwd = config
        .agent
        .cwd
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&config.workspace.root));
    let agent = config.agent.clone();

    if let Some(rel) = expect_fs.as_deref() {
        prepare_testflight_expect_fs(&workspace_root, rel)?;
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|source| StackError::ServeIo { source })?;
    let report = runtime.block_on(async move {
        run_agent_test_inner(agent, env, cwd, prompt, timeout, progress_timeout).await
    })?;

    let fs_outcome = match expect_fs.as_deref() {
        Some(rel) => Some(verify_testflight_expect_fs(&workspace_root, rel)?),
        None => None,
    };

    println!("agent test: ok");
    println!("agent: {}", config.agent.id);
    println!("prompt: {}", prompt_source.label());
    println!("session_id: {}", report.session_id);
    println!("stop_reason: {}", stop_reason_label(report.stop_reason));
    println!("updates: {}", report.updates);
    if let Some(outcome) = fs_outcome {
        println!(
            "fs_smoke: ok ({} bytes at {})",
            outcome.bytes,
            outcome.path.display()
        );
    }
    Ok(())
}

#[derive(Copy, Clone)]
enum AgentTestPromptSource {
    CliFlag,
    Registry,
    Default,
}

impl AgentTestPromptSource {
    fn label(self) -> &'static str {
        match self {
            AgentTestPromptSource::CliFlag => "provided",
            AgentTestPromptSource::Registry => "registry",
            AgentTestPromptSource::Default => "default",
        }
    }
}

#[derive(Debug)]
struct TestflightFsOutcome {
    path: PathBuf,
    bytes: u64,
}

/// Verify the registry-declared testflight artifact lives under the workspace
/// after the prompt completes. Treats absence and zero-length files as
/// failures so the operator can distinguish "agent did not run the tool"
/// from "agent ran the tool successfully". Uses canonical paths to reject
/// an agent that resolved a symlink out of the workspace.
fn prepare_testflight_expect_fs(workspace_root: &Path, relative: &str) -> Result<()> {
    let path = testflight_expect_fs_path(workspace_root, relative)?;
    ensure_testflight_parent_within_workspace(workspace_root, &path)?;
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() => {
            std::fs::remove_file(&path).map_err(|source| StackError::AgentTestFailed {
                stage: "fs smoke".to_owned(),
                reason: format!(
                    "remove stale testflight artifact `{}` failed: {source}",
                    path.display()
                ),
            })?;
            Ok(())
        }
        Ok(metadata) => Err(StackError::AgentTestFailed {
            stage: "fs smoke".to_owned(),
            reason: format!(
                "pre-existing testflight artifact `{}` is {}; remove it before running testflight",
                path.display(),
                if metadata.file_type().is_symlink() {
                    "a symlink"
                } else {
                    "not a regular file"
                }
            ),
        }),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(StackError::AgentTestFailed {
            stage: "fs smoke".to_owned(),
            reason: format!(
                "stat pre-existing testflight artifact `{}` failed: {source}",
                path.display()
            ),
        }),
    }
}

fn verify_testflight_expect_fs(
    workspace_root: &Path,
    relative: &str,
) -> Result<TestflightFsOutcome> {
    let path = testflight_expect_fs_path(workspace_root, relative)?;
    let workspace =
        workspace_root
            .canonicalize()
            .map_err(|source| StackError::AgentTestFailed {
                stage: "fs smoke".to_owned(),
                reason: format!(
                    "canonicalize workspace root `{}` failed: {source}",
                    workspace_root.display()
                ),
            })?;
    let metadata =
        std::fs::symlink_metadata(&path).map_err(|source| StackError::AgentTestFailed {
            stage: "fs smoke".to_owned(),
            reason: format!(
                "expected agent to create `{}` (workspace-relative `{}`) but stat failed: {source}",
                path.display(),
                relative
            ),
        })?;
    if metadata.file_type().is_symlink() {
        return Err(StackError::AgentTestFailed {
            stage: "fs smoke".to_owned(),
            reason: format!(
                "expected agent to create regular file `{}`, but it is a symlink",
                path.display()
            ),
        });
    }
    if !metadata.is_file() {
        return Err(StackError::AgentTestFailed {
            stage: "fs smoke".to_owned(),
            reason: format!(
                "expected agent to create regular file `{}`, but it is not a regular file",
                path.display()
            ),
        });
    }
    let canonical_path = path
        .canonicalize()
        .map_err(|source| StackError::AgentTestFailed {
            stage: "fs smoke".to_owned(),
            reason: format!(
                "canonicalize testflight artifact `{}` failed: {source}",
                path.display()
            ),
        })?;
    if !canonical_path.starts_with(&workspace) {
        return Err(StackError::AgentTestFailed {
            stage: "fs smoke".to_owned(),
            reason: format!(
                "testflight artifact `{}` resolved outside workspace `{}`",
                canonical_path.display(),
                workspace.display()
            ),
        });
    }
    if metadata.len() == 0 {
        return Err(StackError::AgentTestFailed {
            stage: "fs smoke".to_owned(),
            reason: format!(
                "agent created `{}` but the file is empty; treating as no tool action",
                path.display()
            ),
        });
    }
    Ok(TestflightFsOutcome {
        path,
        bytes: metadata.len(),
    })
}

fn testflight_expect_fs_path(workspace_root: &Path, relative: &str) -> Result<PathBuf> {
    if Path::new(relative).is_absolute() || relative.split('/').any(|seg| seg == "..") {
        return Err(StackError::AgentTestFailed {
            stage: "fs smoke".to_owned(),
            reason: format!(
                "testflight_expect_fs `{relative}` must be a workspace-relative path with no `..` segments"
            ),
        });
    }
    Ok(workspace_root.join(relative))
}

fn ensure_testflight_parent_within_workspace(workspace_root: &Path, path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let workspace =
        workspace_root
            .canonicalize()
            .map_err(|source| StackError::AgentTestFailed {
                stage: "fs smoke".to_owned(),
                reason: format!(
                    "canonicalize workspace root `{}` failed: {source}",
                    workspace_root.display()
                ),
            })?;
    let parent = match parent.canonicalize() {
        Ok(parent) => parent,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(StackError::AgentTestFailed {
                stage: "fs smoke".to_owned(),
                reason: format!("canonicalize `{}` failed: {source}", parent.display()),
            });
        }
    };
    if parent.starts_with(&workspace) {
        Ok(())
    } else {
        Err(StackError::AgentTestFailed {
            stage: "fs smoke".to_owned(),
            reason: format!(
                "testflight artifact parent `{}` resolved outside workspace `{}`",
                parent.display(),
                workspace.display()
            ),
        })
    }
}

fn parse_agent_test_duration(field: &'static str, value: &str) -> Result<Duration> {
    let duration =
        config::parse_duration_string(value).ok_or(StackError::InvalidDurationField { field })?;
    if duration.is_zero() {
        return Err(StackError::InvalidDurationField { field });
    }
    Ok(duration)
}

async fn run_agent_test_inner(
    agent: crate::config::AgentConfig,
    env: HashMap<String, String>,
    cwd: PathBuf,
    prompt: String,
    prompt_timeout: Duration,
    progress_timeout: Duration,
) -> Result<AgentTestReport> {
    let sink = Arc::new(AgentTestSessionEventSink::new());
    let bridge = AcpBridge::spawn(&agent, env, cwd.clone(), sink.clone(), None)
        .await
        .map_err(agent_test_spawn_error)?;

    let result = async {
        let session = bridge
            .new_session(cwd, Vec::new())
            .await
            .map_err(|err| agent_test_error("session creation", err))?;
        apply_agent_test_session_config(&bridge, &agent, &session)
            .await
            .map_err(|err| agent_test_error("session creation", err))?;
        let request = PromptRequest::new(
            session.session_id.clone(),
            vec![ContentBlock::Text(TextContent::new(prompt))],
        );
        let stop_reason = run_agent_test_prompt(
            &bridge,
            request,
            sink.clone(),
            prompt_timeout,
            progress_timeout,
        )
        .await?;
        if stop_reason != StopReason::EndTurn {
            return Err(StackError::AgentTestFailed {
                stage: "prompt completion".to_owned(),
                reason: format!(
                    "expected stop_reason end_turn, got {}",
                    stop_reason_label(stop_reason)
                ),
            });
        }
        Ok(AgentTestReport {
            session_id: session.session_id.to_string(),
            stop_reason,
            updates: sink.update_count(),
        })
    }
    .await;

    let shutdown = bridge.shutdown().await;
    match (result, shutdown) {
        (Ok(report), Ok(_)) => Ok(report),
        (Err(err), _) => Err(err),
        (Ok(_), Err(err)) => Err(agent_test_error("shutdown", err)),
    }
}

async fn run_agent_test_prompt(
    bridge: &AcpBridge,
    request: PromptRequest,
    sink: Arc<AgentTestSessionEventSink>,
    prompt_timeout: Duration,
    progress_timeout: Duration,
) -> Result<StopReason> {
    let prompt_call = async {
        let prompt_future = bridge.prompt_session(request);
        tokio::pin!(prompt_future);
        let mut observed_updates = sink.update_count();
        loop {
            let progress_timer = tokio::time::sleep(progress_timeout);
            tokio::pin!(progress_timer);
            tokio::select! {
                result = &mut prompt_future => {
                    return result.map_err(|err| agent_test_error("prompt completion", err));
                }
                _ = sink.wait_for_update_after(observed_updates) => {
                    observed_updates = sink.update_count();
                }
                _ = &mut progress_timer => {
                    return Err(StackError::AgentTestFailed {
                        stage: "prompt/progress timeout".to_owned(),
                        reason: format!(
                            "no new session/update or terminal prompt response within {}",
                            human_duration(progress_timeout)
                        ),
                    });
                }
            }
        }
    };

    tokio::time::timeout(prompt_timeout, prompt_call)
        .await
        .map_err(|_| StackError::AgentTestFailed {
            stage: "prompt/progress timeout".to_owned(),
            reason: format!(
                "prompt did not complete within {}",
                human_duration(prompt_timeout)
            ),
        })?
}

async fn apply_agent_test_session_config(
    bridge: &AcpBridge,
    agent: &crate::config::AgentConfig,
    response: &agent_client_protocol::schema::NewSessionResponse,
) -> Result<()> {
    if let Some(mode) = agent.mode.as_deref() {
        let config_id = session_config_id_for_value(
            response.config_options.as_deref(),
            AgentSessionConfigCategory::Mode,
            mode,
        )?;
        bridge
            .set_session_config_option(response.session_id.clone(), &config_id, mode)
            .await?;
    }
    if let Some(model) = agent.model.as_deref().or_else(|| {
        agent
            .provider
            .as_ref()
            .and_then(|provider| provider.model.as_deref())
    }) {
        match session_model_selection_for_value(response, model)? {
            AgentSessionModelSelection::ConfigOption { config_id } => {
                bridge
                    .set_session_config_option(response.session_id.clone(), &config_id, model)
                    .await?;
            }
            AgentSessionModelSelection::LegacyModel => {
                bridge
                    .set_session_model(response.session_id.clone(), model)
                    .await?;
            }
        }
    }
    Ok(())
}

fn agent_test_spawn_error(error: StackError) -> StackError {
    let stage = match error {
        StackError::AgentSpawnFailed { .. } => "spawn/start",
        StackError::AgentInitializeFailed { .. } => "ACP initialize",
        _ => "spawn/start",
    };
    agent_test_error(stage, error)
}

fn agent_test_error(stage: &'static str, error: StackError) -> StackError {
    StackError::AgentTestFailed {
        stage: stage.to_owned(),
        reason: error.to_string(),
    }
}

fn stop_reason_label(reason: StopReason) -> String {
    match reason {
        StopReason::EndTurn => "end_turn".to_owned(),
        StopReason::MaxTokens => "max_tokens".to_owned(),
        StopReason::MaxTurnRequests => "max_turn_requests".to_owned(),
        StopReason::Refusal => "refusal".to_owned(),
        StopReason::Cancelled => "cancelled".to_owned(),
        other => format!("{other:?}").to_lowercase(),
    }
}

fn human_duration(duration: Duration) -> String {
    if duration.as_millis() < 1_000 {
        format!("{}ms", duration.as_millis())
    } else {
        format!("{}s", duration.as_secs())
    }
}

fn run_agent_daemon_post(path: &'static str, label: &'static str) -> Result<()> {
    let home = home_dir()?;
    let config = Config::load_from_default_path()?;
    let store = SecretStore::open(&home)?;
    let admin_key = store.get(&config.auth.admin_key_ref)?.to_owned();
    let base_url = daemon_base_url(config.api.public_url.as_deref(), &config.api.bind)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|source| StackError::ServeIo { source })?;
    let body = runtime.block_on(post_agent_daemon(&base_url, path, &admin_key))?;
    if label == "start" {
        let pid = body["data"]["pid"]
            .as_u64()
            .map(|pid| pid.to_string())
            .unwrap_or_else(|| "unknown".to_owned());
        println!("agent start: running");
        println!("pid: {pid}");
    } else {
        let exit_status = body["data"]["exit_status"]
            .as_i64()
            .map(|status| status.to_string())
            .unwrap_or_else(|| "unknown".to_owned());
        println!("agent stop: stopped");
        println!("exit_status: {exit_status}");
    }
    Ok(())
}

async fn post_agent_daemon(
    base_url: &str,
    path: &'static str,
    admin_key: &str,
) -> Result<serde_json::Value> {
    let url = format!("{}{}", base_url.trim_end_matches('/'), path);
    let response = reqwest::Client::new()
        .post(url)
        .bearer_auth(admin_key)
        .send()
        .await
        .map_err(|source| StackError::AgentApiRequest { path, source })?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|source| StackError::AgentApiRequest { path, source })?;
    if !status.is_success() {
        return Err(StackError::AgentApiStatus { path, status, body });
    }
    serde_json::from_str(&body).map_err(|err| StackError::AgentInitializeFailed {
        reason: format!("agent API response was not JSON: {err}"),
    })
}

fn run_agent_install() -> Result<()> {
    let home = home_dir()?;
    let config = Config::load_from_default_path()?;

    let state_path = default_state_path(&home);
    let state_dir = parent_dir(&state_path)?;
    create_dir_owner_only(state_dir)?;
    pre_create_owner_only(&state_path)?;
    let store = StateStore::open(&state_path)?;
    store.migrate()?;
    set_owner_only_file(&state_path)?;

    let workspace_root = PathBuf::from(config.workspace.root.clone());
    let log_base = crate::state::default_installer_log_base(&home);

    let outcome = if let Some(install) = config.agent.install.as_ref() {
        // Operator escape-hatch shell recipe takes precedence over the
        // embedded registry. Useful for private forks of an agent whose id
        // happens to clash with a curated entry.
        let env = resolve_agent_env_for_cli(&home, &config)?;
        let expected_sha256 = config.agent.expected_sha256.clone();
        run_installer(
            &config.agent.id,
            install,
            expected_sha256.as_deref(),
            env,
            &workspace_root,
            &store,
            Some(&log_base),
        )?
    } else {
        let registry = RegistryCatalog::load_with_override(&operator_registry_override(&home))?;
        let entry =
            registry
                .lookup(&config.agent.id)
                .ok_or_else(|| StackError::AgentRegistryMissing {
                    id: config.agent.id.clone(),
                })?;
        let dest = local_bin_dir(&home);
        install_resolved(
            &config.agent,
            entry,
            Default::default(),
            &workspace_root,
            &dest,
            &store,
            Some(&log_base),
        )?
    };

    println!("agent install: {}", outcome.label());
    println!("path: {}", outcome.path().display());
    println!("sha256: {}", outcome.sha256());
    Ok(())
}

fn operator_registry_override(home: &std::path::Path) -> PathBuf {
    home.join(".config").join("acp-stack").join("agents.toml")
}

fn local_bin_dir(home: &std::path::Path) -> PathBuf {
    home.join(".local").join("bin")
}

fn resolve_agent_env_for_cli(
    home: &std::path::Path,
    config: &Config,
) -> Result<HashMap<String, String>> {
    if config.agent.env.is_empty() {
        return Ok(HashMap::new());
    }
    let store = SecretStore::open(home)?;
    let mut env = HashMap::with_capacity(config.agent.env.len());
    for name in &config.agent.env {
        let value = store.get(name)?;
        env.insert(name.clone(), value.to_owned());
    }
    Ok(env)
}

/// Result of comparing the installed managed-agent version against upstream.
/// Carried as a typed enum so the CLI printer and test cases can pattern-match
/// the four states deterministically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum AgentCheckStatus {
    /// Installed and upstream agree on a non-empty version.
    UpToDate { version: String },
    /// Both versions are known and they differ — operator should re-run install.
    Stale { installed: String, latest: String },
    /// We could not derive an upstream version (shell-recipe install, missing
    /// registry kind, or upstream API error captured as a fall-through).
    Unknown { reason: String },
    /// No successful installer row for this step yet.
    NotInstalled,
}

/// Sources of "latest version" used by `acps agent check`. Trait-based so unit
/// tests can substitute a deterministic mock; the production runtime injects
/// `LiveLatestVersionResolver` which actually hits npm and GitHub.
pub(super) trait LatestVersionResolver {
    fn npm(&self, package: &str) -> Result<String>;
    fn github(&self, repo: &str) -> Result<String>;
}

struct LiveLatestVersionResolver;

impl LatestVersionResolver for LiveLatestVersionResolver {
    fn npm(&self, package: &str) -> Result<String> {
        crate::runtime::install::npm_registry::latest_version(package)
    }
    fn github(&self, repo: &str) -> Result<String> {
        crate::runtime::install::github_release::latest_release_tag(repo)
    }
}

/// Resolve the registry-declared upstream version for the given step. Returns
/// `Ok(Some)` when the registry entry pins this step to a known source
/// (npm package, GitHub release), `Ok(None)` when the install kind has no
/// queryable upstream (shell recipes), and `Err` when the upstream lookup
/// itself fails. Caller decides how to surface each variant in the report.
fn resolve_upstream_version_for_step(
    entry: &RegistryEntry,
    step: &str,
    resolver: &dyn LatestVersionResolver,
) -> Result<Option<String>> {
    let install = match step {
        STEP_HARNESS | STEP_INSTALL => entry.harness.as_ref().map(|h| &h.install),
        STEP_ADAPTER => entry.adapter.as_ref().map(|a| &a.install),
        _ => None,
    };
    let Some(install) = install else {
        return Ok(None);
    };
    if let Some(npm) = &install.npm {
        return resolver.npm(&npm.package).map(Some);
    }
    if let Some(_github) = &install.github {
        let github_url = if step == STEP_ADAPTER {
            entry
                .adapter
                .as_ref()
                .and_then(|a| a.github.as_deref())
                .or(entry.github.as_deref())
        } else {
            entry.github.as_deref()
        };
        let Some(github_url) = github_url else {
            return Ok(None);
        };
        let repo = crate::runtime::install::agent_registry::github_repo_from_url(
            &entry.id, "github", github_url,
        )?;
        return resolver.github(&repo).map(Some);
    }
    // Shell-recipe installs have no machine-checkable upstream; let the caller
    // render this as "unknown, manual check required".
    Ok(None)
}

/// Compare an installed version against an optional upstream version. Pure
/// function so the comparison rules can be unit-tested without touching the
/// network or the registry.
fn compare_versions(installed: &str, latest: Option<&str>) -> AgentCheckStatus {
    match latest {
        None => AgentCheckStatus::Unknown {
            reason: format!(
                "no machine-checkable upstream for this step (installed `{installed}`); run `acps installer history` for the full row"
            ),
        },
        Some(latest) => {
            if normalize_version(installed) == normalize_version(latest) {
                AgentCheckStatus::UpToDate {
                    version: installed.to_owned(),
                }
            } else {
                AgentCheckStatus::Stale {
                    installed: installed.to_owned(),
                    latest: latest.to_owned(),
                }
            }
        }
    }
}

/// Strip a leading `v` so a `v0.11.1` installer row compares equal to a
/// `0.11.1` npm registry response (and vice versa). Other normalization (e.g.
/// pre-release tags) is deliberately not applied — we want to flag any other
/// drift as stale.
fn normalize_version(value: &str) -> &str {
    value
        .trim()
        .strip_prefix('v')
        .unwrap_or_else(|| value.trim())
}

fn run_agent_check() -> Result<()> {
    let home = home_dir()?;
    let config = Config::load_from_default_path()?;
    let registry = RegistryCatalog::load_with_override(&operator_registry_override(&home))?;
    let entry =
        registry
            .lookup(&config.agent.id)
            .ok_or_else(|| StackError::AgentRegistryMissing {
                id: config.agent.id.clone(),
            })?;
    let state_path = default_state_path(&home);
    let state_dir = parent_dir(&state_path)?;
    create_dir_owner_only(state_dir)?;
    pre_create_owner_only(&state_path)?;
    let store = StateStore::open(&state_path)?;
    store.migrate()?;
    set_owner_only_file(&state_path)?;
    let installed_rows = store.latest_successful_installer_runs_for_agent(&config.agent.id)?;

    let resolver = LiveLatestVersionResolver;
    let report = build_agent_check_report(entry, &installed_rows, &resolver);
    let has_failure = agent_check_has_failure(&report);

    println!("agent check: {}", config.agent.id);
    if report.is_empty() {
        println!(
            "no installer runs recorded for `{}`; run `acps agent install` first",
            config.agent.id
        );
        return Ok(());
    }
    for (step, status) in &report {
        match status {
            AgentCheckStatus::UpToDate { version } => {
                println!("{step}: up-to-date ({version})");
            }
            AgentCheckStatus::Stale { installed, latest } => {
                println!("{step}: stale (installed {installed}, latest {latest})");
            }
            AgentCheckStatus::Unknown { reason } => {
                println!("{step}: unknown ({reason})");
            }
            AgentCheckStatus::NotInstalled => {
                println!("{step}: not installed");
            }
        }
    }
    if has_failure {
        return Err(StackError::AgentCheckStale);
    }
    Ok(())
}

/// Walk the registry's expected managed steps for an agent and pair each one
/// with a freshness verdict. Missing successful rows are reported explicitly
/// so partial adapter installs cannot look healthy.
fn build_agent_check_report(
    entry: &RegistryEntry,
    installed_rows: &[crate::state::InstallerRun],
    resolver: &dyn LatestVersionResolver,
) -> Vec<(String, AgentCheckStatus)> {
    let expected_steps = expected_agent_check_steps(entry);
    let mut out = Vec::with_capacity(expected_steps.len());
    for step in expected_steps {
        let Some(row) = installed_rows.iter().find(|row| row.step == *step) else {
            out.push(((*step).to_owned(), AgentCheckStatus::NotInstalled));
            continue;
        };
        let latest = match resolve_upstream_version_for_step(entry, step, resolver) {
            Ok(value) => value,
            Err(err) => {
                out.push((
                    (*step).to_owned(),
                    AgentCheckStatus::Unknown {
                        reason: format!("upstream lookup failed: {err}"),
                    },
                ));
                continue;
            }
        };
        let status = match row.version.as_deref() {
            Some(installed) => compare_versions(installed, latest.as_deref()),
            None => AgentCheckStatus::Unknown {
                reason: if latest.is_some() {
                    "installed version was not recorded; run `acps installer history` for the full row"
                        .to_owned()
                } else {
                    "no machine-checkable upstream for this step; run `acps installer history` for the full row"
                        .to_owned()
                },
            },
        };
        out.push(((*step).to_owned(), status));
    }
    out
}

fn expected_agent_check_steps(entry: &RegistryEntry) -> &'static [&'static str] {
    if entry.kind == RegistryKind::Adapter {
        &[STEP_HARNESS, STEP_ADAPTER]
    } else {
        &[STEP_INSTALL]
    }
}

fn agent_check_has_failure(report: &[(String, AgentCheckStatus)]) -> bool {
    report.iter().any(|(_, status)| {
        matches!(
            status,
            AgentCheckStatus::Stale { .. } | AgentCheckStatus::NotInstalled
        )
    })
}

fn run_agent_status() -> Result<()> {
    let home = home_dir()?;
    let config = Config::load_from_default_path()?;
    let registry = RegistryCatalog::load_with_override(&operator_registry_override(&home))?;
    let registry_entry = registry.lookup(&config.agent.id);
    let state_path = default_state_path(&home);
    let state_dir = parent_dir(&state_path)?;
    create_dir_owner_only(state_dir)?;
    pre_create_owner_only(&state_path)?;
    let store = StateStore::open(&state_path)?;
    store.migrate()?;
    set_owner_only_file(&state_path)?;

    println!("agent: {}", config.agent.id);
    print_agent_status_params(&config, registry_entry);
    let installed_versions = store.latest_successful_installer_runs_for_agent(&config.agent.id)?;
    print_installed_versions(&installed_versions);
    println!("command: {}", config.agent.command);

    match store.latest_agent_capabilities(&config.agent.id)? {
        Some(record) => {
            if let Ok(capabilities) = serde_json::from_str::<
                crate::runtime::agent::acp_bridge::AgentCapabilitiesDto,
            >(&record.capabilities_json)
            {
                println!("ACP version: {}", capabilities.protocol_version);
            }
            println!("latest capabilities captured: {}", record.captured_at);
            println!("capabilities_json: {}", record.capabilities_json);
        }
        None => println!("latest capabilities: none recorded yet"),
    }

    let lifecycle = store.query_agent_lifecycle(10)?;
    if lifecycle.is_empty() {
        println!("recent lifecycle: (no rows)");
    } else {
        println!("recent lifecycle:");
        for event in lifecycle {
            println!(
                "  {} {} {}",
                event.created_at, event.event_kind, event.message
            );
        }
    }
    Ok(())
}

enum AgentStatusParamState {
    Configured(&'static str, String),
    Unset(&'static str),
    Unavailable(&'static str),
}

fn print_agent_status_params(config: &Config, registry_entry: Option<&RegistryEntry>) {
    let params = agent_status_params(config, registry_entry);
    let mut unset = Vec::new();
    let mut unavailable = Vec::new();

    for param in params {
        match param {
            AgentStatusParamState::Configured(name, value) => println!("{name}: {value}"),
            AgentStatusParamState::Unset(name) => unset.push(name),
            AgentStatusParamState::Unavailable(name) => unavailable.push(name),
        }
    }

    if !unset.is_empty() {
        println!("{} unset", human_list(&unset));
    }
    if !unavailable.is_empty() {
        println!("{} unavailable", human_list(&unavailable));
    }
}

fn agent_status_params(
    config: &Config,
    registry_entry: Option<&RegistryEntry>,
) -> Vec<AgentStatusParamState> {
    let provider = config
        .agent
        .provider
        .as_ref()
        .map(|provider| provider.id.clone());
    let model = config.agent.model.clone().or_else(|| {
        config
            .agent
            .provider
            .as_ref()
            .and_then(|provider| provider.model.clone())
    });
    let mode = config.agent.mode.clone();

    vec![
        agent_status_param(
            "provider",
            provider,
            registry_entry.is_some_and(|entry| entry.set_provider),
        ),
        agent_status_param(
            "model",
            model,
            registry_entry.is_some_and(|entry| entry.set_model),
        ),
        agent_status_param(
            "mode",
            mode,
            registry_entry.is_some_and(|entry| entry.set_mode),
        ),
    ]
}

fn agent_status_param(
    name: &'static str,
    configured: Option<String>,
    supported: bool,
) -> AgentStatusParamState {
    if let Some(value) = configured {
        return AgentStatusParamState::Configured(name, value);
    }
    if supported {
        AgentStatusParamState::Unset(name)
    } else {
        AgentStatusParamState::Unavailable(name)
    }
}

/// Render one line per `installer_runs.step` recorded for this agent, showing
/// the step name and the resolved version when known. Steps that ran without
/// a recorded version (shell installs) print "version unknown"
/// so the operator can tell the difference between "no install row at all"
/// and "install ran but produced no version".
fn print_installed_versions(rows: &[crate::state::InstallerRun]) {
    if rows.is_empty() {
        return;
    }
    for row in rows {
        let label = installed_version_label(&row.step);
        match row.version.as_deref() {
            Some(value) if !value.is_empty() => {
                println!("{label}: {value}");
            }
            _ => println!("{label}: version unknown"),
        }
    }
}

fn installed_version_label(step: &str) -> String {
    match step {
        STEP_INSTALL => "agent version".to_owned(),
        STEP_HARNESS => "harness version".to_owned(),
        STEP_ADAPTER => "adapter version".to_owned(),
        other => format!("{other} version"),
    }
}

fn human_list(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [single] => (*single).to_owned(),
        [first, second] => format!("{first} and {second}"),
        _ => {
            let (last, rest) = items.split_last().expect("non-empty list");
            format!("{}, and {last}", rest.join(", "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn opencode_cloudflare_gateway_defaults_to_token_ref() {
        assert_eq!(
            default_api_key_ref_for_agent_provider("opencode", "cloudflare-ai-gateway"),
            Some("CLOUDFLARE_API_TOKEN".to_owned())
        );
        assert_eq!(
            default_api_key_ref_for_agent_provider("pi", "cloudflare-ai-gateway"),
            Some("CLOUDFLARE_API_KEY".to_owned())
        );
    }

    struct MockResolver {
        npm: std::collections::HashMap<String, String>,
        github: std::collections::HashMap<String, String>,
    }

    impl MockResolver {
        fn new() -> Self {
            Self {
                npm: std::collections::HashMap::new(),
                github: std::collections::HashMap::new(),
            }
        }
        fn with_npm(mut self, package: &str, version: &str) -> Self {
            self.npm.insert(package.to_owned(), version.to_owned());
            self
        }
        #[allow(dead_code)]
        fn with_github(mut self, repo: &str, version: &str) -> Self {
            self.github.insert(repo.to_owned(), version.to_owned());
            self
        }
    }

    impl LatestVersionResolver for MockResolver {
        fn npm(&self, package: &str) -> Result<String> {
            self.npm
                .get(package)
                .cloned()
                .ok_or_else(|| StackError::NpmRegistryEmptyVersion {
                    package: package.to_owned(),
                })
        }
        fn github(&self, repo: &str) -> Result<String> {
            self.github
                .get(repo)
                .cloned()
                .ok_or_else(|| StackError::AgentRegistryMissing {
                    id: repo.to_owned(),
                })
        }
    }

    fn installer_row(step: &str, version: Option<&str>) -> crate::state::InstallerRun {
        crate::state::InstallerRun {
            id: format!("ins_{step}"),
            agent_id: Some("test-agent".to_owned()),
            started_at: "2026-05-22T00:00:00.000000000Z".to_owned(),
            finished_at: Some("2026-05-22T00:00:01.000000000Z".to_owned()),
            status: "ran".to_owned(),
            stdout: String::new(),
            stderr: String::new(),
            exit_status: Some(0),
            step: step.to_owned(),
            version: version.map(str::to_owned),
            log_dir: None,
            apply_run_id: None,
        }
    }

    #[test]
    fn compare_versions_normalizes_leading_v() {
        let status = compare_versions("v1.2.3", Some("1.2.3"));
        assert!(matches!(
            status,
            AgentCheckStatus::UpToDate { ref version } if version == "v1.2.3"
        ));
    }

    #[test]
    fn compare_versions_flags_drift() {
        let status = compare_versions("1.0.0", Some("2.0.0"));
        assert!(matches!(
            status,
            AgentCheckStatus::Stale {
                ref installed,
                ref latest,
            } if installed == "1.0.0" && latest == "2.0.0"
        ));
    }

    #[test]
    fn compare_versions_returns_unknown_when_upstream_missing() {
        let status = compare_versions("1.0.0", None);
        assert!(matches!(status, AgentCheckStatus::Unknown { .. }));
    }

    fn embedded_entry(id: &str) -> RegistryEntry {
        crate::runtime::install::agent_registry::RegistryCatalog::load_embedded()
            .expect("registry embeds")
            .lookup(id)
            .expect("entry exists")
            .clone()
    }

    #[test]
    fn build_agent_check_report_returns_stale_for_codex_adapter() {
        // Codex declares npm for both harness (`@openai/codex`) and adapter
        // (`@zed-industries/codex-acp`). The install-path resolver prefers npm
        // when both are present, so the mock provides both.
        let entry = embedded_entry("codex");
        let resolver = MockResolver::new()
            .with_npm("@openai/codex", "rust-v999.0.0")
            .with_npm("@zed-industries/codex-acp", "9.9.9");
        let rows = vec![
            installer_row("harness", Some("rust-v0.50.0")),
            installer_row("adapter", Some("0.1.0")),
        ];
        let report = build_agent_check_report(&entry, &rows, &resolver);
        assert_eq!(report.len(), 2);
        // harness: npm version drift -> stale
        assert!(matches!(
            &report[0],
            (step, AgentCheckStatus::Stale { .. }) if step == "harness"
        ));
        // adapter: npm version drift -> stale
        assert!(matches!(
            &report[1],
            (step, AgentCheckStatus::Stale { .. }) if step == "adapter"
        ));
        assert!(agent_check_has_failure(&report));
    }

    #[test]
    fn build_agent_check_report_returns_up_to_date_when_versions_match() {
        let entry = embedded_entry("codex");
        let resolver = MockResolver::new()
            .with_npm("@openai/codex", "rust-v0.50.0")
            .with_npm("@zed-industries/codex-acp", "0.1.0");
        let rows = vec![
            installer_row("harness", Some("rust-v0.50.0")),
            installer_row("adapter", Some("0.1.0")),
        ];
        let report = build_agent_check_report(&entry, &rows, &resolver);
        assert!(matches!(
            &report[0],
            (step, AgentCheckStatus::UpToDate { .. }) if step == "harness"
        ));
        assert!(matches!(
            &report[1],
            (step, AgentCheckStatus::UpToDate { .. }) if step == "adapter"
        ));
    }

    #[test]
    fn build_agent_check_report_marks_resolver_errors_as_unknown() {
        let entry = embedded_entry("codex");
        // No mock entries -> resolver errors -> report should mark each step
        // as Unknown rather than crash the whole report.
        let resolver = MockResolver::new();
        let rows = vec![installer_row("adapter", Some("0.1.0"))];
        let report = build_agent_check_report(&entry, &rows, &resolver);
        let adapter = report
            .iter()
            .find(|(step, _)| step == "adapter")
            .expect("adapter report");
        assert!(matches!(adapter, (_, AgentCheckStatus::Unknown { .. })));
    }

    #[test]
    fn build_agent_check_report_returns_unknown_for_shell_native_without_version() {
        let entry = embedded_entry("cursor");
        let resolver = MockResolver::new();
        let rows = vec![installer_row("install", None)];
        let report = build_agent_check_report(&entry, &rows, &resolver);
        assert_eq!(report.len(), 1);
        assert!(matches!(
            &report[0],
            (step, AgentCheckStatus::Unknown { .. }) if step == "install"
        ));
        assert!(!agent_check_has_failure(&report));
    }

    #[test]
    fn build_agent_check_report_marks_missing_adapter_not_installed() {
        let entry = embedded_entry("amp");
        let resolver = MockResolver::new();
        let rows = vec![installer_row("harness", None)];
        let report = build_agent_check_report(&entry, &rows, &resolver);
        assert_eq!(report.len(), 2);
        assert!(matches!(
            &report[0],
            (step, AgentCheckStatus::Unknown { .. }) if step == "harness"
        ));
        assert!(matches!(
            &report[1],
            (step, AgentCheckStatus::NotInstalled) if step == "adapter"
        ));
        assert!(agent_check_has_failure(&report));
    }

    #[test]
    fn build_agent_check_report_marks_missing_native_install_not_installed() {
        let entry = embedded_entry("cursor");
        let resolver = MockResolver::new();
        let report = build_agent_check_report(&entry, &[], &resolver);
        assert_eq!(report.len(), 1);
        assert!(matches!(
            &report[0],
            (step, AgentCheckStatus::NotInstalled) if step == "install"
        ));
        assert!(agent_check_has_failure(&report));
    }

    #[test]
    fn build_agent_check_report_unknown_when_queryable_version_was_not_recorded() {
        let entry = embedded_entry("codex");
        let resolver = MockResolver::new()
            .with_npm("@openai/codex", "rust-v0.50.0")
            .with_npm("@zed-industries/codex-acp", "0.1.0");
        let rows = vec![
            installer_row("harness", Some("rust-v0.50.0")),
            installer_row("adapter", None),
        ];
        let report = build_agent_check_report(&entry, &rows, &resolver);
        assert!(matches!(
            &report[1],
            (step, AgentCheckStatus::Unknown { reason }) if step == "adapter"
                && reason.contains("installed version was not recorded")
        ));
        assert!(!agent_check_has_failure(&report));
    }

    #[test]
    fn verify_testflight_expect_fs_succeeds_for_non_empty_file_under_workspace() {
        let workspace = TempDir::new().expect("tempdir");
        let target = workspace.path().join("marker.txt");
        std::fs::write(&target, b"ok\n").expect("write");
        let outcome =
            verify_testflight_expect_fs(workspace.path(), "marker.txt").expect("verify ok");
        assert_eq!(outcome.path, target);
        assert_eq!(outcome.bytes, 3);
    }

    #[test]
    fn verify_testflight_expect_fs_fails_when_file_missing() {
        let workspace = TempDir::new().expect("tempdir");
        let err = verify_testflight_expect_fs(workspace.path(), "missing.txt")
            .expect_err("missing file must fail");
        match err {
            StackError::AgentTestFailed { stage, reason } => {
                assert_eq!(stage, "fs smoke");
                assert!(reason.contains("stat failed"), "reason: {reason}");
            }
            other => panic!("expected AgentTestFailed, got {other:?}"),
        }
    }

    #[test]
    fn verify_testflight_expect_fs_fails_on_empty_file() {
        let workspace = TempDir::new().expect("tempdir");
        let target = workspace.path().join("empty.txt");
        std::fs::write(&target, b"").expect("write");
        let err = verify_testflight_expect_fs(workspace.path(), "empty.txt")
            .expect_err("empty file must fail");
        assert!(matches!(err, StackError::AgentTestFailed { .. }));
    }

    #[test]
    fn verify_testflight_expect_fs_rejects_absolute_path_argument() {
        let workspace = TempDir::new().expect("tempdir");
        let err = verify_testflight_expect_fs(workspace.path(), "/etc/passwd")
            .expect_err("absolute path must be rejected");
        match err {
            StackError::AgentTestFailed { reason, .. } => {
                assert!(reason.contains("workspace-relative"), "reason: {reason}");
            }
            other => panic!("expected AgentTestFailed, got {other:?}"),
        }
    }

    #[test]
    fn verify_testflight_expect_fs_rejects_parent_traversal() {
        let workspace = TempDir::new().expect("tempdir");
        let err = verify_testflight_expect_fs(workspace.path(), "sub/../escape.txt")
            .expect_err("`..` segment must be rejected");
        assert!(matches!(err, StackError::AgentTestFailed { .. }));
    }

    #[test]
    fn prepare_testflight_expect_fs_removes_stale_regular_file() {
        let workspace = TempDir::new().expect("tempdir");
        let target = workspace.path().join("marker.txt");
        std::fs::write(&target, b"old\n").expect("write");
        prepare_testflight_expect_fs(workspace.path(), "marker.txt").expect("prepare ok");
        assert!(!target.exists(), "stale marker should be removed");
    }

    #[cfg(unix)]
    #[test]
    fn prepare_testflight_expect_fs_rejects_preexisting_symlink() {
        let workspace = TempDir::new().expect("tempdir");
        let outside = TempDir::new().expect("outside tempdir");
        let outside_file = outside.path().join("marker.txt");
        std::fs::write(&outside_file, b"outside\n").expect("write outside");
        std::os::unix::fs::symlink(&outside_file, workspace.path().join("marker.txt"))
            .expect("symlink");

        let err = prepare_testflight_expect_fs(workspace.path(), "marker.txt")
            .expect_err("symlink marker must fail");
        match err {
            StackError::AgentTestFailed { reason, .. } => {
                assert!(reason.contains("symlink"), "reason: {reason}");
            }
            other => panic!("expected AgentTestFailed, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn verify_testflight_expect_fs_rejects_parent_symlink_escape() {
        let workspace = TempDir::new().expect("tempdir");
        let outside = TempDir::new().expect("outside tempdir");
        std::fs::write(outside.path().join("marker.txt"), b"outside\n").expect("write outside");
        std::os::unix::fs::symlink(outside.path(), workspace.path().join("linked"))
            .expect("symlink");

        let err = verify_testflight_expect_fs(workspace.path(), "linked/marker.txt")
            .expect_err("canonical escape must fail");
        match err {
            StackError::AgentTestFailed { reason, .. } => {
                assert!(reason.contains("outside workspace"), "reason: {reason}");
            }
            other => panic!("expected AgentTestFailed, got {other:?}"),
        }
    }
}
