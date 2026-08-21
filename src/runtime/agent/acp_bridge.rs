//! Live ACP bridge: spawns an ACP agent subprocess and owns the JSON-RPC
//! connection to it.
//!
//! Wire format is newline-delimited JSON over stdio (per
//! `docs/ref/acp/protocol/transports.md`). Framing, request/response
//! correlation, and the message schema all live in the
//! `agent-client-protocol` crate; this module is the thin wrapper that:
//!
//! - spawns the configured `[agent].command` via `tokio::process::Command`
//!   with the minimum env we resolved for `[agent].env`,
//! - drives the ACP `initialize` handshake,
//! - captures the resulting `AgentCapabilities` as a JSON snapshot for our
//!   own API contract (so upstream renames don't leak through),
//! - retains a `ConnectionTo<Agent>` handle so session methods can be
//!   dispatched after initialize completes,
//! - persists `session/update` notifications to SQLite through a
//!   `SessionEventSink`,
//! - keeps the connection running in a dedicated task until `shutdown` is
//!   called or the supervisor is dropped.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    AgentNotification, CancelNotification, ClientCapabilities, ClientSessionCapabilities,
    CloseSessionRequest, CreateTerminalRequest, DeleteSessionRequest, FileSystemCapabilities,
    ForkSessionRequest, ForkSessionResponse, Implementation, InitializeRequest, InitializeResponse,
    KillTerminalRequest, ListSessionsRequest, ListSessionsResponse, LoadSessionRequest, McpServer,
    NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse, ReadTextFileRequest,
    ReleaseTerminalRequest, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, ResumeSessionRequest, SessionConfigOptionCategory,
    SessionConfigOptionsCapabilities, SessionConfigValueId, SessionId, SessionInfo,
    SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, TerminalOutputRequest,
    WaitForTerminalExitRequest, WriteTextFileRequest,
};
use agent_client_protocol::{Agent, Client, ConnectionTo};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex as TokioMutex, Notify, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::config::AgentConfig;
use crate::error::{Result, StackError};
use crate::runtime::agent::acp_codec::{
    auto_approve_acp_permission, enqueue_session_notification, handle_read_text_file,
    handle_write_text_file, resolve_acp_permission, spawn_session_notification_queue,
};
use crate::runtime::agent::acp_terminal::{
    TerminalHandlerContext, TerminalRegistry, handle_create_terminal, handle_kill_terminal,
    handle_release_terminal, handle_terminal_output, handle_wait_for_terminal_exit,
};
use crate::runtime::agent::inference_failure::{self, Classified};
use crate::runtime::mediation::permissions::PermissionService;
use crate::runtime::process_runner::{forward_host_env_tokio, kill_tokio_process_group};
use crate::state::FailureClass;

mod capabilities;
mod process_env;
mod sessions;
mod spawn;

use self::process_env::build_agent_process_env;

pub use self::capabilities::{
    AgentCapabilitiesDto, IGNORED_FEATURE_AGENT_MODE, IGNORED_FEATURE_AGENT_MODEL,
    IGNORED_FEATURE_MCP_SERVER, IgnoredFeature, PartitionedMcpServers, SkippedMcpServer,
};
pub(crate) use self::process_env::{
    KIMI_API_KEY_ENV, KIMI_CODE_AGENT_ID, kimi_default_model_for_provider,
};
// `spawn.rs` owns command resolution; `resolve_command_path` and
// `agent_process_path` keep their pre-split paths for the CLI, supervisor and
// terminal handlers that import them from this module.
pub(super) use self::spawn::agent_process_path;
pub(crate) use self::spawn::resolve_command_path;

// External callers (CLI, supervisor, model_discovery, integration tests) wrote
// `crate::runtime::agent::acp_bridge::{SessionEventSink, StateStoreSessionSink, session_*}`
// before the extraction. Preserve those paths with re-exports so the split is
// internal to `runtime::agent`.
pub use crate::runtime::agent::acp_codec::{
    meta_message_id, prompt_message_id_meta, session_config_id_for_value, session_config_values,
    session_model_selection_for_value, session_model_values,
};
pub use crate::runtime::agent::acp_terminal::TerminalCommandLog;
pub use crate::runtime::agent::session_changes::SessionChangesHandle;
pub use crate::runtime::agent::session_sink::{SessionEventSink, StateStoreSessionSink};

/// How the bridge answers agent-initiated `session/request_permission` calls.
#[derive(Clone)]
pub enum AcpPermissionPolicy {
    /// No operator channel (model-discovery probes): answer `cancelled`.
    Cancel,
    /// Daemon path: durable, operator-decided permissions.
    Service(PermissionService),
    /// `acps agent test`: a non-interactive smoke test with no operator to
    /// ask, so allow-kind options are approved on the spot.
    AutoApprove,
}

impl From<Option<PermissionService>> for AcpPermissionPolicy {
    fn from(service: Option<PermissionService>) -> Self {
        match service {
            Some(service) => Self::Service(service),
            None => Self::Cancel,
        }
    }
}

