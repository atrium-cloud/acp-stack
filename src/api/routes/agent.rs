use axum::extract::State;
use chrono::{SecondsFormat, Utc};
use serde::Serialize;

use super::super::core::AppState;
use crate::acp_bridge::AgentCapabilitiesDto;
use crate::agent_installer::{
    InstallerSequenceResult, install_resolved_capture, run_installer_capture,
};
use crate::agent_registry::RegistryCatalog;
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
    let workspace_root = std::path::PathBuf::from(state.config.workspace.root.clone());
    let home = home_dir()?;
    let local_bin = home.join(".local").join("bin");
    let log_base = crate::state::default_installer_log_base(&home);

    let outcome = if let Some(install) = state.config.agent.install.clone() {
        // Escape-hatch shell recipe. One row, persisted after the shell runs.
        let env = open_agent_env(&state.config)?;
        let expected_sha256 = state.config.agent.expected_sha256.clone();
        let mut result = tokio::task::spawn_blocking(move || {
            run_installer_capture(&install, expected_sha256.as_deref(), env, &workspace_root)
        })
        .await
        .map_err(|err| StackError::AgentInitializeFailed {
            reason: format!("installer thread join failed: {err}"),
        })?;
        crate::runtime::agent_installer::persist_step_logs_to_disk(
            &mut result.row,
            &state.config.agent.id,
            Some(&log_base),
        )?;
        {
            let store = state.state.lock().await;
            store.append_installer_run(InstallerRunInput {
                agent_id: &state.config.agent.id,
                started_at: &result.row.started_at,
                finished_at: result.row.finished_at.as_deref(),
                status: &result.row.status,
                stdout: &result.row.stdout,
                stderr: &result.row.stderr,
                exit_status: result.row.exit_status,
                step: &result.row.step,
                version: result.row.version.as_deref(),
                log_dir: result.row.log_dir.as_deref(),
                apply_run_id: None,
            })?;
        }
        result.outcome?
    } else {
        // Registry-resolved install: one row for native, two for adapter-backed.
        let override_path = home.join(".config").join("acp-stack").join("agents.toml");
        let registry = RegistryCatalog::load_with_override(&override_path)?;
        let entry = registry
            .lookup(&state.config.agent.id)
            .ok_or_else(|| StackError::AgentRegistryMissing {
                id: state.config.agent.id.clone(),
            })?
            .clone();
        let agent = state.config.agent.clone();
        let mut result: InstallerSequenceResult = tokio::task::spawn_blocking(move || {
            install_resolved_capture(
                &agent,
                &entry,
                Default::default(),
                &workspace_root,
                &local_bin,
            )
        })
        .await
        .map_err(|err| StackError::AgentInitializeFailed {
            reason: format!("installer thread join failed: {err}"),
        })?;
        for row in result.rows.iter_mut() {
            crate::runtime::agent_installer::persist_step_logs_to_disk(
                row,
                &state.config.agent.id,
                Some(&log_base),
            )?;
        }
        {
            let store = state.state.lock().await;
            for row in &result.rows {
                store.append_installer_run(InstallerRunInput {
                    agent_id: &state.config.agent.id,
                    started_at: &row.started_at,
                    finished_at: row.finished_at.as_deref(),
                    status: &row.status,
                    stdout: &row.stdout,
                    stderr: &row.stderr,
                    exit_status: row.exit_status,
                    step: &row.step,
                    version: row.version.as_deref(),
                    log_dir: row.log_dir.as_deref(),
                    apply_run_id: None,
                })?;
            }
        }
        result.outcome?
    };

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
pub(crate) struct AgentRestartResponse {
    stopped_at: String,
    started_at: String,
    /// Exit status of the prior process. `None` when the supervisor
    /// was not running (the restart degenerated into a plain start).
    prior_exit_status: Option<i32>,
    capabilities: AgentCapabilitiesDto,
    pid: Option<u32>,
}

/// Stop the supervised agent (if running) and start it again, reading
/// the freshly-on-disk `[agent]` block instead of the daemon's
/// in-memory `Arc<Config>` snapshot. Used by operators after
/// `acps agent set` writes provider/model changes that require a
/// process-level config reload — agents that read provider/model from
/// their on-disk config at process start can only see updated values
/// after a restart. Goose model changes do NOT need this endpoint;
/// clients can switch live via `session/set_config_option`.
///
/// Note: the daemon's other handlers (status, capabilities, etc.)
/// continue to see the cached config until the daemon itself is
/// restarted (`systemctl restart acps` or equivalent). This endpoint
/// only refreshes the supervised agent process.
pub(crate) async fn agent_restart_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<AgentRestartResponse>, StackError> {
    // Load + validate the fresh on-disk config AND resolve env BEFORE
    // stopping the currently running agent. A malformed config or a
    // missing required secret should fail this call cleanly and leave
    // the running agent alone, rather than taking it down and
    // returning an error with no agent running at all.
    let fresh_config = crate::config::Config::load_from_path(&state.runtime_paths.config_path)?;
    let env = open_agent_env(&fresh_config)?;

    // Now safe to stop the prior process. `stop` returns
    // `Result<Option<i32>, _>`: outer `Err(AgentNotRunning)` means
    // there was nothing to stop (acceptable — a "restart" against a
    // stopped agent degenerates into a plain start); inner
    // `Option<i32>` is the optional exit status of the prior process.
    let prior_exit_status = match state
        .agent_supervisor
        .stop(&state.state, &state.event_hub)
        .await
    {
        Ok(code) => code,
        Err(StackError::AgentNotRunning) => None,
        Err(err) => return Err(err),
    };
    let stopped_at = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);

    // Update the live agent-config cache so post-restart session
    // creation (which reads `state.live_agent_config` for
    // `agent.mode`/`agent.model`/`agent.provider`) sees the new
    // values too. Without this, the supervised process would be on
    // the new binary/command but `/v1/sessions` would still apply
    // the stale model — silently giving operators the wrong agent
    // behavior after a `acps agent set`.
    {
        let mut live = state.live_agent_config.lock().await;
        *live = fresh_config.agent.clone();
    }
    let capabilities = state
        .agent_supervisor
        .start(
            &fresh_config.agent,
            &fresh_config.workspace.root,
            env,
            &state.state,
            state.event_hub.clone(),
            Some(state.permissions.clone()),
        )
        .await?;
    let started_at = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
    let pid = state.agent_supervisor.snapshot().await.pid;

    Ok(ApiSuccess::new(AgentRestartResponse {
        stopped_at,
        started_at,
        prior_exit_status,
        capabilities,
        pid,
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
