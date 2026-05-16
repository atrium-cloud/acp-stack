use crate::agent_installer::run_installer;
use crate::config::Config;
use crate::error::{Result, StackError};
use crate::fs_util::{
    create_dir_owner_only, home_dir, parent_dir, pre_create_owner_only, set_owner_only_file,
};
use crate::secrets::SecretStore;
use crate::state::{StateStore, default_state_path};
use clap::Subcommand;
use std::collections::HashMap;

use super::core::daemon_base_url;

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
}

pub(super) fn run_agent_command(command: AgentCommand) -> Result<()> {
    match command {
        AgentCommand::Install => run_agent_install(),
        AgentCommand::Start => run_agent_daemon_post("/v1/agent/start", "start"),
        AgentCommand::Stop => run_agent_daemon_post("/v1/agent/stop", "stop"),
        AgentCommand::Status => run_agent_status(),
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
    let install = config
        .agent
        .install
        .clone()
        .ok_or(StackError::AgentNotConfigured)?;
    let expected_sha256 = config.agent.expected_sha256.clone();

    let state_path = default_state_path(&home);
    let state_dir = parent_dir(&state_path)?;
    create_dir_owner_only(state_dir)?;
    pre_create_owner_only(&state_path)?;
    let store = StateStore::open(&state_path)?;
    store.migrate()?;
    set_owner_only_file(&state_path)?;

    let env = resolve_agent_env_for_cli(&home, &config)?;
    let workspace_root = std::path::PathBuf::from(config.workspace.root.clone());
    let outcome = run_installer(
        &install,
        expected_sha256.as_deref(),
        env,
        &workspace_root,
        &store,
    )?;

    println!("agent install: {}", outcome.label());
    println!("path: {}", outcome.path().display());
    println!("sha256: {}", outcome.sha256());
    Ok(())
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
    let state_path = default_state_path(&home);
    let state_dir = parent_dir(&state_path)?;
    create_dir_owner_only(state_dir)?;
    pre_create_owner_only(&state_path)?;
    let store = StateStore::open(&state_path)?;
    store.migrate()?;
    set_owner_only_file(&state_path)?;

    println!("agent: {} ({})", config.agent.name, config.agent.id);
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
