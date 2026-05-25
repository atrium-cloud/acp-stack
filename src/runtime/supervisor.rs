//! Runtime supervisor lifecycle hooks.
//!
//! Two units live here:
//!
//! 1. [`ServerLifecycle`] records the daemon's own start/stop transitions
//!    (`server.starting`, `server.started`, `server.stopped`) into
//!    `agent_lifecycle`. One per `acps serve` invocation.
//!
//! 2. [`AgentSupervisor`] owns the spawned ACP agent's lifecycle: it spawns
//!    the agent through [`AcpBridge`], persists capabilities, records the
//!    `agent.*` lifecycle events, and tears the agent down on stop or on
//!    daemon shutdown. One per running daemon.
//!
//! State machine for the agent supervisor:
//!
//! ```text
//! Stopped --start()--> Starting --(initialize succeeds)--> Running
//!                          \--(initialize fails)----------> Stopped
//! Running --stop()---> Stopping --(child reaped)--> Stopped
//! ```
//!
//! `Starting` exists so that two concurrent `POST /v1/agent/start` requests
//! during a slow initialize cannot both spawn an agent — the second one
//! sees `Starting` and returns `agent.already_running`.
//!
//! `record_*` helpers come in two flavors: sync (`&StateStore`) for use before
//! the store is moved into `AppState`, and async (`&Arc<Mutex<StateStore>>`)
//! for use after, where a brief lock acquires the connection.

use std::collections::HashMap;
use std::mem;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Maximum wall time `cancel_all_prompts` will spend awaiting in-flight prompt
/// tasks during agent shutdown. We cancel first, then drain; if a task ignores
/// cancellation for longer than this we detach and let the bridge teardown
/// finish closing the connection, which surfaces the error to the task on its
/// next attempted ACP send.
const PROMPT_DRAIN_BUDGET: Duration = Duration::from_secs(5);

use agent_client_protocol::schema::{
    ContentBlock, McpServer, PromptRequest, SessionId as AcpSessionId, StopReason,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex as TokioMutex, RwLock};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::acp_bridge::{
    AcpBridge, AgentCapabilitiesDto, AgentSessionConfigCategory, AgentSessionModelSelection,
    SessionEventSink, StateStoreSessionSink, resolve_command_path, session_config_id_for_value,
    session_model_selection_for_value,
};
use crate::config::AgentConfig;
use crate::error::{Result, StackError};
use crate::events::EventHub;
use crate::secrets::SecretStore;
use crate::state::{
    NewPromptRecord, NewSessionRecord, PromptRecord, PromptStatus, SessionRecord, StateStore,
    next_prompt_id,
};

pub struct ServerLifecycle {
    started_at: Instant,
}

impl ServerLifecycle {
    /// Record `server.starting` while the store is still a direct handle, then
    /// hand back a lifecycle handle that tracks elapsed wall time for the
    /// `server.stopped` payload. No `status` topic fan-out here because the
    /// event hub is constructed inside `AppState::with_effective_bind`, which
    /// has not run yet at this point — and a subscriber cannot exist before
    /// the listener accepts its first connection anyway.
    pub fn starting(state: &StateStore, bind: &str) -> Result<Self> {
        let payload = json!({ "bind": bind }).to_string();
        state.append_agent_lifecycle("server.starting", "acps serve starting", &payload)?;
        Ok(Self {
            started_at: Instant::now(),
        })
    }

    /// Record `server.started` after the listener is bound. Async-aware so the
    /// caller can hold the same `Arc<Mutex<StateStore>>` it later hands to
    /// axum handlers. Publishes the row to the `status` topic.
    pub async fn started(
        &self,
        state: &Arc<TokioMutex<StateStore>>,
        event_hub: &EventHub,
        bind: &str,
    ) -> Result<()> {
        let data = json!({ "bind": bind });
        let payload = data.to_string();
        let guard = state.lock().await;
        let row =
            guard.append_agent_lifecycle("server.started", "acps serve listening", &payload)?;
        drop(guard);
        event_hub.publish_status_event(&row.id, &row.created_at, "server.started", data);
        Ok(())
    }

