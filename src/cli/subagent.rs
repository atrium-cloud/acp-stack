use clap::{Args, Subcommand};

use crate::config::{
    self, AgentCustomProviderConfig, AgentProviderConfig, AgentSubagentConfig, Config,
    DEFAULT_CUSTOM_MODEL_CONTEXT, DEFAULT_CUSTOM_MODEL_OUTPUT_MAX_TOKENS,
};
use crate::error::{Result, StackError};
use crate::fs_util::{atomic_write_owner_only, home_dir};
use crate::runtime::agent::acp_bridge::AgentSessionConfigCategory;
use crate::runtime::agent::agent_headless_config::{
    OPENCODE_AGENT_ID, OPENCODE_DISABLED_SMALL_MODEL, provision_agent_headless_config,
};
use crate::runtime::agent::provider_keys::{
    agent_provider_id_for_provider_id, optional_env_refs_for_provider_id, provider_id_is_known,
    provider_id_supports_agent, required_env_refs_for_provider_id,
};
use crate::runtime::install::agent_registry::{RegistryCatalog, RegistryEntry};

use super::agent::{
    default_api_key_ref_for_agent_provider, default_custom_provider_api,
    operator_registry_override, parse_custom_provider_api, parse_custom_token_limit,
    print_agent_set_effective_notice_for, required_custom_arg, resolve_agent_model_value,
    validate_agent_session_config_value,
};

// These are OpenCode-specific free small-model shortcuts exposed by provider
// families that are stable enough to make the operator intent explicit.
const OPENCODE_FREE_OPENROUTER_MODEL: &str = "openrouter/free";
const OPENCODE_FREE_OPENCODE_MODEL: &str = "opencode/big-pickle";
const OPENROUTER_PROVIDER_ID: &str = "openrouter";
const OPENCODE_PROVIDER_ID: &str = "opencode";
const OPENCODE_GO_PROVIDER_ID: &str = "opencode-go";
const OPENROUTER_API_KEY_REF: &str = "OPENROUTER_API_KEY";
const OPENCODE_API_KEY_REF: &str = "OPENCODE_API_KEY";
const SUBAGENT_UNSUPPORTED_MESSAGE: &str = "Current agent does not support subagent configuration.";

#[derive(Debug, Subcommand)]
pub enum SubagentCommand {
    /// Print auxiliary/subagent provider and model configuration.
    Status,
    /// Set the provider id, model, and API-key ref used by auxiliary/subagent tasks.
    Set(Box<SubagentSetArgs>),
    /// Use a known free auxiliary model for the configured OpenCode provider family.
    Free(SubagentFreeArgs),
    /// Disable auxiliary/subagent model calls where the harness supports it.
    Disable,
}

#[derive(Debug, Args)]
pub struct SubagentSetArgs {
    /// Configure a provider/model outside the embedded provider mapping.
    #[arg(long)]
    custom_provider: bool,
    /// Provider id, such as opencode-go, openai, or anthropic.
    #[arg(long)]
    provider: String,
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
    model: String,
    /// Display name for a custom model.
    #[arg(long = "model-name")]
    model_name: Option<String>,
    /// Context window in tokens for a custom model.
    #[arg(long)]
    context: Option<String>,
    /// Maximum output tokens for a custom model.
    #[arg(long = "output-max-tokens")]
    output_max_tokens: Option<String>,
    /// Secret ref to inject for this provider. Defaults from provider metadata.
    #[arg(long)]
    api_key_ref: Option<String>,
}

#[derive(Debug, Args)]
pub struct SubagentFreeArgs {
    /// Free provider family to use: openrouter or opencode. Defaults from current config.
    #[arg(long)]
    provider: Option<String>,
    /// Secret ref to inject for the selected free provider. Defaults from provider metadata.
    #[arg(long)]
    api_key_ref: Option<String>,
}

pub(super) fn run_subagent_command(command: SubagentCommand) -> Result<()> {
    match command {
        SubagentCommand::Status => run_subagent_status(),
        SubagentCommand::Set(args) => run_subagent_set(*args),
        SubagentCommand::Free(args) => run_subagent_free(args),
        SubagentCommand::Disable => run_subagent_disable(),
    }
}

