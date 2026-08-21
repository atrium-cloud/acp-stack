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
//! Stopped --begin_update()--> Updating --finish_update()--> Stopped
//! ```
//!
//! `Starting` exists so that two concurrent `POST /v1/agent/start` requests
//! during a slow initialize cannot both spawn an agent — the second one
//! sees `Starting` and returns `agent.already_running`.
//!
//! `record_*` helpers come in two flavors: sync (`&StateStore`) for use before
//! the store is moved into `AppState`, and async (`&Arc<Mutex<StateStore>>`)
//! for use after, where a brief lock acquires the connection.

mod bridge;
mod parse;
mod prompts;
mod sessions;

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

/// Small fixed delay before an `on-crash` restart. This keeps a fast-crashing
/// harness from tight-looping while preserving the current single-retry
/// restart-policy shape (`never` vs `on-crash`).
const AGENT_CRASH_RESTART_BACKOFF: Duration = Duration::from_millis(250);

/// Poll cadence for child-process exit detection. The ACP SDK task can remain
/// parked waiting for orderly shutdown after stdio EOF, so the supervisor also
/// observes the subprocess directly.
const AGENT_EXIT_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// How long a lazy start will ride out a spawn that is already in flight. The
/// initialize handshake takes seconds on a cold box (adapter spawn plus the
/// harness reading its own config), and a request that lands mid-spawn should
/// use that agent rather than fail on a transient state.
const AGENT_LAZY_START_WAIT: Duration = Duration::from_secs(30);

/// Poll cadence while waiting out an in-flight `Starting` transition.
const AGENT_LAZY_START_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// `[agent].restart` value that opts the target out of every runtime-initiated
/// spawn: no crash restart, and no lazy start on a request either.
pub const AGENT_RESTART_NEVER: &str = "never";

use agent_client_protocol::schema::v1::{
    ContentBlock, McpServer, PromptRequest, PromptResponse, SessionId as AcpSessionId, StopReason,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex as TokioMutex, RwLock};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::AgentConfig;
use crate::error::{Result, StackError};
use crate::events::EventHub;
use crate::runtime::agent::acp_bridge::{
    AcpBridge, AcpBridgeExit, AcpBridgeExitReason, AgentCapabilitiesDto,
    AgentSessionConfigCategory, AgentSessionModelSelection, IGNORED_FEATURE_AGENT_MODE,
    IGNORED_FEATURE_AGENT_MODEL, IgnoredFeature, PartitionedMcpServers, SessionEventSink,
    SkippedMcpServer, StateStoreSessionSink, meta_message_id, prompt_message_id_meta,
    resolve_command_path, session_config_id_for_value, session_model_selection_for_value,
};
use crate::runtime::agent::model_discovery::model_value_is_explicit_without_discovery;
use crate::runtime::agent::provider_keys::ResolvedProviderSnapshot;
use crate::runtime::agent::session_changes::SessionChangesHandle;
use crate::secrets::SecretStore;
use crate::state::{
    EVENT_KIND_MCP_SESSION_SKIPPED, EVENT_KIND_PROMPT_ERRORED, EVENT_KIND_PROMPT_INFERENCE_FAILED,
    EVENT_KIND_SESSION_CAPABILITY_IGNORED, EVENT_SOURCE_SYSTEM, FailureClass, ListedSessionRecord,
    NewPromptRecord, NewSessionRecord, PromptRecord, PromptStatus, SESSION_STATUS_ACTIVE,
    SESSION_STATUS_CLOSED, SessionRecord, StateStore, next_prompt_id, next_prompt_message_id,
    next_session_id,
};

use self::bridge::*;
use self::parse::*;

pub(crate) use self::parse::resolve_session_cwd;
pub use self::parse::{parse_mcp_servers, parse_prompt_blocks, resolve_agent_env};

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
    Updating,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentStateLabel {
    Stopped,
    Starting,
    Running,
    Stopping,
    Updating,
}