    /// Record `server.stopped` with elapsed wall time. Called from the shutdown
    /// arm after axum's graceful-shutdown future resolves.
    pub async fn stopped(
        &self,
        state: &Arc<TokioMutex<StateStore>>,
        event_hub: &EventHub,
        reason: &str,
    ) -> Result<()> {
        let elapsed_ms = u64::try_from(self.started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
        let data = json!({ "reason": reason, "elapsed_ms": elapsed_ms });
        let payload = data.to_string();
        let guard = state.lock().await;
        let row = guard.append_agent_lifecycle("server.stopped", "acps serve stopped", &payload)?;
        drop(guard);
        event_hub.publish_status_event(&row.id, &row.created_at, "server.stopped", data);
        Ok(())
    }
}

/// In-memory state machine for the active ACP agent. `Running` owns the live
/// `AcpBridge` behind an `Arc` so session dispatchers can clone the handle
/// out of the state mutex and call into the bridge without holding the lock
/// across `await` (which would block all other supervisor operations,
/// including status snapshots, for the duration of every prompt).
enum AgentState {
    Stopped,
    Starting,
    Running(Arc<AcpBridge>),
    Stopping,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStateLabel {
    Stopped,
    Starting,
    Running,
    Stopping,
}

impl AgentStateLabel {
    fn from_state(state: &AgentState) -> Self {
        match state {
            AgentState::Stopped => Self::Stopped,
            AgentState::Starting => Self::Starting,
            AgentState::Running(_) => Self::Running,
            AgentState::Stopping => Self::Stopping,
        }
    }

    /// Canonical snake_case wire label. Matches the `#[serde(rename_all =
    /// "snake_case")]` annotation on this enum; carved out as a method so
    /// status/health handlers don't fall back to `format!("{:?}", ...)`,
    /// which would silently break for any multi-word variant added later.
    pub fn as_wire_str(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Stopping => "stopping",
        }
    }
}

/// Public snapshot of the supervisor: status handlers read this without
/// touching the bridge mutex.
#[derive(Debug, Clone)]
pub struct AgentSnapshot {
    pub state: AgentStateLabel,
    pub latest_capabilities: Option<AgentCapabilitiesDto>,
    pub pid: Option<u32>,
}

/// Per-prompt cancellation + join handle. We keep both so a `session/cancel`
/// can fire the token (asks the future to settle as cancelled) and so a slow
/// agent-shutdown path can also reap the background task.
struct PromptHandle {
    cancel: CancellationToken,
    join: JoinHandle<()>,
    session_id: String,
}

/// Owner of the single configured ACP agent's lifecycle.
///
/// Construction is cheap (no IO). The agent process is spawned on `start()`
/// and reaped on `stop()` (or `shutdown_on_serve_exit()` during daemon
/// shutdown). Methods are async because handlers may await across the
/// initialize handshake while holding state.
pub struct AgentSupervisor {
    state: TokioMutex<AgentState>,
    capabilities: RwLock<Option<AgentCapabilitiesDto>>,
    last_pid: RwLock<Option<u32>>,
    /// In-flight prompt registry. Each entry is a fire-and-forget background
    /// task plus its cancellation token. We never block on these from
    /// session-tier handlers — the durable `prompts` row is the source of
    /// truth for clients polling status.
    prompts: TokioMutex<HashMap<String, PromptHandle>>,
}

impl Default for AgentSupervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentSupervisor {
    pub fn new() -> Self {
        Self {
            state: TokioMutex::new(AgentState::Stopped),
            capabilities: RwLock::new(None),
            last_pid: RwLock::new(None),
            prompts: TokioMutex::new(HashMap::new()),
        }
    }

    /// Snapshot of the running bridge for session dispatchers. Returns
    /// `AgentNotRunning` when stopped/starting/stopping — handlers must
    /// surface that as the configured envelope error.
    async fn bridge(&self) -> Result<Arc<AcpBridge>> {
        let guard = self.state.lock().await;
        match &*guard {
            AgentState::Running(bridge) => Ok(Arc::clone(bridge)),
            _ => Err(StackError::AgentNotRunning),
        }
    }

