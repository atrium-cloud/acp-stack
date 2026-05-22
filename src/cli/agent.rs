use crate::agent_installer::{install_resolved, run_installer};
use crate::agent_registry::RegistryCatalog;
use crate::config::{self, AgentProviderConfig, Config};
use crate::error::{Result, StackError};
use crate::fs_util::{
    atomic_write_owner_only, create_dir_owner_only, home_dir, parent_dir, pre_create_owner_only,
    set_owner_only_file,
};
use crate::runtime::acp_bridge::{
    AcpBridge, AgentSessionConfigCategory, SessionEventSink, session_config_id_for_value,
    session_config_values, session_model_selection_for_value, session_model_values,
};
use crate::runtime::agent_headless_config::provision_agent_headless_config;
use crate::runtime::agent_registry::RegistryEntry;
use crate::runtime::provider_keys::{
    env_refs_for_agent_id, env_var_for_agent_provider_id, optional_env_refs_for_provider_id,
    provider_id_is_known, provider_id_supports_agent, required_env_refs_for_provider_id,
};
use crate::secrets::SecretStore;
use crate::state::{StateStore, default_state_path};
use clap::{Args, Subcommand};
use std::collections::HashMap;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use super::core::daemon_base_url;

const ACP_CONFIG_OPTIONS_FIXTURE_ENV: &str = "ACP_STACK_AGENT_CONFIG_OPTIONS_PATH";
const ACP_NEW_SESSION_RESPONSE_FIXTURE_ENV: &str = "ACP_STACK_AGENT_NEW_SESSION_RESPONSE_PATH";

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
    /// Set the provider id, model, and API-key ref used by generated agent config.
    Set(AgentSetArgs),
}

#[derive(Debug, Args)]
pub struct AgentSetArgs {
    /// Provider id, such as opencode-go, openai, or anthropic.
    #[arg(long)]
    provider: Option<String>,
    /// Provider-qualified model id or model pattern.
    #[arg(long)]
    model: Option<String>,
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
            reason: format!("provider `{provider_id}` is not listed in data/mapping.toml"),
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
    });
    let model = match args.model {
        Some(model) => resolve_agent_model_value(&home, &config, Some(&provider_id), &model)?,
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
    print_agent_set_effective_notice();
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
    print_agent_set_effective_notice();
    Ok(())
}

fn run_agent_model_set(
    mut config: Config,
    config_path: PathBuf,
    home: &Path,
    args: AgentSetArgs,
    entry: &RegistryEntry,
) -> Result<()> {
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
    print_agent_set_effective_notice();
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
    print_agent_set_effective_notice();
    Ok(())
}

fn print_agent_set_agent(config: &Config) {
    println!("agent: {}", config.agent.id);
}

fn print_agent_set_effective_notice() {
    println!("settings will take effect on new sessions");
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
        match row.version.as_deref() {
            Some(value) if !value.is_empty() => {
                println!("installed {}: {value}", row.step);
            }
            _ => println!("installed {}: version unknown", row.step),
        }
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
}
