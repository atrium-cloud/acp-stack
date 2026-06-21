use std::io::IsTerminal;
use std::path::Path;
use std::time::Duration;

use clap::{Args, Subcommand};

use crate::config::{
    self, AgentConfig, AgentCustomProviderConfig, AgentProviderConfig, ArrayTargetConfig, Config,
    DEFAULT_CUSTOM_MODEL_CONTEXT, DEFAULT_CUSTOM_MODEL_OUTPUT_MAX_TOKENS, LocalSessionAuth,
};
use crate::error::{Result, StackError};
use crate::fs_util::{atomic_write_owner_only, home_dir};
use crate::runtime::agent::acp_bridge::AgentSessionConfigCategory;
use crate::runtime::agent::agent_headless_config::provision_agent_headless_config;
use crate::runtime::agent::provider_keys::{
    agent_provider_id_for_provider_id, env_refs_for_agent_id, provider_id_is_known,
    provider_id_supports_agent, provider_uses_agent_native_auth,
    required_env_refs_for_agent_provider_id,
};
use crate::runtime::agent::switch::adapter_from_registry_entry;
use crate::runtime::install::agent_registry::{RegistryCatalog, RegistryEntry, RegistryKind};

use super::agent::operator_registry_override;
use super::agent::{
    default_api_key_ref_for_agent_provider, default_custom_provider_api, parse_custom_provider_api,
    parse_custom_token_limit, required_custom_arg, resolve_agent_model_value,
    validate_agent_session_config_value, validate_custom_provider_api_for_agent,
};
use super::core::{
    CliMethod, OutputFormat, SessionAccess, daemon_base_url, daemon_request, encode_path_segment,
    local_daemon_request, print_json, resolve_admin_key, resolve_session_access,
};

const ARRAY_STATUS_DAEMON_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// The four per-target daemon mutation verbs. Typed so the route segment and
/// the Array-off policy gate are keyed off the same value and cannot drift via
/// a stray string literal.
#[derive(Clone, Copy)]
enum ArrayDaemonAction {
    Install,
    Start,
    Stop,
    Restart,
}

impl ArrayDaemonAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
        }
    }

    /// Whether this action starts (or keeps) an agent process and therefore
    /// must be gated to the primary target while Array mode is off. `install`
    /// and `stop` are unrestricted: install is idempotent and stop on a
    /// non-running target is a daemon no-op.
    fn gated_to_primary_when_array_off(self) -> bool {
        matches!(self, Self::Start | Self::Restart)
    }
}

#[derive(Debug, Subcommand)]
pub enum ArrayCommand {
    /// Show Array config and local delegation readiness.
    Status,
    /// Enable Array mode.
    On,
    /// Disable Array mode without deleting configured targets.
    Off,
    /// Add a configured target from the agent registry.
    Add(ArrayAddArgs),
    /// Configure provider, model, mode, or API-key ref for a target.
    Set(Box<ArraySetArgs>),
    /// Install one target or every configured target.
    Install(ArrayDaemonArgs),
    /// Start one target or every configured target.
    Start(ArrayDaemonArgs),
    /// Stop one target or every configured target.
    Stop(ArrayDaemonArgs),
    /// Restart one target or every configured target.
    Restart(ArrayDaemonArgs),
}

#[derive(Debug, Args)]
pub struct ArrayAddArgs {
    /// Agent registry id, for example codex, opencode, claude-code, or goose.
    agent: String,
}