    /// Spawn the configured agent and run the ACP `initialize` handshake.
    ///
    /// The caller is responsible for resolving `[agent].env` (via the secret
    /// store) and passing the resulting name->value map; that keeps the
    /// supervisor independent of secrets infrastructure and avoids opening
    /// the secret store in code paths whose `agent.env` is empty. `cwd`
    /// falls back to `workspace.root` per `docs/specs/acp/acp-bridge.md:15`.
    ///
    /// On success, records `agent.started` and an UPSERT into
    /// `agent_capabilities`. On failure, transitions back to `Stopped` so a
    /// retry can succeed without an intervening `stop`.
    pub async fn start(
        &self,
        agent: &AgentConfig,
        workspace_root: &str,
        env: HashMap<String, String>,
        state: &Arc<TokioMutex<StateStore>>,
        event_hub: EventHub,
        permissions: Option<crate::permissions::PermissionService>,
    ) -> Result<AgentCapabilitiesDto> {
        // First lock: atomically transition Stopped -> Starting. Refusing
        // any other start under the same lock prevents concurrent spawns.
        {
            let mut guard = self.state.lock().await;
            match &*guard {
                AgentState::Stopped => {
                    *guard = AgentState::Starting;
                }
                AgentState::Starting | AgentState::Running(_) | AgentState::Stopping => {
                    return Err(StackError::AgentAlreadyRunning);
                }
            }
        }

        match self
            .do_start(agent, workspace_root, env, state, event_hub, permissions)
            .await
        {
            Ok((capabilities, bridge)) => {
                {
                    let mut guard = self.state.lock().await;
                    *guard = AgentState::Running(Arc::new(bridge));
                }
                *self.capabilities.write().await = Some(capabilities.clone());
                Ok(capabilities)
            }
            Err(err) => {
                // Roll back to Stopped unconditionally so the next start
                // can proceed. `do_start` is responsible for tearing down
                // any partially-spawned bridge before returning.
                {
                    let mut guard = self.state.lock().await;
                    *guard = AgentState::Stopped;
                }
                *self.last_pid.write().await = None;
                Err(err)
            }
        }
    }

    /// Inner half of `start`: between the `Starting` and `Running` state
    /// transitions. On any error, MUST shut down any bridge it spawned so
    /// the caller's rollback only needs to flip state.
    async fn do_start(
        &self,
        agent: &AgentConfig,
        workspace_root: &str,
        env: HashMap<String, String>,
        state: &Arc<TokioMutex<StateStore>>,
        event_hub: EventHub,
        permissions: Option<crate::permissions::PermissionService>,
    ) -> Result<(AgentCapabilitiesDto, AcpBridge)> {
        let cwd = resolve_agent_cwd(agent, workspace_root);

        // Enforce the optional integrity guard BEFORE spawning. The installer
        // already hashes `[agent.install].creates`, but `[agent].command` may
        // resolve to a different binary (different path, or replaced on
        // disk between install and start). If `expected_sha256` is
        // configured, hash the resolved command — relative to the same cwd
        // that will be used for spawn — and refuse to start on mismatch.
        if let Some(expected) = agent.expected_sha256.as_deref() {
            verify_agent_binary_sha256(&agent.command, &cwd, expected)?;
        }

        let starting_data = json!({
            "agent_id": agent.id,
            "command": agent.command,
            "adapter": agent.adapter,
        });
        let starting_payload = starting_data.to_string();
        let starting_row = {
            let guard = state.lock().await;
            guard.append_agent_lifecycle(
                "agent.starting",
                "starting acp agent",
                &starting_payload,
            )?
        };
        event_hub.publish_agent_event(
            &starting_row.id,
            &starting_row.created_at,
            "agent.starting",
            starting_data,
        );

        let sink: Arc<dyn SessionEventSink> =
            Arc::new(StateStoreSessionSink::new(state.clone(), event_hub.clone()));
        let bridge = match AcpBridge::spawn(agent, env, cwd, sink, permissions).await {
            Ok(bridge) => bridge,
            Err(err) => {
                let failure_data = json!({
                    "agent_id": agent.id,
                    "reason": err.to_string(),
                });
                let failure_payload = failure_data.to_string();
                let row_result = {
                    let guard = state.lock().await;
                    guard.append_agent_lifecycle(
                        "agent.spawn_failed",
                        "agent spawn failed",
                        &failure_payload,
                    )
                };
                if let Ok(row) = row_result {
                    event_hub.publish_agent_event(
                        &row.id,
                        &row.created_at,
                        "agent.spawn_failed",
                        failure_data,
                    );
                }
                return Err(err);
            }
        };

        let capabilities = bridge.capabilities().clone();
        let pid = bridge.pid();
        let caps_json = capabilities.to_json()?;

        // Persist capabilities and the started event AFTER the bridge is
        // live. If any write fails, shut the bridge down before returning
        // so a failed start never leaks the child.
        let started_data = json!({
            "agent_id": agent.id,
            "pid": pid,
            "adapter": agent.adapter,
        });
        let started_row_result: Result<crate::state::AgentLifecycleEvent> = {
            let guard = state.lock().await;
            (|| {
                guard.upsert_agent_capabilities(&agent.id, &caps_json)?;
                let started_payload = started_data.to_string();
                let row = guard.append_agent_lifecycle(
                    "agent.started",
                    "agent initialized",
                    &started_payload,
                )?;
                Ok(row)
            })()
        };
        let persist_result: Result<()> = match started_row_result {
            Ok(row) => {
                event_hub.publish_agent_event(
                    &row.id,
                    &row.created_at,
                    "agent.started",
                    started_data,
                );
                Ok(())
            }
            Err(err) => Err(err),
        };

        if let Err(err) = persist_result {
            if let Err(shutdown_err) = bridge.shutdown().await {
                tracing::warn!(
                    error = %shutdown_err,
                    "agent bridge shutdown after persist failure also failed"
                );
            }
            return Err(err);
        }

        *self.last_pid.write().await = pid;
        Ok((capabilities, bridge))
    }

