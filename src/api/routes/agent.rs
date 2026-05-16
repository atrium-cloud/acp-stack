use axum::extract::State;
use chrono::{SecondsFormat, Utc};
use serde::Serialize;

use super::super::core::AppState;
use crate::acp_bridge::AgentCapabilitiesDto;
use crate::agent_installer::run_installer_capture;
use crate::config::{AgentAdapterConfig, Config};
use crate::envelope::ApiSuccess;
use crate::error::{Result, StackError};
use crate::fs_util::home_dir;
use crate::secrets::SecretStore;
use crate::state::InstallerRunInput;
use crate::supervisor::AgentSnapshot;

#[derive(Serialize)]
pub(crate) struct AgentInstallResponse {
    outcome: &'static str,
    path: String,
    sha256: String,
}

pub(crate) async fn agent_install_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<AgentInstallResponse>, StackError> {
    let install = state
        .config
        .agent
        .install
        .clone()
        .ok_or(StackError::AgentNotConfigured)?;
    let expected_sha256 = state.config.agent.expected_sha256.clone();
    // Resolve agent env from the secret store. The installer should only
    // see the same names the agent itself will see (security.md:91).
    let env = open_agent_env(&state.config)?;
    let workspace_root = std::path::PathBuf::from(state.config.workspace.root.clone());

    // Run the synchronous installer on a blocking thread so its
    // up-to-10-minute timeout window cannot pin a tokio runtime worker.
    // Critically, we do NOT hold the state lock while it runs — that would
    // make every other state-backed endpoint (incl. auth-failure logging)
    // wait behind the install.
    let result = tokio::task::spawn_blocking(move || {
        run_installer_capture(&install, expected_sha256.as_deref(), env, &workspace_root)
    })
    .await
    .map_err(|err| StackError::AgentInitializeFailed {
        reason: format!("installer thread join failed: {err}"),
    })?;

    // Persist the row briefly under the state lock. The lock is held only
    // for the single INSERT, not for the installer's runtime.
    {
        let store = state.state.lock().await;
        store.append_installer_run(InstallerRunInput {
            started_at: &result.row.started_at,
            finished_at: result.row.finished_at.as_deref(),
            status: &result.row.status,
            stdout: &result.row.stdout,
            stderr: &result.row.stderr,
            exit_status: result.row.exit_status,
        })?;
    }

    let outcome = result.outcome?;
    let outcome_label = outcome.label();
    let path = outcome.path().to_string_lossy().into_owned();
    let sha256 = outcome.sha256().to_owned();
    Ok(ApiSuccess::new(AgentInstallResponse {
        outcome: outcome_label,
        path,
        sha256,
    }))
}

pub(super) fn open_agent_env(config: &Config) -> Result<std::collections::HashMap<String, String>> {
    if config.agent.env.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let home = home_dir()?;
    let store = SecretStore::open(&home)?;
    let mut env = std::collections::HashMap::with_capacity(config.agent.env.len());
    for name in &config.agent.env {
        let value = store.get(name)?;
        env.insert(name.clone(), value.to_owned());
    }
    Ok(env)
}

/// Resolve every configured `[mcp.servers]` entry into the SDK `McpServer`
/// type. Returns an empty Vec when no MCP servers are configured, so the
/// secret store is only opened when there's something to resolve.
pub(super) fn open_mcp_servers(
    config: &Config,
) -> Result<Vec<agent_client_protocol::schema::McpServer>> {
    if config.mcp.servers.is_empty() {
        return Ok(Vec::new());
    }
    let home = home_dir()?;
    let store = SecretStore::open(&home)?;
    crate::mcp::resolve_mcp_servers(&config.mcp, &store)
}

#[derive(Serialize)]
pub(crate) struct AgentStartResponse {
    started_at: String,
    capabilities: AgentCapabilitiesDto,
    pid: Option<u32>,
}

pub(crate) async fn agent_start_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<AgentStartResponse>, StackError> {
    // Resolve env BEFORE invoking the supervisor so the secret store is only
    // opened when [agent].env is non-empty. Production deployments always
    // have a populated store; tests with empty agent.env skip the open
    // entirely. open_agent_env enforces the same allowlist semantics
    // (security.md:91) regardless of caller.
    let env = open_agent_env(&state.config)?;
    let capabilities = state
        .agent_supervisor
        .start(
            &state.config.agent,
            &state.config.workspace.root,
            env,
            &state.state,
            state.event_hub.clone(),
            Some(state.permissions.clone()),
        )
        .await?;
    let started_at = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
    let pid = state.agent_supervisor.snapshot().await.pid;
    Ok(ApiSuccess::new(AgentStartResponse {
        started_at,
        capabilities,
        pid,
    }))
}

#[derive(Serialize)]
pub(crate) struct AgentStopResponse {
    stopped_at: String,
    exit_status: Option<i32>,
}

pub(crate) async fn agent_stop_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<AgentStopResponse>, StackError> {
    let exit_status = state
        .agent_supervisor
        .stop(&state.state, &state.event_hub)
        .await?;
    let stopped_at = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
    Ok(ApiSuccess::new(AgentStopResponse {
        stopped_at,
        exit_status,
    }))
}

#[derive(Serialize)]
pub(crate) struct AgentCapabilitiesResponseBody {
    agent_id: String,
    adapter: Option<AgentAdapterConfig>,
    captured_at: String,
    capabilities: serde_json::Value,
    process_state: String,
}

pub(crate) async fn agent_capabilities_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<AgentCapabilitiesResponseBody>, StackError> {
    let agent_id = state.config.agent.id.clone();
    let snapshot: AgentSnapshot = state.agent_supervisor.snapshot().await;
    let store = state.state.lock().await;
    let record = store.latest_agent_capabilities(&agent_id)?;
    drop(store);
    let record = record.ok_or(StackError::AgentNotInitialized)?;
    let capabilities = serde_json::from_str(&record.capabilities_json).map_err(|err| {
        StackError::AgentInitializeFailed {
            reason: format!("stored capabilities are unparseable: {err}"),
        }
    })?;
    Ok(ApiSuccess::new(AgentCapabilitiesResponseBody {
        agent_id: record.agent_id,
        adapter: state.config.agent.adapter.clone(),
        captured_at: record.captured_at,
        capabilities,
        process_state: format!("{:?}", snapshot.state).to_lowercase(),
    }))
}