impl AgentStateLabel {
    fn from_state(state: &AgentState) -> Self {
        match state {
            AgentState::Stopped => Self::Stopped,
            AgentState::Starting => Self::Starting,
            AgentState::Running(_) => Self::Running,
            AgentState::Stopping => Self::Stopping,
            AgentState::Updating => Self::Updating,
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
            Self::Updating => "updating",
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
    pub loaded_providers: Option<Vec<ResolvedProviderSnapshot>>,
}

/// What a lazy-start caller should do next. Decided by
/// [`AgentSupervisor::await_start_readiness`] so the spawn decision stays
/// inside the supervisor's state machine instead of being re-derived from a
/// racy `snapshot()` by every request handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStartReadiness {
    /// A bridge is live; the request can proceed.
    Running,
    /// The supervisor is idle, so the caller may spawn the configured agent.
    NeedsStart,
    /// The supervisor is mid-stop or updating, or a concurrent spawn did not
    /// settle within `AGENT_LAZY_START_WAIT`. Callers keep surfacing
    /// `agent.not_running`.
    Unavailable,
}

/// Record the MCP servers that were dropped from a session because the running
/// agent does not advertise their transport. Silent when nothing was dropped so
/// the common path adds no rows.
fn append_mcp_skipped_event(
    store: &StateStore,
    session_id: &str,
    skipped: &[SkippedMcpServer],
) -> Result<()> {
    if skipped.is_empty() {
        return Ok(());
    }
    tracing::warn!(
        session_id,
        skipped = skipped.len(),
        "dropping MCP servers whose transport the agent does not advertise"
    );
    let payload = json!({
        "session_id": session_id,
        "skipped": skipped,
    })
    .to_string();
    store.append_session_event(
        session_id,
        "warn",
        EVENT_KIND_MCP_SESSION_SKIPPED,
        "mcp servers skipped for session",
        &payload,
    )?;
    Ok(())
}

/// Record configured features (mode, model) ignored for a session because the
/// agent does not advertise the backing capability. Silent when nothing was
/// ignored so the common path adds no rows.
fn append_capability_ignored_event(
    store: &StateStore,
    session_id: &str,
    ignored: &[IgnoredFeature],
) -> Result<()> {
    if ignored.is_empty() {
        return Ok(());
    }
    tracing::warn!(
        session_id,
        ignored = ignored.len(),
        "ignoring configured features the agent's capabilities cannot honor"
    );
    let payload = json!({
        "session_id": session_id,
        "ignored": ignored,
    })
    .to_string();
    store.append_session_event(
        session_id,
        "warn",
        EVENT_KIND_SESSION_CAPABILITY_IGNORED,
        "configured capabilities ignored for session",
        &payload,
    )?;
    Ok(())
}

/// Session attach result shared by create/load/resume/fork: the durable row,
/// the MCP server names actually sent to the agent, and the configured
/// features that were ignored for capability reasons.
#[derive(Debug)]
pub struct SessionAttachOutcome {
    pub record: SessionRecord,
    pub attached_mcp: Vec<String>,
    pub ignored: Vec<IgnoredFeature>,
}

/// `None` means "still transitioning, poll again". `Stopping` and `Updating`
/// are deliberate operator transitions, so they resolve immediately: spawning
/// under them would fight the operator that asked for the agent to go away.
fn readiness_for_state(state: &AgentState) -> Option<AgentStartReadiness> {
    match state {
        AgentState::Running(_) => Some(AgentStartReadiness::Running),
        AgentState::Stopped => Some(AgentStartReadiness::NeedsStart),
        AgentState::Stopping | AgentState::Updating => Some(AgentStartReadiness::Unavailable),
        AgentState::Starting => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionListSyncStatus {
    Synced,
    Unsupported,
    NotRunning,
}

impl SessionListSyncStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Synced => "synced",
            Self::Unsupported => "unsupported",
            Self::NotRunning => "not_running",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionListSyncResult {
    pub attempted: bool,
    pub status: SessionListSyncStatus,
    pub upserted: u32,
    pub updated: u32,
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
    state: Arc<TokioMutex<AgentState>>,
    capabilities: Arc<RwLock<Option<AgentCapabilitiesDto>>>,
    last_pid: Arc<RwLock<Option<u32>>>,
    loaded_providers: Arc<RwLock<Option<Vec<ResolvedProviderSnapshot>>>>,
    /// In-flight prompt registry. Each entry is a fire-and-forget background
    /// task plus its cancellation token. We never block on these from
    /// session-tier handlers — the durable `prompts` row is the source of
    /// truth for clients polling status.
    prompts: Arc<TokioMutex<HashMap<String, PromptHandle>>>,
    /// Serializes prompt submission with guarded restarts so `restart auto`
    /// cannot pass an idle check while a new prompt has cloned the bridge but not
    /// yet inserted its durable prompt row.
    dispatch_gate: Arc<TokioMutex<()>>,
}

#[derive(Clone)]
struct SupervisorShared {
    state: Arc<TokioMutex<AgentState>>,
    capabilities: Arc<RwLock<Option<AgentCapabilitiesDto>>>,
    last_pid: Arc<RwLock<Option<u32>>>,
    loaded_providers: Arc<RwLock<Option<Vec<ResolvedProviderSnapshot>>>>,
}

#[derive(Clone)]
struct RestartContext {
    target_id: String,
    agent: AgentConfig,
    workspace_root: String,
    env: HashMap<String, String>,
    providers: Vec<ResolvedProviderSnapshot>,
    state_store: Arc<TokioMutex<StateStore>>,
    session_changes: SessionChangesHandle,
    event_hub: EventHub,
    permissions: Option<crate::runtime::mediation::permissions::PermissionService>,
    sandbox: crate::config::SandboxConfig,
    network_provider: Option<crate::extensions::NetworkProviderExtension>,
}

pub struct AgentStartRequest<'a> {
    pub target_id: &'a str,
    pub agent: &'a AgentConfig,
    pub workspace_root: &'a str,
    pub env: HashMap<String, String>,
    pub providers: Vec<ResolvedProviderSnapshot>,
    pub state: &'a Arc<TokioMutex<StateStore>>,
    pub session_changes: &'a SessionChangesHandle,
    pub event_hub: EventHub,
    pub permissions: Option<crate::runtime::mediation::permissions::PermissionService>,
    pub sandbox: crate::config::SandboxConfig,
    pub network_provider: Option<crate::extensions::NetworkProviderExtension>,
}

impl Default for AgentSupervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentSupervisor {
    pub fn new() -> Self {
        Self {
            state: Arc::new(TokioMutex::new(AgentState::Stopped)),
            capabilities: Arc::new(RwLock::new(None)),
            last_pid: Arc::new(RwLock::new(None)),
            loaded_providers: Arc::new(RwLock::new(None)),
            prompts: Arc::new(TokioMutex::new(HashMap::new())),
            dispatch_gate: Arc::new(TokioMutex::new(())),
        }
    }

    fn shared(&self) -> SupervisorShared {
        SupervisorShared {
            state: Arc::clone(&self.state),
            capabilities: Arc::clone(&self.capabilities),
            last_pid: Arc::clone(&self.last_pid),
            loaded_providers: Arc::clone(&self.loaded_providers),
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
    pub async fn start(&self, request: AgentStartRequest<'_>) -> Result<AgentCapabilitiesDto> {
        // First lock: atomically transition Stopped -> Starting. Refusing
        // any other start under the same lock prevents concurrent spawns.
        {
            let mut guard = self.state.lock().await;
            match &*guard {
                AgentState::Stopped => {
                    *guard = AgentState::Starting;
                }
                AgentState::Starting
                | AgentState::Running(_)
                | AgentState::Stopping
                | AgentState::Updating => return Err(StackError::AgentAlreadyRunning),
            }
        }

        let loaded_providers = request.providers.clone();
        let restart_context = RestartContext {
            target_id: request.target_id.to_owned(),
            agent: request.agent.clone(),
            workspace_root: request.workspace_root.to_owned(),
            env: request.env.clone(),
            providers: request.providers.clone(),
            state_store: request.state.clone(),
            session_changes: request.session_changes.clone(),
            event_hub: request.event_hub.clone(),
            permissions: request.permissions.clone(),
            sandbox: request.sandbox.clone(),
            network_provider: request.network_provider.clone(),
        };
        match self.do_start(request).await {
            Ok((capabilities, bridge)) => {
                let pid = bridge.pid();
                let bridge = Arc::new(bridge);
                {
                    let mut guard = self.state.lock().await;
                    *guard = AgentState::Running(Arc::clone(&bridge));
                }
                *self.capabilities.write().await = Some(capabilities.clone());
                *self.last_pid.write().await = pid;
                *self.loaded_providers.write().await = Some(loaded_providers);
                spawn_bridge_exit_monitor(self.shared(), bridge, restart_context);
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
                *self.loaded_providers.write().await = None;
                Err(err)
            }
        }
    }

    /// Inner half of `start`: between the `Starting` and `Running` state
    /// transitions. On any error, MUST shut down any bridge it spawned so
    /// the caller's rollback only needs to flip state.
    async fn do_start(
        &self,
        request: AgentStartRequest<'_>,
    ) -> Result<(AgentCapabilitiesDto, AcpBridge)> {
        spawn_agent_bridge(
            request.target_id,
            request.agent,
            request.workspace_root,
            request.env,
            request.state,
            request.session_changes,
            request.event_hub,
            request.permissions,
            request.sandbox,
            request.network_provider,
        )
        .await
    }

    /// Tear down the running agent. Returns the agent's exit status if
    /// available. Records `agent.stopped` regardless of clean exit.
    pub async fn stop(
        &self,
        target_id: &str,
        state: &Arc<TokioMutex<StateStore>>,
        event_hub: &EventHub,
    ) -> Result<Option<i32>> {
        let _dispatch_guard = self.dispatch_gate.lock().await;
        self.stop_inner(target_id, state, event_hub).await
    }

    pub async fn stop_when_restart_safe(
        &self,
        target_id: &str,
        state: &Arc<TokioMutex<StateStore>>,
        event_hub: &EventHub,
    ) -> Result<std::result::Result<Option<i32>, Vec<crate::state::RestartBlockerRecord>>> {
        let _dispatch_guard = self.dispatch_gate.lock().await;
        let blockers = {
            let guard = state.lock().await;
            guard.query_restart_blockers(Some(target_id))?
        };
        if !blockers.is_empty() {
            return Ok(Err(blockers));
        }
        let exit = match self.stop_inner(target_id, state, event_hub).await {
            Ok(exit) => exit,
            Err(StackError::AgentNotRunning) => None,
            Err(err) => return Err(err),
        };
        Ok(Ok(exit))
    }

    async fn stop_inner(
        &self,
        target_id: &str,
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
        *self.loaded_providers.write().await = None;

        // Record the lifecycle row best-effort. A DB error is logged but
        // does not mask the original shutdown outcome — the supervisor is
        // already in a coherent state thanks to the transition above.
        let exit = shutdown_result?;
        let data = json!({
            "target_id": target_id,
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
        target_id: &str,
        state: &Arc<TokioMutex<StateStore>>,
        event_hub: &EventHub,
    ) {
        // Determine whether there's anything to stop without holding the
        // lock across the entire shutdown sequence.
        let needs_stop = matches!(*self.state.lock().await, AgentState::Running(_));
        if !needs_stop {
            return;
        }
        if let Err(err) = self.stop(target_id, state, event_hub).await {
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
        let loaded_providers = self.loaded_providers.read().await.clone();
        AgentSnapshot {
            state: state_label,
            latest_capabilities: capabilities,
            pid,
            loaded_providers,
        }
    }

    pub async fn is_running(&self) -> bool {
        matches!(*self.state.lock().await, AgentState::Running(_))
    }

    /// Classify the supervisor for a caller that needs a live bridge and is
    /// willing to spawn one. Waits out an in-flight `Starting` up to
    /// `AGENT_LAZY_START_WAIT`; the state lock is released between polls so
    /// the spawn it is waiting on can make progress.
    pub async fn await_start_readiness(&self) -> AgentStartReadiness {
        let deadline = tokio::time::Instant::now() + AGENT_LAZY_START_WAIT;
        loop {
            if let Some(readiness) = readiness_for_state(&*self.state.lock().await) {
                return readiness;
            }
            if tokio::time::Instant::now() >= deadline {
                return AgentStartReadiness::Unavailable;
            }
            tokio::time::sleep(AGENT_LAZY_START_POLL_INTERVAL).await;
        }
    }

    pub async fn try_begin_update(&self) -> bool {
        let mut guard = self.state.lock().await;
        if matches!(*guard, AgentState::Stopped) {
            *guard = AgentState::Updating;
            true
        } else {
            false
        }
    }

    pub async fn finish_update(&self) {
        let mut guard = self.state.lock().await;
        if matches!(*guard, AgentState::Updating) {
            *guard = AgentState::Stopped;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_outcome_classifies_agent_process_failures() {
        let terminal = build_terminal_outcome_with_prompt_id(
            Outcome::Settled(Err(StackError::AgentNotRunning)),
            Some("prm_process"),
        );

        assert_eq!(terminal.status, PromptStatus::Errored);
        assert_eq!(
            terminal.failure_class,
            Some(FailureClass::AgentProcess.as_str())
        );
        let event = terminal.session_event.expect("errored event");
        assert_eq!(event.kind, EVENT_KIND_PROMPT_ERRORED);
        assert!(event.payload_json.contains("prm_process"));
    }

    #[test]
    fn skipped_mcp_servers_are_recorded_only_when_something_was_dropped() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let store = StateStore::open(tempdir.path().join("state.sqlite")).expect("state open");
        store.migrate().expect("migrate");
        store
            .insert_session(NewSessionRecord {
                id: "sess_skip".to_owned(),
                agent_id: "fake".to_owned(),
                cwd: "/tmp/sess_skip".to_owned(),
                title: None,
                metadata_json: "{}".to_owned(),
            })
            .expect("session inserted");

        append_mcp_skipped_event(&store, "sess_skip", &[]).expect("empty skip writes nothing");
        assert!(skipped_events(&store).is_empty());

        append_mcp_skipped_event(
            &store,
            "sess_skip",
            &[SkippedMcpServer {
                name: "linear".to_owned(),
                capability: "mcpCapabilities.http",
            }],
        )
        .expect("skip recorded");

        let events = skipped_events(&store);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].level, "warn");
        assert!(events[0].payload_json.contains("linear"), "{events:?}");
        assert!(
            events[0].payload_json.contains("mcpCapabilities.http"),
            "{events:?}"
        );
    }

    fn skipped_events(store: &StateStore) -> Vec<crate::state::Event> {
        store
            .query_events(crate::state::LogFilter {
                kind: Some(EVENT_KIND_MCP_SESSION_SKIPPED),
                ..crate::state::LogFilter::with_limit(10)
            })
            .expect("query events")
    }

    #[test]
    fn stopped_state_asks_the_caller_to_spawn() {
        assert_eq!(
            readiness_for_state(&AgentState::Stopped),
            Some(AgentStartReadiness::NeedsStart)
        );
    }

    #[test]
    fn operator_driven_transitions_never_become_a_spawn() {
        for state in [AgentState::Stopping, AgentState::Updating] {
            assert_eq!(
                readiness_for_state(&state),
                Some(AgentStartReadiness::Unavailable)
            );
        }
    }

    #[test]
    fn in_flight_start_is_waited_out_rather_than_classified() {
        assert_eq!(readiness_for_state(&AgentState::Starting), None);
    }

    #[tokio::test]
    async fn readiness_waits_for_an_in_flight_start_to_settle() {
        let supervisor = AgentSupervisor::new();
        *supervisor.state.lock().await = AgentState::Starting;
        let state = Arc::clone(&supervisor.state);
        tokio::spawn(async move {
            tokio::time::sleep(AGENT_LAZY_START_POLL_INTERVAL * 2).await;
            *state.lock().await = AgentState::Stopped;
        });

        assert_eq!(
            supervisor.await_start_readiness().await,
            AgentStartReadiness::NeedsStart
        );
    }

    #[test]
    fn terminal_outcome_classifies_sqlite_failures() {
        let terminal = build_terminal_outcome_with_prompt_id(
            Outcome::Settled(Err(StackError::State(rusqlite::Error::InvalidQuery))),
            Some("prm_sqlite"),
        );

        assert_eq!(terminal.status, PromptStatus::Errored);
        assert_eq!(terminal.failure_class, Some(FailureClass::Sqlite.as_str()));
        assert_eq!(
            terminal.session_event.expect("errored event").kind,
            EVENT_KIND_PROMPT_ERRORED
        );
    }
}