    /// Tear down the running agent. Returns the agent's exit status if
    /// available. Records `agent.stopped` regardless of clean exit.
    pub async fn stop(
        &self,
        state: &Arc<TokioMutex<StateStore>>,
        event_hub: &EventHub,
    ) -> Result<Option<i32>> {
        // Extract the bridge under the lock and mark Stopping so a parallel
        // start cannot race with our shutdown work.
        let bridge = {
            let mut guard = self.state.lock().await;
            match mem::replace(&mut *guard, AgentState::Stopping) {
                AgentState::Running(bridge) => bridge,
                other => {
                    // Restore whatever state we found so we don't accidentally
                    // leave the supervisor in `Stopping`.
                    *guard = other;
                    return Err(StackError::AgentNotRunning);
                }
            }
        };

        // Cancel every in-flight prompt before shutting the bridge down so
        // the background tasks settle with `status='cancelled'` instead of
        // an opaque `agent.request_failed` racing against the IO loop teardown.
        self.cancel_all_prompts().await;

        let started_at = Instant::now();
        let shutdown_result = bridge.shutdown().await;
        let elapsed_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);

        // Always transition to Stopped FIRST, even if shutdown or DB writes
        // fail. Without this an error here would leave the supervisor stuck
        // in `Stopping`, and future starts and stops would both refuse.
        {
            let mut guard = self.state.lock().await;
            *guard = AgentState::Stopped;
        }
        *self.last_pid.write().await = None;

        // Record the lifecycle row best-effort. A DB error is logged but
        // does not mask the original shutdown outcome — the supervisor is
        // already in a coherent state thanks to the transition above.
        let exit = shutdown_result?;
        let data = json!({
            "exit_status": exit,
            "elapsed_ms": elapsed_ms,
        });
        let payload = data.to_string();
        let row = {
            let guard = state.lock().await;
            guard.append_agent_lifecycle("agent.stopped", "agent stopped", &payload)
        };
        match row {
            Ok(row) => {
                event_hub.publish_agent_event(&row.id, &row.created_at, "agent.stopped", data);
            }
            Err(err) => {
                tracing::warn!(error = %err, "failed to record agent.stopped lifecycle row");
            }
        }

        Ok(exit)
    }

    /// Called from `acps serve` between the HTTP server returning and
    /// `ServerLifecycle::stopped`. Best-effort cleanup so we don't leak the
    /// agent process past the daemon. Errors are logged but never returned —
    /// the serve path must continue to record `server.stopped` even if the
    /// agent teardown was messy.
    pub async fn shutdown_on_serve_exit(
        &self,
        state: &Arc<TokioMutex<StateStore>>,
        event_hub: &EventHub,
    ) {
        // Determine whether there's anything to stop without holding the
        // lock across the entire shutdown sequence.
        let needs_stop = matches!(*self.state.lock().await, AgentState::Running(_));
        if !needs_stop {
            return;
        }
        if let Err(err) = self.stop(state, event_hub).await {
            tracing::warn!(error = %err, "agent supervisor: shutdown on serve exit failed");
        }
    }