/// Maximum time we wait for `initialize` to return before declaring the agent
/// unresponsive. A warm agent handshakes in milliseconds, but the first launch
/// on a freshly provisioned host pays for cold page cache, JIT/runtime warmup
/// and the agent's own first-run setup; 15s was tight enough that hosted init
/// failed `provider_configure` on real sprites. The deadline exists to catch a
/// wedged or incompatible agent, so it can be generous without losing that.
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(60);

/// Maximum time we wait between sending the shutdown signal and SIGKILLing
/// the agent child. The closure should return immediately once the oneshot
/// fires; if it does not, the child is misbehaving and we cut losses.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Poll cadence for the bridge-owned child exit watcher. ACP transports can
/// remain parked until orderly shutdown, so process death is observed directly.
const CHILD_EXIT_POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentSessionConfigCategory {
    Mode,
    Model,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentSessionModelSelection {
    ConfigOption { config_id: String },
}

impl AgentSessionConfigCategory {
    pub fn id(self) -> &'static str {
        match self {
            Self::Mode => "mode",
            Self::Model => "model",
        }
    }

    pub(super) fn matches(self, category: &SessionConfigOptionCategory) -> bool {
        matches!(
            (self, category),
            (Self::Mode, SessionConfigOptionCategory::Mode)
                | (Self::Model, SessionConfigOptionCategory::Model)
        )
    }
}

/// One spawned agent + its live ACP connection.
///
/// Use through `Arc<AcpBridge>` once spawned so multiple session dispatchers
/// and the shutdown path can hold the same handle without serializing through
/// the supervisor's state lock. Single-use lifecycle: `spawn` once, hold while
/// the agent should run, then call `shutdown()` exactly once.
pub struct AcpBridge {
    /// `TokioMutex<Option<Child>>` so `shutdown(&self)` can `.take()` the
    /// child to await/kill without consuming the bridge. Reads after a
    /// successful shutdown see `None` and short-circuit.
    child: Arc<TokioMutex<Option<Child>>>,
    capabilities: AgentCapabilitiesDto,
    /// Cloneable handle for sending requests/notifications to the agent.
    /// Populated inside the connect closure before it parks on `shutdown_rx`,
    /// so callers outside the closure can dispatch session methods. Wrapped
    /// in an `Option` because `shutdown()` clears it before tearing down.
    connection: TokioMutex<Option<ConnectionTo<Agent>>>,
    shutdown_tx: TokioMutex<Option<oneshot::Sender<()>>>,
    connection_task: TokioMutex<Option<JoinHandle<()>>>,
    planned_shutdown: Arc<AtomicBool>,
    exit_rx: watch::Receiver<Option<AcpBridgeExit>>,
    spawn_pid: Option<u32>,
    /// Held so `shutdown()` can flush any pending `session/update` writes the
    /// sink's background writer task has queued.
    sink: Arc<dyn SessionEventSink>,
    notification_drain: Arc<NotificationDrain>,
    /// Live client terminals. Terminal children run in their own process
    /// groups (not the agent's), so shutdown must drain this registry
    /// explicitly — the agent-pgroup kill never reaches them.
    terminals: Arc<TerminalRegistry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcpBridgeExitReason {
    Shutdown,
    ProcessExited,
    ConnectionEnded,
    ConnectionError,
}

impl AcpBridgeExitReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shutdown => "shutdown",
            Self::ProcessExited => "process_exited",
            Self::ConnectionEnded => "connection_ended",
            Self::ConnectionError => "connection_error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpBridgeExit {
    pub pid: Option<u32>,
    pub planned: bool,
    pub reason: AcpBridgeExitReason,
    pub message: Option<String>,
    pub exit_status: Option<i32>,
}

#[derive(Default)]
pub(super) struct NotificationDrain {
    active: AtomicUsize,
    changed: Notify,
}

pub(super) struct NotificationGuard {
    drain: Arc<NotificationDrain>,
}

impl NotificationDrain {
    pub(super) fn enter(self: &Arc<Self>) -> NotificationGuard {
        self.active.fetch_add(1, Ordering::SeqCst);
        NotificationGuard {
            drain: Arc::clone(self),
        }
    }

    pub(super) async fn wait_idle(&self) {
        self.wait_at_most(0).await;
    }

    pub(super) async fn wait_at_most(&self, maximum: usize) {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            // Register the waiter before re-checking the count: notify_waiters()
            // stores no permit, so a wakeup fired between an unregistered check
            // and the await would be lost and the final 1->0 transition never
            // notifies again.
            notified.as_mut().enable();
            if self.active.load(Ordering::SeqCst) <= maximum {
                return;
            }
            notified.await;
        }
    }
}

impl Drop for NotificationGuard {
    fn drop(&mut self) {
        self.drain.active.fetch_sub(1, Ordering::SeqCst);
        self.drain.changed.notify_waiters();
    }
}

impl AcpBridge {
    pub fn capabilities(&self) -> &AgentCapabilitiesDto {
        &self.capabilities
    }