fn run_subagent_status() -> Result<()> {
    let home = home_dir()?;
    let config = Config::load_from_path(&config::default_config_path()?)?;
    let registry = RegistryCatalog::load_with_override(&operator_registry_override(&home))?;
    let entry =
        registry
            .lookup(&config.agent.id)
            .ok_or_else(|| StackError::AgentRegistryMissing {
                id: config.agent.id.clone(),
            })?;
    ensure_subagent_supported(entry)?;

    print_subagent_header(&config, entry);
    if config
        .agent
        .subagent
        .as_ref()
        .is_some_and(|subagent| subagent.disabled)
    {
        println!("status: disabled");
        println!("model: {OPENCODE_DISABLED_SMALL_MODEL}");
        return Ok(());
    }
    let Some(provider) = config
        .agent
        .subagent
        .as_ref()
        .and_then(|subagent| subagent.provider.as_ref())
    else {
        if let Some(provider) = config.agent.provider.as_ref()
            && provider
                .model
                .as_deref()
                .is_some_and(|model| !model.trim().is_empty())
        {
            println!("status: inherited");
            println!("provider: {}", provider.id);
            println!("model: {}", provider.model.as_deref().unwrap_or("unset"));
            println!(
                "api_key_ref: {}",
                provider.api_key_ref.as_deref().unwrap_or("unset")
            );
            return Ok(());
        }
        println!("provider: unset");
        println!("model: unset");
        return Ok(());
    };
    println!("provider: {}", provider.id);
    println!("model: {}", provider.model.as_deref().unwrap_or("unset"));
    println!(
        "api_key_ref: {}",
        provider.api_key_ref.as_deref().unwrap_or("unset")
    );
    Ok(())
}

fn run_subagent_disable() -> Result<()> {
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
    ensure_subagent_supported(entry)?;

    config.agent.subagent = Some(AgentSubagentConfig {
        disabled: true,
        provider: None,
    });
    let canonical = config.to_canonical_toml()?;
    let config = config::load_config_from_str(&canonical)?;
    let provisioned = provision_agent_headless_config(&config, &home)?;
    atomic_write_owner_only(&config_path, canonical.as_bytes())?;

    print_subagent_header(&config, entry);
    println!("status: disabled");
    println!("model: {OPENCODE_DISABLED_SMALL_MODEL}");
    for item in provisioned {
        println!("{}: {}", item.label, item.path.display());
    }
    print_agent_set_effective_notice_for(Some(&config.agent.id));
    Ok(())
}

fn run_subagent_free(args: SubagentFreeArgs) -> Result<()> {
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
    ensure_subagent_supported(entry)?;

    let free_model = resolve_free_model(&config, args.provider.as_deref())?;
    let default_api_key_ref =
        default_api_key_ref_for_agent_provider(&config.agent.id, free_model.provider_id);
    let api_key_ref = args.api_key_ref.or(default_api_key_ref).ok_or_else(|| {
        StackError::AgentConfigProvision {
            path: config_path.clone(),
            reason: format!(
                "provider `{}` has no default API-key env var; pass --api-key-ref",
                free_model.provider_id
            ),
        }
    })?;
    let required_env_refs = required_env_refs_for_provider_id(free_model.provider_id, &api_key_ref);
    for env_ref in &required_env_refs {
        if !config.agent.env.iter().any(|name| name == env_ref) {
            config.agent.env.push(env_ref.clone());
        }
    }
    config.agent.subagent = Some(AgentSubagentConfig {
        disabled: false,
        provider: Some(AgentProviderConfig {
            id: free_model.provider_id.to_owned(),
            model: Some(free_model.model.to_owned()),
            api_key_ref: Some(api_key_ref),
            custom: None,
        }),
    });
    let canonical = config.to_canonical_toml()?;
    let config = config::load_config_from_str(&canonical)?;
    let provisioned = provision_agent_headless_config(&config, &home)?;
    atomic_write_owner_only(&config_path, canonical.as_bytes())?;

    print_subagent_header(&config, entry);
    println!("provider: {}", free_model.provider_id);
    println!("model: {}", free_model.model);
    for item in provisioned {
        println!("{}: {}", item.label, item.path.display());
    }
    print_agent_set_effective_notice_for(Some(&config.agent.id));
    Ok(())
}