    /// Snapshot the supervisor for status handlers.
    pub async fn snapshot(&self) -> AgentSnapshot {
        let state_label = {
            let guard = self.state.lock().await;
            AgentStateLabel::from_state(&guard)
        };
        let capabilities = self.capabilities.read().await.clone();
        let pid = *self.last_pid.read().await;
        AgentSnapshot {
            state: state_label,
            latest_capabilities: capabilities,
            pid,
        }
    }

    pub async fn is_running(&self) -> bool {
        matches!(*self.state.lock().await, AgentState::Running(_))
    }

    /// `POST /v1/sessions`. Dispatches ACP `session/new`, persists a new
    /// `sessions` row, and returns it. `cwd` defaults to `workspace.root`
    /// when the client omits it.
    pub async fn create_session(
        &self,
        agent: &AgentConfig,
        workspace_root: &str,
        cwd: Option<String>,
        mcp_servers: Vec<McpServer>,
        state: &Arc<TokioMutex<StateStore>>,
    ) -> Result<SessionRecord> {
        let bridge = self.bridge().await?;
        let resolved_cwd = cwd.unwrap_or_else(|| workspace_root.to_owned());
        let cwd_path = PathBuf::from(&resolved_cwd);
        let response = bridge.new_session(cwd_path, mcp_servers).await?;
        let session_id = response.session_id.0.to_string();
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
            match session_model_selection_for_value(&response, model)? {
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

        // Persist after the agent confirms. If we inserted first and the
        // agent rejected, we'd leave a phantom row. The agent's `session_id`
        // is authoritative; we mirror it into our `sessions` table.
        let record = NewSessionRecord {
            id: session_id.clone(),
            agent_id: agent.id.clone(),
            cwd: resolved_cwd,
            title: None,
            metadata_json: "{}".to_owned(),
        };
        let guard = state.lock().await;
        let inserted = guard.insert_session(record)?;
        guard.append_session_event(
            &session_id,
            "info",
            "session.created",
            "session created",
            &json!({ "agent_id": agent.id, "cwd": inserted.cwd }).to_string(),
        )?;
        Ok(inserted)
    }

    /// `POST /v1/sessions/{id}/load`. Capability-gated by the bridge.
    pub async fn load_session(
        &self,
        session_id: &str,
        cwd: Option<String>,
        mcp_servers: Vec<McpServer>,
        workspace_root: &str,
        state: &Arc<TokioMutex<StateStore>>,
    ) -> Result<SessionRecord> {
        let bridge = self.bridge().await?;
        let record = {
            let guard = state.lock().await;
            guard.get_session(session_id)?
        }
        .ok_or_else(|| StackError::SessionNotFound {
            id: session_id.to_owned(),
        })?;
        let resolved_cwd = cwd.unwrap_or_else(|| {
            if record.cwd.is_empty() {
                workspace_root.to_owned()
            } else {
                record.cwd.clone()
            }
        });
        bridge
            .load_session(
                AcpSessionId::new(session_id.to_owned()),
                PathBuf::from(&resolved_cwd),
                mcp_servers,
            )
            .await?;
        let guard = state.lock().await;
        guard.update_session_status(session_id, "active")?;
        guard.append_session_event(session_id, "info", "session.loaded", "session loaded", "{}")?;
        guard
            .get_session(session_id)?
            .ok_or_else(|| StackError::SessionNotFound {
                id: session_id.to_owned(),
            })
    }

    /// `POST /v1/sessions/{id}/resume`.
    pub async fn resume_session(
        &self,
        session_id: &str,
        cwd: Option<String>,
        mcp_servers: Vec<McpServer>,
        workspace_root: &str,
        state: &Arc<TokioMutex<StateStore>>,
    ) -> Result<SessionRecord> {
        let bridge = self.bridge().await?;
        // Confirm session exists before we hit the agent: returning 404 for
        // an unknown id beats letting the agent reject with an opaque error.
        let record = {
            let guard = state.lock().await;
            guard.get_session(session_id)?
        }
        .ok_or_else(|| StackError::SessionNotFound {
            id: session_id.to_owned(),
        })?;
        let resolved_cwd = cwd.unwrap_or_else(|| {
            if record.cwd.is_empty() {
                workspace_root.to_owned()
            } else {
                record.cwd.clone()
            }
        });
        bridge
            .resume_session(
                AcpSessionId::new(session_id.to_owned()),
                PathBuf::from(&resolved_cwd),
                mcp_servers,
            )
            .await?;
        let guard = state.lock().await;
        guard.update_session_status(session_id, "active")?;
        guard.append_session_event(
            session_id,
            "info",
            "session.resumed",
            "session resumed",
            "{}",
        )?;
        guard
            .get_session(session_id)?
            .ok_or_else(|| StackError::SessionNotFound {
                id: session_id.to_owned(),
            })
    }

    /// `DELETE /v1/sessions/{id}`. Closes the agent-side session and marks
    /// the local row `closed`.
    ///
    /// Order matters: send `session/close` to the agent first, and only on
    /// success cancel local in-flight prompts and mark the row closed.
    /// Otherwise a failed bridge call would leave the agent still running
    /// the session while we mark it closed locally.
    pub async fn close_session(
        &self,
        session_id: &str,
        state: &Arc<TokioMutex<StateStore>>,
    ) -> Result<SessionRecord> {
        let bridge = self.bridge().await?;
        {
            let guard = state.lock().await;
            if guard.get_session(session_id)?.is_none() {
                return Err(StackError::SessionNotFound {
                    id: session_id.to_owned(),
                });
            }
        }
        bridge
            .close_session(AcpSessionId::new(session_id.to_owned()))
            .await?;
        // Bridge confirmed the close — now it's safe to settle local state.
        self.cancel_prompts_for_session(session_id).await;
        let guard = state.lock().await;
        guard.update_session_status(session_id, "closed")?;
        guard.append_session_event(session_id, "info", "session.closed", "session closed", "{}")?;
        guard
            .get_session(session_id)?
            .ok_or_else(|| StackError::SessionNotFound {
                id: session_id.to_owned(),
            })
    }

    /// `POST /v1/sessions/{id}/prompt`. Fire-and-forget: inserts a row in
    /// `prompts` with status `pending`, spawns a background task that drives
    /// the ACP `session/prompt` to completion, and returns the prompt id
    /// immediately. Clients poll `GET /v1/sessions/{id}/prompts/{prompt_id}`
    /// (or session events) until the status transitions to a terminal one.
    pub async fn submit_prompt(
        &self,
        session_id: &str,
        prompt_blocks: Vec<ContentBlock>,
        prompt_json: String,
        state: &Arc<TokioMutex<StateStore>>,
    ) -> Result<PromptRecord> {
        let bridge = self.bridge().await?;
        {
            let guard = state.lock().await;
            let session =
                guard
                    .get_session(session_id)?
                    .ok_or_else(|| StackError::SessionNotFound {
                        id: session_id.to_owned(),
                    })?;
            if session.status == "closed" {
                return Err(StackError::SessionClosed {
                    id: session_id.to_owned(),
                });
            }
        }
        let prompt_id = next_prompt_id();
        let record = {
            let guard = state.lock().await;
            guard.insert_prompt(NewPromptRecord {
                id: prompt_id.clone(),
                session_id: session_id.to_owned(),
                prompt_json,
            })?
        };

        let cancel = CancellationToken::new();
        let cancel_inner = cancel.clone();
        let state_clone = state.clone();
        let session_id_owned = session_id.to_owned();
        let prompt_id_owned = prompt_id.clone();
        let acp_request =
            PromptRequest::new(AcpSessionId::new(session_id_owned.clone()), prompt_blocks);

        let join = tokio::spawn(async move {
            // Flip `pending -> running` so clients polling immediately after
            // submit see the task is live. If this write fails, log and
            // continue; the row is still in `pending` and the task will
            // overwrite with a terminal status on settle.
            {
                let guard = state_clone.lock().await;
                if let Err(err) = guard.update_prompt_status(
                    &prompt_id_owned,
                    PromptStatus::Running,
                    None,
                    None,
                    None,
                ) {
                    tracing::warn!(error = %err, prompt_id = %prompt_id_owned, "failed to mark prompt running");
                }
            }

            let bridge_call = bridge.prompt_session(acp_request);
            let outcome = tokio::select! {
                result = bridge_call => Outcome::Settled(result),
                _ = cancel_inner.cancelled() => Outcome::Cancelled,
            };

            let (status, stop_reason, error_code, error_message) = match outcome {
                Outcome::Settled(Ok(stop_reason)) => {
                    let stop_str = stop_reason_str(stop_reason);
                    let status = if stop_reason == StopReason::Cancelled {
                        PromptStatus::Cancelled
                    } else {
                        PromptStatus::Completed
                    };
                    (status, Some(stop_str), None, None)
                }
                Outcome::Settled(Err(err)) => {
                    let code = err.error_code().to_owned();
                    let msg = err.public_message();
                    (PromptStatus::Errored, None, Some(code), Some(msg))
                }
                Outcome::Cancelled => (
                    PromptStatus::Cancelled,
                    Some("cancelled".to_owned()),
                    None,
                    None,
                ),
            };

            let guard = state_clone.lock().await;
            if let Err(err) = guard.update_prompt_status(
                &prompt_id_owned,
                status,
                stop_reason.as_deref(),
                error_code.as_deref(),
                error_message.as_deref(),
            ) {
                tracing::warn!(
                    error = %err,
                    prompt_id = %prompt_id_owned,
                    "failed to record terminal prompt status"
                );
            }
        });

        self.prompts.lock().await.insert(
            prompt_id.clone(),
            PromptHandle {
                cancel,
                join,
                session_id: session_id.to_owned(),
            },
        );
        // Reap on a delay: every settled task removes its own entry from
        // the map via `reap_finished`. We don't spawn a watchdog; the next
        // submit/cancel call performs the cleanup pass cheaply.
        self.reap_finished().await;
        Ok(record)
    }

    /// `POST /v1/sessions/{id}/cancel`. Notifies the agent via ACP
    /// `session/cancel` first; only on success does the supervisor fire the
    /// local cancellation tokens. This ordering avoids the agent-disagrees
    /// race where a failed bridge call would leave prompt rows locally
    /// `cancelled` while the agent kept running the turn.
    pub async fn cancel_session(
        &self,
        session_id: &str,
        state: &Arc<TokioMutex<StateStore>>,
    ) -> Result<()> {
        let bridge = self.bridge().await?;
        {
            let guard = state.lock().await;
            if guard.get_session(session_id)?.is_none() {
                return Err(StackError::SessionNotFound {
                    id: session_id.to_owned(),
                });
            }
        }
        bridge
            .cancel_session(AcpSessionId::new(session_id.to_owned()))
            .await?;
        // Bridge confirmed the cancel notification went out; settle local
        // state. The agent will return `cancelled` on the in-flight prompt
        // anyway, but firing the token lets the task observe the cancel
        // promptly even if the agent's response is slow.
        self.cancel_prompts_for_session(session_id).await;
        let guard = state.lock().await;
        guard.append_session_event(
            session_id,
            "info",
            "session.cancel_requested",
            "cancel requested",
            "{}",
        )?;
        Ok(())
    }

    async fn cancel_prompts_for_session(&self, session_id: &str) {
        let prompts = self.prompts.lock().await;
        for handle in prompts.values() {
            if handle.session_id == session_id {
                handle.cancel.cancel();
            }
        }
    }

    async fn cancel_all_prompts(&self) {
        // Drain handles out of the map first so we don't hold the registry
        // lock while awaiting tasks (the tasks themselves may indirectly
        // touch the map via `reap_finished` from other paths).
        let handles: Vec<PromptHandle> = {
            let mut prompts = self.prompts.lock().await;
            prompts.drain().map(|(_, handle)| handle).collect()
        };
        for handle in &handles {
            handle.cancel.cancel();
        }
        // Await each task so terminal `prompts` rows ('cancelled' /
        // 'errored') are written before shutdown returns. Bounded so a
        // misbehaving task cannot delay teardown indefinitely; we abort
        // anything still running past the budget and log it.
        let deadline = tokio::time::Instant::now() + PROMPT_DRAIN_BUDGET;
        for handle in handles {
            let PromptHandle { join, .. } = handle;
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, join).await {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    tracing::warn!(error = ?err, "prompt task panicked during drain");
                }
                Err(_) => {
                    // The task is still running. We've already cancelled it;
                    // dropping the JoinHandle here detaches it. The bridge's
                    // connection is being torn down moments from now, so the
                    // task will see send-error and write its terminal row on
                    // its next loop turn.
                    tracing::warn!("prompt task did not settle within drain budget");
                }
            }
        }
    }

    async fn reap_finished(&self) {
        let mut prompts = self.prompts.lock().await;
        prompts.retain(|_, handle| !handle.join.is_finished());
    }
}

