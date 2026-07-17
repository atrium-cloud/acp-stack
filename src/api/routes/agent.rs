use std::time::Duration;

use axum::Json;
use axum::extract::{Path, Query, State};
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use super::super::core::{AgentTargetRuntime, AppState};
use crate::config::{AgentAdapterConfig, Config, LocalSessionAuth};
use crate::envelope::ApiSuccess;
use crate::error::{Result, StackError};
use crate::fs_util::home_dir;
use crate::runtime::agent::acp_bridge::{AgentCapabilitiesDto, AgentSessionConfigCategory};
use crate::runtime::agent::agent_headless_config::{
    CleanedAgentConfig, ProvisionedAgentConfig, cleanup_agent_headless_config,
};
use crate::runtime::agent::model_discovery::{
    DEFAULT_MODELS_DISCOVERY_TIMEOUT, advertised_values_for_category,
    fetch_session_config_with_timeout,
};
use crate::runtime::agent::provider_keys::{
    ResolvedAgentEnvironment, resolve_agent_environment, resolve_agent_environment_without_secrets,
};
use crate::runtime::agent::supervisor::{AgentSnapshot, AgentStartRequest};
use crate::runtime::agent::switch::{
    AgentSwitchRequest as PlannedAgentSwitchRequest, AgentSwitchSecretMigration,
    adapter_from_registry_entry, plan_agent_switch,
};
use crate::runtime::install::agent_installer::{
    InstallerSequenceResult, install_resolved_capture, run_installer_capture,
};
use crate::runtime::install::agent_registry::RegistryCatalog;
use crate::runtime::install::skill_installer::{SkillPortReport, port_agent_skills};
use crate::runtime::workspace_sources::workspace_init::prepare_workspace_base_dirs;
use crate::secrets::SecretStore;
use crate::state::InstallerRunInput;

#[derive(Serialize)]
pub(crate) struct AgentInstallResponse {
    outcome: &'static str,
    path: String,
    sha256: String,
}

pub(crate) async fn agent_install_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<AgentInstallResponse>, StackError> {
    let target_id = state.default_target_id().await?;
    install_agent_target(&state, &target_id).await
}

#[derive(Serialize)]
pub(crate) struct ArrayStatusResponse {
    enabled: bool,
    primary_target: String,
    delegation: ArrayDelegationStatusResponse,
    targets: Vec<ArrayTargetStatusResponse>,
}

#[derive(Serialize)]
struct ArrayDelegationStatusResponse {
    ready: bool,
    local_session_auth: &'static str,
}

#[derive(Serialize)]
struct ArrayTargetStatusResponse {
    id: String,
    agent_id: String,
    name: String,
    primary: bool,
    process_state: String,
    pid: Option<u32>,
    configured_providers: Vec<super::status::ProviderStatusJson>,
    loaded_providers: Option<Vec<super::status::ProviderStatusJson>>,
    provider_restart_required: bool,
    /// Set when this target's provider resolution fails; other targets keep
    /// reporting so one broken credential never aborts the whole fleet status.
    provider_error: Option<String>,
}

pub(crate) async fn array_status_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<ArrayStatusResponse>, StackError> {
    let config = state.refresh_array_runtime_from_disk().await?;
    let local_session_auth = state.local_session_auth().await;
    let mut targets = Vec::with_capacity(config.array.targets.len());
    for target_config in &config.array.targets {
        let target = state.agent_target(&target_config.id)?;
        let snapshot = target.supervisor.snapshot().await;
        let mut resolved_config = config.clone();
        resolved_config.agent = target_config.agent.clone();
        let (configured_provider_snapshot, provider_error) =
            super::status::configured_providers_or_error(open_agent_environment(&resolved_config));
        let provider_restart_required = super::status::provider_restart_required_for_status(
            provider_error.is_some(),
            snapshot.state,
            snapshot.loaded_providers.as_deref(),
            &configured_provider_snapshot,
        );
        targets.push(ArrayTargetStatusResponse {
            id: target_config.id.clone(),
            agent_id: target_config.agent.id.clone(),
            name: target_config.agent.name.clone(),
            primary: target_config.id == config.array.primary_target,
            process_state: snapshot.state.as_wire_str().to_owned(),
            pid: snapshot.pid,
            configured_providers: configured_provider_snapshot
                .iter()
                .map(super::status::ProviderStatusJson::from)
                .collect(),
            loaded_providers: snapshot.loaded_providers.as_ref().map(|providers| {
                providers
                    .iter()
                    .map(super::status::ProviderStatusJson::from)
                    .collect()
            }),
            provider_restart_required,
            provider_error,
        });
    }
    Ok(ApiSuccess::new(ArrayStatusResponse {
        enabled: config.array.enabled,
        primary_target: config.array.primary_target,
        delegation: ArrayDelegationStatusResponse {
            ready: local_session_auth == LocalSessionAuth::Keyless,
            local_session_auth: local_session_auth.as_str(),
        },
        targets,
    }))
}