#[derive(Debug, Args)]
pub struct ArraySetArgs {
    /// Target id to update.
    #[arg(long)]
    target: String,
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
    /// API family for a custom provider: chat-completions, responses, or anthropic-messages.
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

#[derive(Debug, Args)]
pub struct ArrayDaemonArgs {
    /// Target id. Omit to apply to every configured target.
    #[arg(long)]
    target: Option<String>,
    /// Admin API key. Required when stdin is not a terminal.
    #[arg(long = "admin-key")]
    admin_key: Option<String>,
}

pub(super) fn run_array_command(command: ArrayCommand, output: OutputFormat) -> Result<()> {
    match command {
        ArrayCommand::Status => run_array_status(output),
        ArrayCommand::On => run_array_toggle(true, output),
        ArrayCommand::Off => run_array_toggle(false, output),
        ArrayCommand::Add(args) => run_array_add(args, output),
        ArrayCommand::Set(args) => run_array_set(*args, output),
        ArrayCommand::Install(args) => run_array_daemon(args, ArrayDaemonAction::Install, output),
        ArrayCommand::Start(args) => run_array_daemon(args, ArrayDaemonAction::Start, output),
        ArrayCommand::Stop(args) => run_array_daemon(args, ArrayDaemonAction::Stop, output),
        ArrayCommand::Restart(args) => run_array_daemon(args, ArrayDaemonAction::Restart, output),
    }
}

fn run_array_status(output: OutputFormat) -> Result<()> {
    let config = Config::load_from_default_path()?;
    let status = array_status_with_daemon_overlay(&config);
    if output.is_json() {
        print_json(&status)?;
        return Ok(());
    }
    let delegation_ready = status
        .get("delegation")
        .and_then(|value| value.get("ready"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    println!("array: {}", if config.array.enabled { "on" } else { "off" });
    println!("primary_target: {}", config.array.primary_target);
    println!(
        "delegation: {}",
        if delegation_ready {
            "ready"
        } else {
            "not_ready"
        }
    );
    if !delegation_ready {
        println!(
            "warning: local delegation via `acps sessions ... --target ...` needs [local].session_auth = \"keyless\""
        );
    }
    if let Some(daemon) = status.get("daemon") {
        let daemon_status = daemon
            .get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unavailable");
        if daemon_status == "ready" {
            println!("daemon: ready");
        } else {
            let reason = daemon
                .get("reason")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");
            println!("daemon: unavailable ({reason})");
        }
    }
    for target in status
        .get("targets")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
    {
        let target_id = target
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("?");
        let agent_id = target
            .get("agent_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("?");
        let name = target
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let marker = if target_id == config.array.primary_target {
            " primary"
        } else {
            ""
        };
        let process_state = target
            .get("process_state")
            .and_then(serde_json::Value::as_str);
        let pid = target.get("pid").and_then(serde_json::Value::as_u64);
        if let Some(process_state) = process_state {
            let pid_suffix = match pid {
                Some(pid) => format!(" pid={pid}"),
                None => String::new(),
            };
            println!(
                "target: {target_id} agent={agent_id} name=\"{name}\" state={process_state}{pid_suffix}{marker}"
            );
        } else {
            println!("target: {target_id} agent={agent_id} name=\"{name}\"{marker}");
        }
    }
    Ok(())
}

fn array_status_with_daemon_overlay(config: &Config) -> serde_json::Value {
    match query_daemon_array_status(config) {
        ArrayDaemonStatus::Ready(mut data) => {
            if let Some(object) = data.as_object_mut() {
                object.insert(
                    "daemon".to_owned(),
                    serde_json::json!({ "status": "ready" }),
                );
            }
            data
        }
        ArrayDaemonStatus::Unavailable(reason) => {
            let delegation_ready = config.local.session_auth == LocalSessionAuth::Keyless;
            serde_json::json!({
                "enabled": config.array.enabled,
                "primary_target": config.array.primary_target,
                "delegation": {
                    "ready": delegation_ready,
                    "local_session_auth": config.local.session_auth.as_str(),
                },
                "targets": config.array.targets.iter().map(|target| {
                    serde_json::json!({
                        "id": target.id,
                        "agent_id": target.agent.id,
                        "name": target.agent.name,
                        "primary": target.id == config.array.primary_target,
                    })
                }).collect::<Vec<_>>(),
                "daemon": {
                    "status": "unavailable",
                    "reason": reason,
                },
            })
        }
    }
}

enum ArrayDaemonStatus {
    Ready(serde_json::Value),
    Unavailable(String),
}

fn query_daemon_array_status(config: &Config) -> ArrayDaemonStatus {
    let session_access = match resolve_session_access(config, None) {
        Ok(access) => access,
        Err(err) => return ArrayDaemonStatus::Unavailable(err.public_message()),
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => return ArrayDaemonStatus::Unavailable(format!("runtime unavailable: {err}")),
    };
    let result = runtime.block_on(async {
        tokio::time::timeout(
            ARRAY_STATUS_DAEMON_PROBE_TIMEOUT,
            query_daemon_array_status_async(config, &session_access),
        )
        .await
    });
    match result {
        Ok(Ok(body)) => ArrayDaemonStatus::Ready(body.get("data").cloned().unwrap_or(body)),
        Ok(Err(err)) => ArrayDaemonStatus::Unavailable(err.public_message()),
        Err(_) => ArrayDaemonStatus::Unavailable("request timed out".to_owned()),
    }
}

async fn query_daemon_array_status_async(
    config: &Config,
    session_access: &SessionAccess,
) -> Result<serde_json::Value> {
    match session_access {
        SessionAccess::Local => {
            local_daemon_request(config, CliMethod::Get, "/v1/array/status", None).await
        }
        SessionAccess::Bearer(session_key) => {
            let base_url = daemon_base_url(config.api.public_url.as_deref(), &config.api.bind)?;
            daemon_request(
                &base_url,
                CliMethod::Get,
                "/v1/array/status",
                session_key,
                None,
            )
            .await
        }
    }
}

fn run_array_toggle(enabled: bool, output: OutputFormat) -> Result<()> {
    let config_path = config::default_config_path()?;
    let mut config = Config::load_from_path(&config_path)?;
    config.array.enabled = enabled;
    write_config(&config_path, &config)?;
    if output.is_json() {
        print_json(&serde_json::json!({
            "enabled": config.array.enabled,
            "primary_target": config.array.primary_target,
        }))?;
    } else {
        println!("array: {}", if enabled { "on" } else { "off" });
        println!("primary_target: {}", config.array.primary_target);
    }
    Ok(())
}

fn run_array_add(args: ArrayAddArgs, output: OutputFormat) -> Result<()> {
    let home = home_dir()?;
    let config_path = config::default_config_path()?;
    let mut config = Config::load_from_path(&config_path)?;
    if config
        .array
        .targets
        .iter()
        .any(|target| target.agent.id == args.agent)
    {
        return Err(StackError::InvalidParam {
            field: "agent",
            reason: format!(
                "agent `{}` is already configured; Array v1 requires different harnesses per target",
                args.agent
            ),
        });
    }
    let registry = RegistryCatalog::load_with_override(&operator_registry_override(&home))?;
    let entry = registry.lookup_required(&args.agent)?;
    entry.ensure_supported()?;
    let agent = agent_config_from_registry_entry(&config, entry)?;
    config.array.targets.push(ArrayTargetConfig {
        id: entry.id.clone(),
        agent,
    });
    write_config(&config_path, &config)?;
    if output.is_json() {
        print_json(&serde_json::json!({
            "target_id": entry.id.clone(),
            "agent_id": entry.id.clone(),
        }))?;
    } else {
        println!("array target added: {}", entry.id);
        println!("agent: {}", entry.id);
    }
    Ok(())
}

fn agent_config_from_registry_entry(config: &Config, entry: &RegistryEntry) -> Result<AgentConfig> {
    let mut agent = config.agent.clone();
    agent.id = entry.id.clone();
    agent.name = entry.name.clone();
    agent.cwd = Some(config.workspace.root.clone());
    agent.env = env_refs_for_agent_id(&entry.id)
        .into_iter()
        .map(str::to_owned)
        .collect();
    agent.expected_sha256 = None;
    agent.restart = "on-crash".to_owned();
    agent.mode = None;
    agent.model = None;
    agent.harness_version = None;
    agent.adapter = adapter_from_registry_entry(entry);
    agent.provider = None;
    agent.subagent = None;
    agent.auto_update = None;
    agent.install = None;
    match entry.kind {
        RegistryKind::Native => {
            let harness = entry
                .harness
                .as_ref()
                .ok_or_else(|| StackError::RegistryLoad {
                    reason: format!("agent `{}` is missing harness metadata", entry.id),
                })?;
            agent.command = harness.id.clone();
            agent.args = vec!["acp".to_owned()];
        }
        RegistryKind::Adapter => {
            let adapter = entry
                .adapter
                .as_ref()
                .ok_or_else(|| StackError::RegistryLoad {
                    reason: format!("agent `{}` is missing adapter metadata", entry.id),
                })?;
            agent.command = adapter.id.clone();
            agent.args = Vec::new();
        }
    }
    Ok(agent)
}

fn run_array_set(args: ArraySetArgs, output: OutputFormat) -> Result<()> {
    if !args.custom_provider {
        reject_custom_provider_args(&args)?;
    }
    // Reject the --mode + --custom-provider conflict up front, alongside the
    // other argument-shape checks, so an invalid invocation fails before any
    // provider validation or in-memory mutation runs.
    if args.custom_provider && args.mode.is_some() {
        return Err(StackError::InvalidParam {
            field: "mode",
            reason: "--mode cannot be combined with --custom-provider".to_owned(),
        });
    }
    if args.provider.is_none()
        && args.model.is_none()
        && args.mode.is_none()
        && args.api_key_ref.is_none()
        && !args.custom_provider
    {
        return Err(StackError::InvalidParam {
            field: "array set",
            reason: "pass at least one of --provider, --model, --mode, or --api-key-ref".to_owned(),
        });
    }
    let home = home_dir()?;
    let config_path = config::default_config_path()?;
    let mut config = Config::load_from_path(&config_path)?;
    let target_index = config
        .array
        .targets
        .iter()
        .position(|target| target.id == args.target)
        .ok_or_else(|| StackError::InvalidParam {
            field: "target",
            reason: format!("unknown Array target `{}`", args.target),
        })?;
    let registry = RegistryCatalog::load_with_override(&operator_registry_override(&home))?;
    let entry = registry.lookup_required(&config.array.targets[target_index].agent.id)?;
    let mut target_config = config.clone();
    target_config.agent = config.array.targets[target_index].agent.clone();

    if args.custom_provider {
        apply_custom_provider(&mut target_config, entry, &args)?;
    }

    if !args.custom_provider
        && let Some(provider_id) = args.provider.as_deref()
    {
        apply_provider(
            &mut target_config,
            entry,
            provider_id,
            args.api_key_ref.clone(),
        )?;
    } else if !args.custom_provider
        && let Some(api_key_ref) = args.api_key_ref.clone()
    {
        let Some(provider) = target_config.agent.provider.as_mut() else {
            return Err(StackError::InvalidParam {
                field: "api-key-ref",
                reason: "--api-key-ref requires --provider or an existing target provider"
                    .to_owned(),
            });
        };
        provider.api_key_ref = Some(api_key_ref.clone());
        if !target_config
            .agent
            .env
            .iter()
            .any(|name| name == &api_key_ref)
        {
            target_config.agent.env.push(api_key_ref);
        }
    }

    if let Some(mode) = args.mode.as_deref() {
        if !entry.set_mode {
            return Err(StackError::AgentConfigProvision {
                path: config_path.clone(),
                reason: format!(
                    "{} does not support mode configuration through `acps array set`",
                    entry.name
                ),
            });
        }
        validate_agent_session_config_value(
            &home,
            &target_config,
            AgentSessionConfigCategory::Mode,
            mode,
        )?;
        target_config.agent.mode = Some(mode.to_owned());
    }

    if !args.custom_provider
        && let Some(model) = args.model.as_deref()
    {
        if !entry.set_model {
            return Err(StackError::AgentConfigProvision {
                path: config_path.clone(),
                reason: format!(
                    "{} does not support model configuration through `acps array set`",
                    entry.name
                ),
            });
        }
        let provider_id = target_config.agent.provider.as_ref().and_then(|provider| {
            agent_provider_id_for_provider_id(&target_config.agent.id, &provider.id)
        });
        let resolved = resolve_agent_model_value(&home, &target_config, provider_id, model)?;
        if let Some(provider) = target_config.agent.provider.as_mut() {
            provider.model = Some(resolved);
            target_config.agent.model = None;
        } else {
            target_config.agent.model = Some(resolved);
        }
    }

    config.array.targets[target_index].agent = target_config.agent;
    let (validated, canonical) = canonicalize_config_for_write(&config)?;
    let target_index = validated
        .array
        .targets
        .iter()
        .position(|target| target.id == args.target)
        .ok_or_else(|| StackError::InvalidParam {
            field: "target",
            reason: format!("unknown Array target `{}`", args.target),
        })?;
    let mut target_config = validated.clone();
    target_config.agent = validated.array.targets[target_index].agent.clone();
    let provisioned = provision_agent_headless_config(&target_config, &home)?;
    atomic_write_owner_only(&config_path, canonical.as_bytes())?;
    if output.is_json() {
        let target = &validated.array.targets[target_index];
        print_json(&serde_json::json!({
            "target_id": target.id,
            "agent_id": target.agent.id,
            "provider": target.agent.provider.as_ref().map(|provider| provider.id.clone()),
            "model": target.agent.provider.as_ref().and_then(|provider| provider.model.clone()).or_else(|| target.agent.model.clone()),
            "mode": target.agent.mode,
            "provisioned": provisioned.iter().map(|item| {
                serde_json::json!({
                    "label": item.label,
                    "path": item.path.display().to_string(),
                })
            }).collect::<Vec<_>>(),
        }))?;
    } else {
        let target = &validated.array.targets[target_index];
        println!("array target set: {}", target.id);
        println!("agent: {}", target.agent.id);
        if let Some(provider) = target.agent.provider.as_ref() {
            println!("provider: {}", provider.id);
            if let Some(model) = provider.model.as_deref() {
                println!("model: {model}");
            }
            if let Some(api_key_ref) = provider.api_key_ref.as_deref() {
                println!("api_key_ref: {api_key_ref}");
            }
        } else if let Some(model) = target.agent.model.as_deref() {
            println!("model: {model}");
        }
        if let Some(mode) = target.agent.mode.as_deref() {
            println!("mode: {mode}");
        }
        for item in provisioned {
            println!("{}: {}", item.label, item.path.display());
        }
    }
    Ok(())
}

fn apply_custom_provider(
    config: &mut Config,
    entry: &RegistryEntry,
    args: &ArraySetArgs,
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
    let provider_id = required_custom_arg("provider", args.provider.clone())?;
    let provider_name = required_custom_arg("provider-name", args.provider_name.clone())?;
    let base_url = required_custom_arg("base-url", args.base_url.clone())?;
    let api_key_ref = required_custom_arg("api-key-ref", args.api_key_ref.clone())?;
    let model = required_custom_arg("model", args.model.clone())?;
    let model_name = args.model_name.clone().unwrap_or_else(|| model.clone());
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
        api_key_ref: Some(api_key_ref),
        custom: Some(AgentCustomProviderConfig {
            name: provider_name,
            base_url,
            api,
            model_name: Some(model_name),
            context,
            output_max_tokens,
        }),
    });
    Ok(())
}

fn reject_custom_provider_args(args: &ArraySetArgs) -> Result<()> {
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

fn apply_provider(
    config: &mut Config,
    entry: &RegistryEntry,
    provider_id: &str,
    explicit_api_key_ref: Option<String>,
) -> Result<()> {
    if !entry.set_provider {
        return Err(StackError::InvalidParam {
            field: "provider",
            reason: format!(
                "{} does not support provider configuration through `acps array set`",
                entry.name
            ),
        });
    }
    if !provider_id_is_known(provider_id) {
        return Err(StackError::InvalidParam {
            field: "provider",
            reason: format!("provider `{provider_id}` is not listed in provider/env mapping"),
        });
    }
    if !provider_id_supports_agent(provider_id, &config.agent.id) {
        return Err(StackError::InvalidParam {
            field: "provider",
            reason: format!(
                "provider `{provider_id}` is not supported for agent `{}`",
                config.agent.id
            ),
        });
    }
    let native_auth = provider_uses_agent_native_auth(&config.agent.id, provider_id);
    if native_auth && explicit_api_key_ref.is_some() {
        return Err(StackError::AgentConfigProvision {
            path: config::default_config_path()?,
            reason: format!(
                "{} provider `{provider_id}` uses agent-native auth; do not pass --api-key-ref",
                entry.name
            ),
        });
    }
    let api_key_ref = explicit_api_key_ref
        .or_else(|| default_api_key_ref_for_agent_provider(&config.agent.id, provider_id));
    if api_key_ref.is_none() && !native_auth {
        return Err(StackError::AgentConfigProvision {
            path: config::default_config_path()?,
            reason: format!(
                "provider `{provider_id}` has no default API-key env var; pass --api-key-ref"
            ),
        });
    }
    let required_env_refs = required_env_refs_for_agent_provider_id(
        &config.agent.id,
        provider_id,
        api_key_ref.as_deref(),
    );
    for env_ref in &required_env_refs {
        if !config.agent.env.iter().any(|name| name == env_ref) {
            config.agent.env.push(env_ref.clone());
        }
    }
    config.agent.provider = Some(AgentProviderConfig {
        id: provider_id.to_owned(),
        model: None,
        api_key_ref,
        custom: None,
    });
    config.agent.model = None;
    Ok(())
}

fn run_array_daemon(
    args: ArrayDaemonArgs,
    action: ArrayDaemonAction,
    output: OutputFormat,
) -> Result<()> {
    let config = Config::load_from_default_path()?;
    ensure_daemon_action_allowed(&config, args.target.as_deref(), action)?;
    let targets = resolve_targets(&config, args.target.as_deref())?;
    let admin_key = resolve_admin_key(args.admin_key, std::io::stdin().is_terminal())?;
    let base_url = daemon_base_url(config.api.public_url.as_deref(), &config.api.bind)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|source| StackError::ServeIo { source })?;
    let action = action.as_str();
    // Fan-out over all configured targets when --target is omitted. A failing
    // target must NOT abort the batch: every target is attempted, each outcome
    // is recorded and reported, and a single aggregated error is returned at
    // the end. Using `?` here would short-circuit on the first failure, leave a
    // partially-mutated fleet, and discard the per-target report entirely.
    let total = targets.len();
    let mut results = Vec::with_capacity(total);
    let mut failures: Vec<String> = Vec::new();
    for target in targets {
        let encoded = encode_path_segment(&target.id);
        let path = format!("/v1/array/targets/{encoded}/{action}");
        match runtime.block_on(daemon_request(
            &base_url,
            CliMethod::Post,
            &path,
            &admin_key,
            None,
        )) {
            Ok(response) => results.push(serde_json::json!({
                "target_id": target.id,
                "agent_id": target.agent.id,
                "status": "ok",
                "response": response.get("data").cloned().unwrap_or(response),
            })),
            Err(error) => {
                let message = error.to_string();
                failures.push(format!("{}: {message}", target.id));
                results.push(serde_json::json!({
                    "target_id": target.id,
                    "agent_id": target.agent.id,
                    "status": "error",
                    "error": message,
                }));
            }
        }
    }
    if output.is_json() {
        print_json(&serde_json::json!({
            "action": action,
            "ok": failures.is_empty(),
            "targets": results,
        }))?;
    } else {
        for result in &results {
            let target_id = result["target_id"].as_str().unwrap_or("?");
            let agent_id = result["agent_id"].as_str().unwrap_or("?");
            if result["status"].as_str() == Some("ok") {
                println!("array {action}: target={target_id} agent={agent_id} ok");
            } else {
                let error = result["error"].as_str().unwrap_or("unknown error");
                println!("array {action}: target={target_id} agent={agent_id} error: {error}");
            }
        }
    }
    if !failures.is_empty() {
        return Err(StackError::ArrayTargetsFailed {
            action,
            failed: failures.len(),
            total,
            summary: failures.join("; "),
        });
    }
    Ok(())
}

fn ensure_daemon_action_allowed(
    config: &Config,
    target_id: Option<&str>,
    action: ArrayDaemonAction,
) -> Result<()> {
    if !action.gated_to_primary_when_array_off() || config.array.enabled {
        return Ok(());
    }
    match target_id {
        Some(target_id) if target_id == config.array.primary_target => Ok(()),
        Some(_) => Err(StackError::InvalidParam {
            field: "target",
            reason: format!(
                "Array mode is off; only default target `{}` is active",
                config.array.primary_target
            ),
        }),
        None => Err(StackError::InvalidParam {
            field: "target",
            reason: format!(
                "Array mode is off; use `acps agent {}` for default target `{}` or run `acps array on` first",
                action.as_str(),
                config.array.primary_target
            ),
        }),
    }
}

fn resolve_targets<'a>(
    config: &'a Config,
    target_id: Option<&str>,
) -> Result<Vec<&'a ArrayTargetConfig>> {
    if let Some(target_id) = target_id {
        return config
            .array
            .target(target_id)
            .map(|target| vec![target])
            .ok_or_else(|| StackError::InvalidParam {
                field: "target",
                reason: format!("unknown Array target `{target_id}`"),
            });
    }
    Ok(config.array.targets.iter().collect())
}

fn write_config(config_path: &Path, config: &Config) -> Result<()> {
    let (_, canonical) = canonicalize_config_for_write(config)?;
    atomic_write_owner_only(config_path, canonical.as_bytes())
}

fn canonicalize_config_for_write(config: &Config) -> Result<(Config, String)> {
    let mut config = config.clone();
    if let Some(primary) = config.array.primary_target() {
        config.agent = primary.agent.clone();
    }
    let canonical = config.to_canonical_toml()?;
    let validated = config::load_config_from_str(&canonical)?;
    let canonical = validated.to_canonical_toml()?;
    Ok((validated, canonical))
}