fn run_subagent_set(args: SubagentSetArgs) -> Result<()> {
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
    ensure_subagent_supported(entry)?;

    if args.custom_provider {
        configure_custom_subagent(&mut config, entry, args)?;
    } else {
        reject_custom_provider_args(&args)?;
        configure_mapped_subagent(&home, &config_path, &mut config, args)?;
    }

    let canonical = config.to_canonical_toml()?;
    let config = config::load_config_from_str(&canonical)?;
    if let Some(model) = config
        .agent
        .subagent
        .as_ref()
        .and_then(|subagent| subagent.provider.as_ref())
        .filter(|provider| provider.custom.is_none())
        .and_then(|provider| provider.model.as_deref())
    {
        validate_agent_session_config_value(
            &home,
            &config,
            AgentSessionConfigCategory::Model,
            model,
        )?;
    }
    let provisioned = provision_agent_headless_config(&config, &home)?;
    atomic_write_owner_only(&config_path, canonical.as_bytes())?;

    print_subagent_header(&config, entry);
    let provider = config
        .agent
        .subagent
        .as_ref()
        .and_then(|subagent| subagent.provider.as_ref())
        .expect("subagent provider set");
    println!("provider: {}", provider.id);
    println!("model: {}", provider.model.as_deref().unwrap_or(""));
    if let Some(api_key_ref) = provider.api_key_ref.as_deref() {
        println!("api_key_ref: {api_key_ref}");
    }
    if provider.custom.is_none() {
        let required_env_refs = required_env_refs_for_provider_id(
            &provider.id,
            provider.api_key_ref.as_deref().unwrap_or(""),
        );
        if required_env_refs.len() > 1 {
            println!("required_env_refs: {}", required_env_refs.join(", "));
        }
        let optional = optional_env_refs_for_provider_id(&provider.id);
        if !optional.is_empty() {
            println!("optional_env_refs: {}", optional.join(", "));
        }
    }
    for item in provisioned {
        println!("{}: {}", item.label, item.path.display());
    }
    print_agent_set_effective_notice_for(Some(&config.agent.id));
    Ok(())
}

// `acps subagent` only gates OpenCode's `small_model` today. Other harnesses
// (pi, goose, amp, cursor, codex) have their own in-harness subagent/role
// mechanisms that are out of scope until they're tested end-to-end. Keep this
// guard tied to the built-in OpenCode id so a registry override cannot enable
// an untested code path.
fn ensure_subagent_supported(entry: &RegistryEntry) -> Result<()> {
    if entry.id == OPENCODE_AGENT_ID {
        return Ok(());
    }
    Err(StackError::InvalidParam {
        field: "subagent",
        reason: SUBAGENT_UNSUPPORTED_MESSAGE.to_owned(),
    })
}

fn configure_mapped_subagent(
    home: &std::path::Path,
    config_path: &std::path::Path,
    config: &mut Config,
    args: SubagentSetArgs,
) -> Result<()> {
    if !provider_id_is_known(&args.provider) {
        return Err(StackError::InvalidParam {
            field: "provider",
            reason: format!(
                "provider `{}` is not listed in provider/env mapping",
                args.provider
            ),
        });
    }
    if !provider_id_supports_agent(&args.provider, &config.agent.id) {
        return Err(StackError::InvalidParam {
            field: "provider",
            reason: format!(
                "provider `{}` is not supported for agent `{}`",
                args.provider, config.agent.id
            ),
        });
    }
    let default_api_key_ref =
        default_api_key_ref_for_agent_provider(&config.agent.id, &args.provider);
    let api_key_ref = args.api_key_ref.or(default_api_key_ref).ok_or_else(|| {
        StackError::AgentConfigProvision {
            path: config_path.to_path_buf(),
            reason: format!(
                "provider `{}` has no default API-key env var; pass --api-key-ref",
                args.provider
            ),
        }
    })?;
    let required_env_refs = required_env_refs_for_provider_id(&args.provider, &api_key_ref);
    for env_ref in &required_env_refs {
        if !config.agent.env.iter().any(|name| name == env_ref) {
            config.agent.env.push(env_ref.clone());
        }
    }
    let Some(agent_provider_id) =
        agent_provider_id_for_provider_id(&config.agent.id, &args.provider)
    else {
        return Err(StackError::InvalidParam {
            field: "provider",
            reason: format!(
                "provider `{}` is not supported for agent `{}`",
                args.provider, config.agent.id
            ),
        });
    };
    let model = resolve_agent_model_value(home, config, Some(agent_provider_id), &args.model)?;
    config.agent.subagent = Some(AgentSubagentConfig {
        disabled: false,
        provider: Some(AgentProviderConfig {
            id: args.provider,
            model: Some(model),
            api_key_ref: Some(api_key_ref),
            custom: None,
        }),
    });
    Ok(())
}