    /// Best-effort pid of the spawned child. Captured at spawn time and
    /// stable for the bridge lifetime; once `shutdown()` has reaped the
    /// child, callers should rely on `agent_lifecycle` rows instead.
    pub fn pid(&self) -> Option<u32> {
        self.spawn_pid
    }

    pub fn subscribe_exit(&self) -> watch::Receiver<Option<AcpBridgeExit>> {
        self.exit_rx.clone()
    }

    pub fn planned_shutdown(&self) -> bool {
        self.planned_shutdown.load(Ordering::SeqCst)
    }

    pub async fn try_wait_child(&self) -> Result<Option<i32>> {
        let mut guard = self.child.lock().await;
        let Some(child) = guard.as_mut() else {
            return Ok(None);
        };
        let Some(status) = child
            .try_wait()
            .map_err(|source| StackError::AgentSpawnFailed { source })?
        else {
            return Ok(None);
        };
        *guard = None;
        Ok(status.code())
    }

    /// Gracefully tear down the agent: signal the connection task to return,
    /// then close stdin / wait / SIGKILL the child on a bounded timeline.
    /// Returns the exit status if available. Idempotent: a second call sees
    /// every field already `None` and returns `Ok(None)`.
    pub async fn shutdown(&self) -> Result<Option<i32>> {
        self.teardown(false).await
    }

    /// Tear down a provisional probe by killing the process group before the
    /// client IO loop drops stdout. This keeps one-shot discovery from
    /// surfacing adapter-side broken-pipe stack traces after values were read.
    pub async fn terminate_probe(&self) -> Result<Option<i32>> {
        self.teardown(true).await
    }

    /// Shared teardown for both exit paths. `kill_first` selects the probe
    /// ordering: SIGKILL the process group before the client IO loop is asked
    /// to stop. The graceful path instead stops the IO loop first, so the
    /// child can notice stdin closure and exit on its own, and only escalates
    /// to a kill when it does not.
    async fn teardown(&self, kill_first: bool) -> Result<Option<i32>> {
        self.planned_shutdown.store(true, Ordering::SeqCst);
        // Clear the cloneable handle so any in-flight session calls fail
        // fast with `AgentNotRunning` rather than hanging on a dead IO loop.
        self.clear_connection().await;
        // Kill-and-release live client terminals before agent teardown: they
        // run in their own process groups, so the agent-pgroup SIGKILL below
        // would orphan them. The supervisor's crash monitor also routes
        // through shutdown(), so this covers unplanned exits too.
        self.terminals.drain_all().await;

        if !kill_first {
            self.stop_connection_task().await;
        }

        let status = match self.child.lock().await.take() {
            Some(mut child) => {
                // Every kill here is a process-group SIGKILL rather than a
                // plain child kill, so any grandchildren the agent forked (MCP
                // servers, tool subprocesses) also die with the daemon — the
                // bridge spawned with `process_group(0)`, so the child is its
                // own pgid leader.
                if kill_first {
                    kill_tokio_process_group(&mut child);
                }
                match timeout(SHUTDOWN_GRACE, child.wait()).await {
                    Ok(Ok(status)) => Some(status),
                    Ok(Err(err)) => {
                        if kill_first {
                            tracing::warn!(error = ?err, "acp bridge: wait failed after probe kill");
                        } else {
                            tracing::warn!(error = ?err, "acp bridge: wait failed");
                            kill_tokio_process_group(&mut child);
                        }
                        None
                    }
                    Err(_) => {
                        if !kill_first {
                            kill_tokio_process_group(&mut child);
                            let _ = child.wait().await.ok();
                        }
                        None
                    }
                }
            }
            None => None,
        };

        if kill_first {
            self.stop_connection_task().await;
        }

        Ok(status.and_then(|s| s.code()))
    }

    /// Signal the connect closure to return, wait for the connection task to
    /// finish, then flush everything it queued.
    async fn stop_connection_task(&self) {
        if let Some(tx) = self.shutdown_tx.lock().await.take() {
            let _ = tx.send(());
        }
        self.wait_connection_task().await;
        self.flush_notifications().await;
    }

    async fn clear_connection(&self) {
        let mut guard = self.connection.lock().await;
        *guard = None;
    }

    async fn wait_connection_task(&self) {
        let task = self.connection_task.lock().await.take();
        if let Some(mut task) = task {
            let sleep = tokio::time::sleep(SHUTDOWN_GRACE);
            tokio::pin!(sleep);
            tokio::select! {
                result = &mut task => {
                    if let Err(err) = result {
                        tracing::warn!(error = ?err, "acp bridge task panicked on shutdown");
                    }
                }
                _ = &mut sleep => {
                    task.abort();
                    let _ = task.await;
                }
            }
        }
    }

    async fn flush_notifications(&self) {
        self.notification_drain.wait_idle().await;
        // Drain queued `session/update` writes after the connection task has
        // stopped and every accepted notification append task has finished
        // enqueueing its row. Only then is it safe to close the sink.
        self.sink.flush().await;
    }
}

#[cfg(test)]
mod tests;