enum Outcome {
    Settled(Result<StopReason>),
    Cancelled,
}

fn stop_reason_str(reason: StopReason) -> String {
    match reason {
        StopReason::EndTurn => "end_turn".to_owned(),
        StopReason::MaxTokens => "max_tokens".to_owned(),
        StopReason::MaxTurnRequests => "max_turn_requests".to_owned(),
        StopReason::Refusal => "refusal".to_owned(),
        StopReason::Cancelled => "cancelled".to_owned(),
        // StopReason is #[non_exhaustive]; future SDK additions surface as
        // the wire string verbatim until we add a typed mapping for them.
        other => format!("{other:?}").to_lowercase(),
    }
}

/// Convert client-supplied prompt JSON into the typed `ContentBlock` vec the
/// ACP SDK requires. The accepted shape is `[{ "type": "text", "text": "..." }]`
/// (camelCase) or a bare string for convenience. Other ACP content variants
/// (resource, resource_link, image, audio) round-trip through `serde_json::from_value`.
pub fn parse_prompt_blocks(prompt: &Value) -> Result<Vec<ContentBlock>> {
    let blocks = match prompt {
        Value::String(text) => vec![ContentBlock::Text(
            agent_client_protocol::schema::TextContent::new(text.clone()),
        )],
        Value::Array(items) => {
            if items.is_empty() {
                return Err(StackError::PromptBodyEmpty);
            }
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                let block: ContentBlock = serde_json::from_value(item.clone())
                    .map_err(|err| StackError::PromptBodyInvalid(err.to_string()))?;
                out.push(block);
            }
            out
        }
        Value::Null => return Err(StackError::PromptBodyEmpty),
        other => {
            return Err(StackError::PromptBodyInvalid(format!(
                "prompt must be a string or array, got {}",
                value_kind(other)
            )));
        }
    };
    Ok(blocks)
}

fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Parse the optional `mcp_servers` field of a session create/load body into
/// the SDK's `Vec<McpServer>`.
pub fn parse_mcp_servers(value: Option<&Value>) -> Result<Vec<McpServer>> {
    let Some(value) = value else {
        return Ok(vec![]);
    };
    if value.is_null() {
        return Ok(vec![]);
    }
    serde_json::from_value(value.clone())
        .map_err(|err| StackError::PromptBodyInvalid(format!("mcp_servers invalid: {err}")))
}

/// Hash `[agent].command` and compare against `expected_sha256`. Returns
/// `AgentSha256Mismatch` on mismatch and `AgentSpawnFailed` if the file
/// cannot be read. Path resolution mirrors what `tokio::process::Command`
/// will do at spawn time: bare names look up `$PATH`, relative paths with
/// a `/` resolve against `cwd`, absolute paths are used as-is.
fn verify_agent_binary_sha256(command: &str, cwd: &std::path::Path, expected: &str) -> Result<()> {
    let path =
        resolve_command_path(command, cwd).ok_or_else(|| StackError::AgentInitializeFailed {
            reason: format!("agent command `{command}` not found on PATH"),
        })?;
    let bytes = std::fs::read(&path).map_err(|source| StackError::AgentSpawnFailed { source })?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let actual = format!("{:x}", hasher.finalize());
    if actual != expected {
        return Err(StackError::AgentSha256Mismatch {
            expected: expected.to_owned(),
            actual,
        });
    }
    Ok(())
}

/// Resolve `[agent].env` names against the secret store. Returns an empty
/// map when the list is empty so the secret store is never opened by
/// no-secret agents (relevant for tests and stripped-down deployments).
pub fn resolve_agent_env(
    agent: &AgentConfig,
    secrets: &SecretStore,
) -> Result<HashMap<String, String>> {
    let mut env = HashMap::with_capacity(agent.env.len());
    for name in &agent.env {
        let value = secrets.get(name)?;
        env.insert(name.clone(), value.to_owned());
    }
    Ok(env)
}

fn resolve_agent_cwd(agent: &AgentConfig, workspace_root: &str) -> PathBuf {
    agent
        .cwd
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(workspace_root))
}