fn configure_custom_subagent(
    config: &mut Config,
    entry: &RegistryEntry,
    args: SubagentSetArgs,
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
    let model_name = args.model_name.unwrap_or_else(|| args.model.clone());
    let api = parse_custom_provider_api(
        args.provider_api.as_deref(),
        default_custom_provider_api(&config.agent.id),
    )?;
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
    config.agent.subagent = Some(AgentSubagentConfig {
        disabled: false,
        provider: Some(AgentProviderConfig {
            id: args.provider,
            model: Some(args.model),
            api_key_ref: Some(api_key_ref),
            custom: Some(AgentCustomProviderConfig {
                name: provider_name,
                base_url,
                api,
                model_name: Some(model_name),
                context,
                output_max_tokens,
            }),
        }),
    });
    Ok(())
}

fn reject_custom_provider_args(args: &SubagentSetArgs) -> Result<()> {
    if args.provider_name.is_some()
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

fn print_subagent_header(config: &Config, entry: &RegistryEntry) {
    println!("agent: {}", config.agent.id);
    println!(
        "subagent: {}",
        entry.subagent_alias.as_deref().unwrap_or("subagent")
    );
}

#[derive(Debug, Clone, Copy)]
struct FreeSubagentModel {
    provider_id: &'static str,
    model: &'static str,
}

fn resolve_free_model(
    config: &Config,
    requested_provider: Option<&str>,
) -> Result<FreeSubagentModel> {
    match requested_provider {
        Some(OPENROUTER_PROVIDER_ID) => {
            return Ok(FreeSubagentModel {
                provider_id: OPENROUTER_PROVIDER_ID,
                model: OPENCODE_FREE_OPENROUTER_MODEL,
            });
        }
        Some(OPENCODE_PROVIDER_ID | OPENCODE_GO_PROVIDER_ID) => {
            return Ok(FreeSubagentModel {
                provider_id: OPENCODE_PROVIDER_ID,
                model: OPENCODE_FREE_OPENCODE_MODEL,
            });
        }
        Some(other) => {
            return Err(StackError::InvalidParam {
                field: "provider",
                reason: format!(
                    "free subagent model provider must be `{OPENROUTER_PROVIDER_ID}` or `{OPENCODE_PROVIDER_ID}`, got `{other}`"
                ),
            });
        }
        None => {}
    }

    let configured_provider = config.agent.provider.as_ref();
    if configured_provider.is_some_and(|provider| provider.id == OPENROUTER_PROVIDER_ID) {
        return Ok(FreeSubagentModel {
            provider_id: OPENROUTER_PROVIDER_ID,
            model: OPENCODE_FREE_OPENROUTER_MODEL,
        });
    }
    if configured_provider.is_some_and(|provider| {
        provider.id == OPENCODE_PROVIDER_ID || provider.id == OPENCODE_GO_PROVIDER_ID
    }) {
        return Ok(FreeSubagentModel {
            provider_id: OPENCODE_PROVIDER_ID,
            model: OPENCODE_FREE_OPENCODE_MODEL,
        });
    }
    if configured_provider
        .is_some_and(|provider| provider.api_key_ref.as_deref() == Some(OPENROUTER_API_KEY_REF))
        || config
            .agent
            .env
            .iter()
            .any(|name| name == OPENROUTER_API_KEY_REF)
    {
        return Ok(FreeSubagentModel {
            provider_id: OPENROUTER_PROVIDER_ID,
            model: OPENCODE_FREE_OPENROUTER_MODEL,
        });
    }
    if configured_provider
        .is_some_and(|provider| provider.api_key_ref.as_deref() == Some(OPENCODE_API_KEY_REF))
        || config
            .agent
            .env
            .iter()
            .any(|name| name == OPENCODE_API_KEY_REF)
    {
        return Ok(FreeSubagentModel {
            provider_id: OPENCODE_PROVIDER_ID,
            model: OPENCODE_FREE_OPENCODE_MODEL,
        });
    }

    Err(StackError::InvalidParam {
        field: "provider",
        reason: format!(
            "could not infer a free subagent model provider; pass --provider {OPENROUTER_PROVIDER_ID} or --provider {OPENCODE_PROVIDER_ID}"
        ),
    })
}