pub(crate) async fn array_agent_install_handler(
    State(state): State<AppState>,
    Path(target_id): Path<String>,
) -> std::result::Result<ApiSuccess<AgentInstallResponse>, StackError> {
    install_agent_target(&state, &target_id).await
}

async fn install_agent_target(
    state: &AppState,
    target_id: &str,
) -> std::result::Result<ApiSuccess<AgentInstallResponse>, StackError> {
    let (config, _) = load_fresh_config_for_target(state, target_id).await?;
    install_agent_for_config(state, &config)
        .await
        .map(ApiSuccess::new)
}

async fn install_agent_for_config(
    state: &AppState,
    config: &Config,
) -> Result<AgentInstallResponse> {
    prepare_workspace_base_dirs(&config.workspace)?;
    let workspace_root = std::path::PathBuf::from(config.workspace.root.clone());
    let home = home_dir()?;
    let local_bin = home.join(".local").join("bin");
    let log_base = crate::state::default_installer_log_base(&home);

    let outcome = if let Some(install) = config.agent.install.clone() {
        // Escape-hatch shell recipe. One row, persisted after the shell runs.
        let env = open_agent_env(config)?;
        let expected_sha256 = config.agent.expected_sha256.clone();
        let mut result = tokio::task::spawn_blocking(move || {
            run_installer_capture(&install, expected_sha256.as_deref(), env, &workspace_root)
        })
        .await
        .map_err(|err| StackError::AgentInitializeFailed {
            reason: format!("installer thread join failed: {err}"),
        })?;
        crate::runtime::install::agent_installer::persist_step_logs_to_disk(
            &mut result.row,
            &config.agent.id,
            Some(&log_base),
        )?;
        {
            let store = state.state.lock().await;
            store.append_installer_run(InstallerRunInput {
                agent_id: &config.agent.id,
                started_at: &result.row.started_at,
                finished_at: result.row.finished_at.as_deref(),
                status: &result.row.status,
                stdout: &result.row.stdout,
                stderr: &result.row.stderr,
                exit_status: result.row.exit_status,
                step: &result.row.step,
                version: result.row.version.as_deref(),
                operation: crate::state::INSTALLER_OPERATION_INSTALL,
                method: result.row.method.as_deref(),
                log_dir: result.row.log_dir.as_deref(),
                apply_run_id: None,
            })?;
        }
        result.outcome?
    } else {
        // Registry-resolved install: one row for native, two for adapter-backed.
        let override_path = home.join(".config").join("acp-stack").join("agents.toml");
        let registry = RegistryCatalog::load_with_override(&override_path)?;
        let entry = registry.lookup_required(&config.agent.id)?.clone();
        let agent = config.agent.clone();
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
            crate::runtime::install::agent_installer::persist_step_logs_to_disk(
                row,
                &config.agent.id,
                Some(&log_base),
            )?;
        }
        {
            let store = state.state.lock().await;
            for row in &result.rows {
                store.append_installer_run(InstallerRunInput {
                    agent_id: &config.agent.id,
                    started_at: &row.started_at,
                    finished_at: row.finished_at.as_deref(),
                    status: &row.status,
                    stdout: &row.stdout,
                    stderr: &row.stderr,
                    exit_status: row.exit_status,
                    step: &row.step,
                    version: row.version.as_deref(),
                    operation: crate::state::INSTALLER_OPERATION_INSTALL,
                    method: row.method.as_deref(),
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
    Ok(AgentInstallResponse {
        outcome: outcome_label,
        path,
        sha256,
    })
}

pub(crate) fn open_agent_env(config: &Config) -> Result<std::collections::HashMap<String, String>> {
    Ok(open_agent_environment(config)?.env)
}

pub(crate) fn open_agent_environment(config: &Config) -> Result<ResolvedAgentEnvironment> {
    if let Some(environment) = resolve_agent_environment_without_secrets(config) {
        return Ok(environment);
    }
    let home = home_dir()?;
    let store = SecretStore::open(&home)?;
    resolve_agent_environment(config, &store)
}

async fn load_fresh_config_for_target(
    state: &AppState,
    target_id: &str,
) -> Result<(Config, AgentTargetRuntime)> {
    let mut config = state.refresh_array_runtime_from_disk().await?;
    let target = state.agent_target(target_id)?;
    let live_agent = target.live_agent_config.lock().await.clone();
    let Some(target_config) = config.array.target_mut(target_id) else {
        return Err(StackError::InvalidParam {
            field: "target",
            reason: format!("unknown Array target `{target_id}`"),
        });
    };
    if target_config.agent.id == live_agent.id && target_config.agent.adapter.is_none() {
        target_config.agent.adapter = live_agent.adapter;
    }
    config.agent = target_config.agent.clone();
    Ok((config, target))
}

/// Resolve every configured `[mcp.servers]` entry into the SDK `McpServer`
/// type. Returns an empty Vec when no MCP servers are configured, so the
/// secret store is only opened when there's something to resolve.
pub(super) fn open_mcp_servers(
    config: &Config,
) -> Result<Vec<agent_client_protocol::schema::v1::McpServer>> {
    if config.mcp.servers.is_empty() {
        return Ok(Vec::new());
    }
    let home = home_dir()?;
    let store = SecretStore::open(&home)?;
    crate::runtime::agent::mcp::resolve_mcp_servers(&config.mcp, &store)
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
    let target_id = state.default_target_id().await?;
    start_agent_target(&state, &target_id).await
}

pub(crate) async fn array_agent_start_handler(
    State(state): State<AppState>,
    Path(target_id): Path<String>,
) -> std::result::Result<ApiSuccess<AgentStartResponse>, StackError> {
    start_agent_target(&state, &target_id).await
}

async fn start_agent_target(
    state: &AppState,
    target_id: &str,
) -> std::result::Result<ApiSuccess<AgentStartResponse>, StackError> {
    let _mutation = state.lock_agent_config_mutation().await?;
    // Re-read disk config and resolve env BEFORE invoking the supervisor so
    // `acps agent set` changes made while the daemon is running are honored
    // by the next start. open_agent_env enforces the same allowlist semantics
    // (security.md:91) regardless of caller.
    let (config, target) = load_fresh_config_for_target(state, target_id).await?;
    ensure_array_process_start_allowed(&config, target_id)?;
    let environment = open_agent_environment(&config)?;
    let capabilities = target
        .supervisor
        .start(AgentStartRequest {
            target_id: &target.target_id,
            agent: &config.agent,
            workspace_root: &config.workspace.root,
            env: environment.env,
            providers: environment.providers,
            state: &state.state,
            session_changes: &state.session_changes,
            event_hub: state.event_hub.clone(),
            permissions: Some(state.permissions.clone()),
            sandbox: config.workspace.sandbox.clone(),
        })
        .await?;
    {
        let mut live = target.live_agent_config.lock().await;
        *live = config.agent.clone();
    }
    let started_at = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
    let pid = target.supervisor.snapshot().await.pid;
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
    let target_id = state.default_target_id().await?;
    stop_agent_target(&state, &target_id).await
}

pub(crate) async fn array_agent_stop_handler(
    State(state): State<AppState>,
    Path(target_id): Path<String>,
) -> std::result::Result<ApiSuccess<AgentStopResponse>, StackError> {
    stop_agent_target(&state, &target_id).await
}

async fn stop_agent_target(
    state: &AppState,
    target_id: &str,
) -> std::result::Result<ApiSuccess<AgentStopResponse>, StackError> {
    let _mutation = state.lock_agent_config_mutation().await?;
    state.refresh_array_runtime_from_disk().await?;
    let target = state.agent_target(target_id)?;
    cancel_pending_acp_permissions_for_target(state, target_id, "agent-stopped").await;
    let exit_status = target
        .supervisor
        .stop(&target.target_id, &state.state, &state.event_hub)
        .await?;
    cancel_pending_acp_permissions_for_target(state, target_id, "agent-stopped").await;
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

#[derive(Debug, Default, Deserialize)]
pub(crate) struct AgentRestartQuery {
    #[serde(default)]
    require_idle: bool,
    #[serde(default)]
    auto: bool,
}

#[derive(Serialize)]
#[serde(untagged)]
pub(crate) enum AgentRestartResultResponse {
    Restarted(AgentRestartResponse),
    Blocked(AgentRestartBlockedResponse),
    Queued(AgentRestartQueuedResponse),
}

#[derive(Serialize)]
pub(crate) struct AgentRestartBlockedResponse {
    restarted: bool,
    target_id: String,
    blockers: Vec<AgentRestartBlockerResponse>,
}

#[derive(Serialize)]
pub(crate) struct AgentRestartQueuedResponse {
    queued: bool,
    already_queued: bool,
    target_id: String,
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
/// This endpoint also refreshes the daemon's live agent cache so
/// status, capabilities, and subsequent session creation observe the
/// same `[agent]` block used to spawn the supervised process.
pub(crate) async fn agent_restart_handler(
    State(state): State<AppState>,
    Query(query): Query<AgentRestartQuery>,
) -> std::result::Result<ApiSuccess<AgentRestartResultResponse>, StackError> {
    let target_id = state.default_target_id().await?;
    if query.auto {
        return queue_agent_restart(&state, target_id).await;
    }
    restart_agent_target(&state, &target_id, query.require_idle).await
}

#[derive(Serialize)]
pub(crate) struct AgentRestartBlockersResponse {
    target_id: String,
    blockers: Vec<AgentRestartBlockerResponse>,
}

#[derive(Serialize)]
struct AgentRestartBlockerResponse {
    session_id: String,
    target_id: String,
    state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_stop_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    permission_id: Option<String>,
}

pub(crate) async fn agent_restart_blockers_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<AgentRestartBlockersResponse>, StackError> {
    let target_id = state.default_target_id().await?;
    restart_blockers_for_target(&state, &target_id).await
}

async fn restart_blockers_for_target(
    state: &AppState,
    target_id: &str,
) -> std::result::Result<ApiSuccess<AgentRestartBlockersResponse>, StackError> {
    state.refresh_array_runtime_from_disk().await?;
    let blockers = {
        let store = state.state.lock().await;
        store.query_restart_blockers(Some(target_id))?
    }
    .into_iter()
    .map(AgentRestartBlockerResponse::from)
    .collect();
    Ok(ApiSuccess::new(AgentRestartBlockersResponse {
        target_id: target_id.to_owned(),
        blockers,
    }))
}

pub(crate) async fn array_agent_restart_handler(
    State(state): State<AppState>,
    Path(target_id): Path<String>,
    Query(query): Query<AgentRestartQuery>,
) -> std::result::Result<ApiSuccess<AgentRestartResultResponse>, StackError> {
    if query.auto {
        return queue_agent_restart(&state, target_id).await;
    }
    restart_agent_target(&state, &target_id, query.require_idle).await
}

const AGENT_RESTART_AUTO_POLL_INTERVAL: Duration = Duration::from_secs(2);

async fn queue_agent_restart(
    state: &AppState,
    target_id: String,
) -> std::result::Result<ApiSuccess<AgentRestartResultResponse>, StackError> {
    let fresh_config = state.refresh_array_runtime_from_disk().await?;
    ensure_array_process_start_allowed(&fresh_config, &target_id)?;
    state.agent_target(&target_id)?;
    let already_queued = state
        .queued_agent_restarts
        .insert(target_id.clone(), ())
        .is_some();
    if !already_queued {
        let state = state.clone();
        let target_id_for_task = target_id.clone();
        tokio::spawn(async move {
            queued_agent_restart_worker(state, target_id_for_task).await;
        });
    }
    Ok(ApiSuccess::new(AgentRestartResultResponse::Queued(
        AgentRestartQueuedResponse {
            queued: true,
            already_queued,
            target_id,
        },
    )))
}

async fn queued_agent_restart_worker(state: AppState, target_id: String) {
    loop {
        match restart_agent_target(&state, &target_id, true).await {
            Ok(ApiSuccess {
                data: AgentRestartResultResponse::Blocked(_),
                ..
            }) => {
                tokio::time::sleep(AGENT_RESTART_AUTO_POLL_INTERVAL).await;
            }
            Ok(_) => {
                state.queued_agent_restarts.remove(&target_id);
                return;
            }
            Err(err) => {
                state.queued_agent_restarts.remove(&target_id);
                tracing::warn!(
                    error = %err,
                    target_id,
                    "queued agent restart failed"
                );
                return;
            }
        }
    }
}

async fn restart_agent_target(
    state: &AppState,
    target_id: &str,
    require_idle: bool,
) -> std::result::Result<ApiSuccess<AgentRestartResultResponse>, StackError> {
    let _mutation = state.lock_agent_config_mutation().await?;
    // Load + validate the fresh on-disk config AND resolve env BEFORE
    // stopping the currently running agent. A malformed config or a
    // missing required secret should fail this call cleanly and leave
    // the running agent alone, rather than taking it down and
    // returning an error with no agent running at all.
    let (fresh_config, target) = load_fresh_config_for_target(state, target_id).await?;
    ensure_array_process_start_allowed(&fresh_config, target_id)?;
    let environment = open_agent_environment(&fresh_config)?;

    // Now safe to stop the prior process. `stop` returns
    // `Result<Option<i32>, _>`: outer `Err(AgentNotRunning)` means
    // there was nothing to stop (acceptable — a "restart" against a
    // stopped agent degenerates into a plain start); inner
    // `Option<i32>` is the optional exit status of the prior process.
    let prior_exit_status = if require_idle {
        match target
            .supervisor
            .stop_when_restart_safe(&target.target_id, &state.state, &state.event_hub)
            .await?
        {
            Ok(code) => {
                cancel_pending_acp_permissions_for_target(state, target_id, "agent-restarted")
                    .await;
                code
            }
            Err(blockers) => {
                return Ok(ApiSuccess::new(AgentRestartResultResponse::Blocked(
                    AgentRestartBlockedResponse {
                        restarted: false,
                        target_id: target_id.to_owned(),
                        blockers: blockers
                            .into_iter()
                            .map(AgentRestartBlockerResponse::from)
                            .collect(),
                    },
                )));
            }
        }
    } else {
        cancel_pending_acp_permissions_for_target(state, target_id, "agent-restarted").await;
        let code = match target
            .supervisor
            .stop(&target.target_id, &state.state, &state.event_hub)
            .await
        {
            Ok(code) => code,
            Err(StackError::AgentNotRunning) => None,
            Err(err) => return Err(err),
        };
        cancel_pending_acp_permissions_for_target(state, target_id, "agent-restarted").await;
        code
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
        let mut live = target.live_agent_config.lock().await;
        *live = fresh_config.agent.clone();
    }
    let capabilities = target
        .supervisor
        .start(AgentStartRequest {
            target_id: &target.target_id,
            agent: &fresh_config.agent,
            workspace_root: &fresh_config.workspace.root,
            env: environment.env,
            providers: environment.providers,
            state: &state.state,
            session_changes: &state.session_changes,
            event_hub: state.event_hub.clone(),
            permissions: Some(state.permissions.clone()),
            sandbox: fresh_config.workspace.sandbox.clone(),
        })
        .await?;
    let started_at = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
    let pid = target.supervisor.snapshot().await.pid;

    Ok(ApiSuccess::new(AgentRestartResultResponse::Restarted(
        AgentRestartResponse {
            stopped_at,
            started_at,
            prior_exit_status,
            capabilities,
            pid,
        },
    )))
}

impl From<crate::state::RestartBlockerRecord> for AgentRestartBlockerResponse {
    fn from(row: crate::state::RestartBlockerRecord) -> Self {
        Self {
            session_id: row.session_id,
            target_id: row.target_id,
            state: row.state,
            prompt_id: row.prompt_id,
            prompt_status: row.prompt_status,
            prompt_stop_reason: row.prompt_stop_reason,
            permission_id: row.permission_id,
        }
    }
}

pub(crate) async fn cancel_pending_acp_permissions_for_target(
    state: &AppState,
    target_id: &str,
    reason: &str,
) {
    let permission_ids_result = {
        let store = state.state.lock().await;
        store.query_pending_acp_permission_ids_for_target(target_id)
    };
    let permission_ids: Vec<String> = match permission_ids_result {
        Ok(rows) => rows,
        Err(err) => {
            tracing::warn!(error = %err, target_id, "failed to load pending ACP permissions before agent stop");
            return;
        }
    };
    for permission_id in permission_ids {
        if let Err(err) = state.permissions.cancel(&permission_id, reason).await {
            tracing::warn!(
                error = %err,
                permission_id,
                target_id,
                "failed to cancel pending ACP permission before agent stop",
            );
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct AgentSwitchRequest {
    agent: String,
    #[serde(default, rename = "drop")]
    drop_configs: bool,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    api_key_ref: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct AgentSwitchResponse {
    old_agent_id: String,
    agent_id: String,
    provider_status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_key_ref: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    required_env_refs: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    secret_migrations: Vec<AgentSwitchSecretMigrationJson>,
    install: AgentInstallResponse,
    restarted: bool,
    restart_started: bool,
    set_model: bool,
    models: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    follow_up: Option<&'static str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    provisioned: Vec<ProvisionedAgentConfigJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    skills_port: Option<SkillPortReport>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    cleaned_configs: Vec<CleanedAgentConfigJson>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    cleanup_errors: Vec<String>,
}

#[derive(Serialize)]
struct ProvisionedAgentConfigJson {
    label: &'static str,
    path: String,
}

#[derive(Serialize)]
struct CleanedAgentConfigJson {
    label: &'static str,
    path: String,
}

#[derive(Serialize)]
struct AgentSwitchSecretMigrationJson {
    from_ref: String,
    to_ref: String,
}

impl From<ProvisionedAgentConfig> for ProvisionedAgentConfigJson {
    fn from(value: ProvisionedAgentConfig) -> Self {
        Self {
            label: value.label,
            path: value.path.to_string_lossy().into_owned(),
        }
    }
}

impl From<CleanedAgentConfig> for CleanedAgentConfigJson {
    fn from(value: CleanedAgentConfig) -> Self {
        Self {
            label: value.label,
            path: value.path.to_string_lossy().into_owned(),
        }
    }
}

pub(crate) async fn agent_switch_handler(
    State(state): State<AppState>,
    Json(body): Json<AgentSwitchRequest>,
) -> std::result::Result<ApiSuccess<AgentSwitchResponse>, StackError> {
    let _mutation = state.lock_agent_config_mutation().await?;
    let home = home_dir()?;
    let fresh_config = Config::load_from_path(&state.runtime_paths.config_path)?;
    let registry = RegistryCatalog::load_with_override(
        &home.join(".config").join("acp-stack").join("agents.toml"),
    )?;
    if fresh_config.array.target(&body.agent).is_some() {
        return switch_to_existing_array_target(&state, &home, &registry, fresh_config, body).await;
    }
    let plan = plan_agent_switch(
        &fresh_config,
        &registry,
        PlannedAgentSwitchRequest {
            target_agent: body.agent.clone(),
            provider_id: body.provider.clone(),
            api_key_ref: body.api_key_ref.clone(),
        },
    )?;
    let target_entry = registry.lookup_required(&plan.target_agent_id)?;
    let mut candidate_config = plan.config.clone();
    candidate_config.agent.adapter = adapter_from_registry_entry(target_entry);
    rename_default_target_config(
        &mut candidate_config,
        &plan.target_agent_id,
        plan.config.agent.clone(),
    )?;

    let canonical = candidate_config.to_canonical_toml()?;
    let mut candidate_config = crate::config::load_config_from_str(&canonical)?;
    candidate_config.agent.adapter = adapter_from_registry_entry(target_entry);
    let secret_migrations = apply_switch_secret_migrations(&home, &plan.secret_migrations)?;
    let _env = open_agent_env(&candidate_config)?;

    let install = install_agent_for_config(&state, &candidate_config).await?;
    let provisioned =
        crate::runtime::agent::agent_headless_config::provision_agent_headless_config(
            &candidate_config,
            &home,
        )?
        .into_iter()
        .map(ProvisionedAgentConfigJson::from)
        .collect::<Vec<_>>();

    let models = if target_entry.set_model {
        let response = fetch_session_config_with_timeout(
            &home,
            &candidate_config,
            DEFAULT_MODELS_DISCOVERY_TIMEOUT,
        )
        .await?;
        advertised_values_for_category(&response, AgentSessionConfigCategory::Model)?
    } else {
        Vec::new()
    };
    let skills_port = port_agent_skills(
        &home,
        &registry,
        &fresh_config.agent.id,
        &candidate_config.agent.id,
    )?;

    let old_target_id = fresh_config.array.primary_target.clone();
    let old_target = state.agent_target(&old_target_id)?;
    let was_running = old_target.supervisor.snapshot().await.state.as_wire_str() == "running";
    // Rename sessions to the new primary target BEFORE writing the new config.
    // The rename can fail (e.g. a UNIQUE(target_id, agent_session_id) collision
    // is detected up front), and if it does the on-disk config must stay
    // untouched so config and DB never diverge.
    {
        let store = state.state.lock().await;
        store.rename_session_target_id(&old_target_id, &candidate_config.array.primary_target)?;
    }
    crate::fs_util::atomic_write_owner_only(
        &state.runtime_paths.config_path,
        canonical.as_bytes(),
    )?;
    state.refresh_array_runtime_from_disk().await?;
    let restart_started = apply_switch_runtime(
        &state,
        &old_target_id,
        &candidate_config.array.primary_target,
        &candidate_config,
        was_running,
    )
    .await?;
    let (cleaned_configs, cleanup_errors) = if body.drop_configs {
        match cleanup_agent_headless_config(&fresh_config, &home) {
            Ok(cleaned) => (
                cleaned
                    .into_iter()
                    .map(CleanedAgentConfigJson::from)
                    .collect(),
                Vec::new(),
            ),
            Err(err) => {
                tracing::warn!(error = %err, "source agent config cleanup failed after switch");
                (Vec::new(), vec![err.to_string()])
            }
        }
    } else {
        (Vec::new(), Vec::new())
    };

    let response = AgentSwitchResponse {
        old_agent_id: plan.old_agent_id,
        agent_id: plan.target_agent_id,
        provider_status: plan.provider_status.label(),
        provider: plan.provider_status.provider_id().map(str::to_owned),
        api_key_ref: plan.provider_status.api_key_ref().map(str::to_owned),
        required_env_refs: plan.required_env_refs,
        secret_migrations,
        install,
        restarted: was_running,
        restart_started,
        set_model: target_entry.set_model,
        models,
        follow_up: target_entry
            .set_model
            .then_some("acps agent set --model <model-id>"),
        provisioned,
        skills_port,
        cleaned_configs,
        cleanup_errors,
    };
    Ok(ApiSuccess::new(response))
}

async fn switch_to_existing_array_target(
    state: &AppState,
    home: &std::path::Path,
    registry: &RegistryCatalog,
    fresh_config: Config,
    body: AgentSwitchRequest,
) -> std::result::Result<ApiSuccess<AgentSwitchResponse>, StackError> {
    if body.provider.is_some() || body.api_key_ref.is_some() {
        return Err(StackError::InvalidParam {
            field: "provider",
            reason: "provider flags are ignored when switching to an existing Array target; use `acps array set --target ...` first".to_owned(),
        });
    }
    if body.drop_configs {
        return Err(StackError::InvalidParam {
            field: "drop",
            reason: "--drop is not supported when selecting an existing Array target".to_owned(),
        });
    }
    if fresh_config.array.primary_target == body.agent {
        return Err(StackError::InvalidParam {
            field: "agent",
            reason: format!("agent `{}` is already the default target", body.agent),
        });
    }
    let target_agent = fresh_config
        .array
        .target(&body.agent)
        .ok_or_else(|| StackError::InvalidParam {
            field: "agent",
            reason: format!("unknown Array target `{}`", body.agent),
        })?
        .agent
        .clone();
    let target_entry = registry.lookup_required(&target_agent.id)?;
    let mut candidate_config = fresh_config.clone();
    candidate_config.array.primary_target = body.agent.clone();
    candidate_config.agent = target_agent;
    let canonical = candidate_config.to_canonical_toml()?;
    let mut candidate_config = crate::config::load_config_from_str(&canonical)?;
    candidate_config.agent.adapter = adapter_from_registry_entry(target_entry);
    let _env = open_agent_env(&candidate_config)?;
    let required_env_refs = candidate_config.agent.env.clone();

    let install = install_agent_for_config(state, &candidate_config).await?;
    let provisioned =
        crate::runtime::agent::agent_headless_config::provision_agent_headless_config(
            &candidate_config,
            home,
        )?
        .into_iter()
        .map(ProvisionedAgentConfigJson::from)
        .collect::<Vec<_>>();
    let skills_port = port_agent_skills(
        home,
        registry,
        &fresh_config.agent.id,
        &candidate_config.agent.id,
    )?;

    let old_target_id = fresh_config.array.primary_target.clone();
    let old_target = state.agent_target(&old_target_id)?;
    let was_running = old_target.supervisor.snapshot().await.state.as_wire_str() == "running";
    crate::fs_util::atomic_write_owner_only(
        &state.runtime_paths.config_path,
        canonical.as_bytes(),
    )?;
    state.refresh_array_runtime_from_disk().await?;
    let restart_started = apply_switch_runtime(
        state,
        &old_target_id,
        &candidate_config.array.primary_target,
        &candidate_config,
        was_running,
    )
    .await?;

    Ok(ApiSuccess::new(AgentSwitchResponse {
        old_agent_id: fresh_config.agent.id,
        agent_id: candidate_config.agent.id,
        provider_status: "selected",
        provider: candidate_config
            .agent
            .provider
            .as_ref()
            .map(|provider| provider.id.clone()),
        api_key_ref: candidate_config
            .agent
            .provider
            .as_ref()
            .and_then(|provider| provider.api_key_ref.clone()),
        required_env_refs,
        secret_migrations: Vec::new(),
        install,
        restarted: was_running,
        restart_started,
        set_model: false,
        models: Vec::new(),
        follow_up: None,
        provisioned,
        skills_port,
        cleaned_configs: Vec::new(),
        cleanup_errors: Vec::new(),
    }))
}

fn rename_default_target_config(
    config: &mut Config,
    target_id: &str,
    agent: crate::config::AgentConfig,
) -> Result<()> {
    let old_primary = config.array.primary_target.clone();
    let target = config
        .array
        .target_mut(&old_primary)
        .ok_or_else(|| StackError::InvalidParam {
            field: "array.primary_target",
            reason: "must reference an entry in array.targets".to_owned(),
        })?;
    target.id = target_id.to_owned();
    target.agent = agent.clone();
    config.array.primary_target = target_id.to_owned();
    config.agent = agent;
    Ok(())
}

async fn apply_switch_runtime(
    state: &AppState,
    old_target_id: &str,
    new_target_id: &str,
    config: &Config,
    was_running: bool,
) -> Result<bool> {
    let target = state.agent_target(new_target_id)?;
    {
        let mut live = target.live_agent_config.lock().await;
        *live = config.agent.clone();
    }
    if !was_running {
        return Ok(false);
    }
    if let Ok(old_target) = state.agent_target(old_target_id) {
        match old_target
            .supervisor
            .stop(&old_target.target_id, &state.state, &state.event_hub)
            .await
        {
            Ok(_) | Err(StackError::AgentNotRunning) => {}
            Err(err) => return Err(err),
        }
    }
    let target_state = target.supervisor.snapshot().await.state;
    if target_state.as_wire_str() != "stopped" {
        return Ok(false);
    }
    start_agent_with_config(state, &target, config).await?;
    Ok(true)
}

async fn start_agent_with_config(
    state: &AppState,
    target: &AgentTargetRuntime,
    config: &Config,
) -> Result<()> {
    let environment = open_agent_environment(config)?;
    target
        .supervisor
        .start(AgentStartRequest {
            target_id: &target.target_id,
            agent: &config.agent,
            workspace_root: &config.workspace.root,
            env: environment.env,
            providers: environment.providers,
            state: &state.state,
            session_changes: &state.session_changes,
            event_hub: state.event_hub.clone(),
            permissions: Some(state.permissions.clone()),
            sandbox: config.workspace.sandbox.clone(),
        })
        .await?;
    Ok(())
}

fn apply_switch_secret_migrations(
    home: &std::path::Path,
    migrations: &[AgentSwitchSecretMigration],
) -> Result<Vec<AgentSwitchSecretMigrationJson>> {
    if migrations.is_empty() {
        return Ok(Vec::new());
    }
    let mut store = SecretStore::open(home)?;
    let mut applied = Vec::with_capacity(migrations.len());
    for migration in migrations {
        let value = store.get(&migration.from_ref)?.to_owned();
        if !store.contains(&migration.to_ref) {
            store.set(&migration.to_ref, &value)?;
        }
        applied.push(AgentSwitchSecretMigrationJson {
            from_ref: migration.from_ref.clone(),
            to_ref: migration.to_ref.clone(),
        });
    }
    Ok(applied)
}

pub(crate) fn ensure_array_process_start_allowed(config: &Config, target_id: &str) -> Result<()> {
    if config.array.enabled || target_id == config.array.primary_target {
        return Ok(());
    }
    Err(StackError::InvalidParam {
        field: "target",
        reason: format!(
            "Array mode is off; only default target `{}` can be started",
            config.array.primary_target
        ),
    })
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
    let target_id = state.default_target_id().await?;
    capabilities_agent_target(&state, &target_id).await
}

pub(crate) async fn array_agent_capabilities_handler(
    State(state): State<AppState>,
    Path(target_id): Path<String>,
) -> std::result::Result<ApiSuccess<AgentCapabilitiesResponseBody>, StackError> {
    capabilities_agent_target(&state, &target_id).await
}

async fn capabilities_agent_target(
    state: &AppState,
    target_id: &str,
) -> std::result::Result<ApiSuccess<AgentCapabilitiesResponseBody>, StackError> {
    state.refresh_array_runtime_from_disk().await?;
    let target = state.agent_target(target_id)?;
    let agent = target.live_agent_config.lock().await.clone();
    let agent_id = agent.id.clone();
    let snapshot: AgentSnapshot = target.supervisor.snapshot().await;
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
        adapter: agent.adapter,
        captured_at: record.captured_at,
        capabilities,
        process_state: format!("{:?}", snapshot.state).to_lowercase(),
    }))
}
