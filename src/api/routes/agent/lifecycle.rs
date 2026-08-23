//! Agent process lifecycle: start, stop, and restart handlers.

use super::*;

#[derive(Serialize, schemars::JsonSchema)]
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
    start_agent_target_locked(state, target_id).await
}

/// Bring the configured agent up for a request that needs the ACP bridge.
///
/// Nothing else starts the agent after `acps init`: the process manager that
/// owns `acps serve` does not own the agent subprocess, so without this a
/// freshly provisioned host answers every session call with
/// `agent.not_running` until an operator calls `POST /v1/agent/start`. Same
/// path recovers from a crash, because the exit monitor leaves the supervisor
/// in `Stopped`.
///
/// A genuinely misconfigured agent still fails: this only spawns, it does not
/// soften the config, secret, or initialize errors `start` propagates.
/// `restart = "never"` opts a target out entirely — an operator who forbade
/// crash restarts does not want request traffic spawning agents either.
pub(crate) async fn ensure_agent_started(state: &AppState, target_id: &str) -> Result<()> {
    let target = state.agent_target(target_id)?;
    match target.supervisor.await_start_readiness().await {
        AgentStartReadiness::Running => return Ok(()),
        AgentStartReadiness::NeedsStart => {}
        AgentStartReadiness::Unavailable => return Err(StackError::AgentNotRunning),
    }
    // The live cache, not a fresh disk read: this is the per-request hot path,
    // and the policy that matters is the one this daemon is running under.
    if target.live_agent_config.lock().await.restart == AGENT_RESTART_NEVER {
        return Err(StackError::AgentNotRunning);
    }
    let _mutation = state.lock_agent_config_mutation().await?;
    // Re-check under the config-mutation lock: an explicit start or restart
    // may have won the race while we waited for it.
    if target.supervisor.is_running().await {
        return Ok(());
    }
    match start_agent_target_locked(state, target_id).await {
        Ok(_) => Ok(()),
        // Lost the Stopped -> Starting race to a spawn that does not take
        // the config-mutation lock — the crash-exit monitor's restart. A live
        // bridge is not guaranteed yet: returning Ok here would let the
        // caller's next `bridge()` call 409 while the winner is still mid-
        // initialize. Wait that spawn out; only `Running` means success.
        Err(StackError::AgentAlreadyRunning) => {
            match target.supervisor.await_start_readiness().await {
                AgentStartReadiness::Running => Ok(()),
                // The winning spawn failed (state rolled back to Stopped) or
                // never settled; the next request can try again.
                AgentStartReadiness::NeedsStart | AgentStartReadiness::Unavailable => {
                    Err(StackError::AgentNotRunning)
                }
            }
        }
        Err(err) => Err(err),
    }
}

/// Inner half of `start_agent_target`, for callers already holding the
/// agent-config mutation lock.
async fn start_agent_target_locked(
    state: &AppState,
    target_id: &str,
) -> std::result::Result<ApiSuccess<AgentStartResponse>, StackError> {
    // Re-read disk config and resolve env BEFORE invoking the supervisor so
    // `acps agent set` changes made while the daemon is running are honored
    // by the next start. open_agent_env enforces the same allowlist semantics
    // (security.md:49) regardless of caller.
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
            network_provider: crate::extensions::resolve_network_provider(&config),
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

#[derive(Serialize, schemars::JsonSchema)]
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

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct AgentRestartResponse {
    stopped_at: String,
    started_at: String,
    /// Exit status of the prior process. `None` when the supervisor
    /// was not running (the restart degenerated into a plain start).
    prior_exit_status: Option<i32>,
    capabilities: AgentCapabilitiesDto,
    pid: Option<u32>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub(crate) struct AgentRestartQuery {
    #[serde(default)]
    require_idle: bool,
    #[serde(default)]
    auto: bool,
}

#[derive(Serialize, schemars::JsonSchema)]
#[serde(untagged)]
pub(crate) enum AgentRestartResultResponse {
    Restarted(AgentRestartResponse),
    Blocked(AgentRestartBlockedResponse),
    Queued(AgentRestartQueuedResponse),
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct AgentRestartBlockedResponse {
    restarted: bool,
    target_id: String,
    blockers: Vec<AgentRestartBlockerResponse>,
}

#[derive(Serialize, schemars::JsonSchema)]
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

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct AgentRestartBlockersResponse {
    target_id: String,
    blockers: Vec<AgentRestartBlockerResponse>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct AgentRestartBlockerResponse {
    session_id: String,
    target_id: String,
    #[schemars(extend("enum" = ["prompt_sent", "working", "permission_required", "blocked"]))]
    state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_id: Option<String>,
    #[schemars(extend("enum" = ["pending", "running", null]))]
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
            network_provider: crate::extensions::resolve_network_provider(&fresh_config),
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
