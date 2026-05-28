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

use agent_client_protocol::schema::{
    AgentNotification, CancelNotification, CloseSessionRequest, InitializeRequest,
    InitializeResponse, ListSessionsRequest, ListSessionsResponse, LoadSessionRequest, McpServer,
    NewSessionRequest, NewSessionResponse, PromptRequest, ProtocolVersion,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    ResumeSessionRequest, SessionConfigOptionCategory, SessionId, SessionInfo,
    SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, SetSessionModelRequest,
    SetSessionModelResponse, StopReason,
};
use agent_client_protocol::{Agent, Client, ConnectionTo};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::process::{Child, Command};
use tokio::sync::{Mutex as TokioMutex, Notify, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::config::AgentConfig;
use crate::error::{Result, StackError};
use crate::runtime::agent::acp_codec::{enqueue_session_notification, resolve_acp_permission};
use crate::runtime::agent::inference_failure::{self, Classified};
use crate::runtime::mediation::permissions::PermissionService;
use crate::runtime::process_runner::{forward_host_env_tokio, kill_tokio_process_group};
use crate::state::FailureClass;

// External callers (CLI, supervisor, model_discovery, integration tests) wrote
// `crate::runtime::agent::acp_bridge::{SessionEventSink, StateStoreSessionSink, session_*}`
// before the extraction. Preserve those paths with re-exports so the split is
// internal to `runtime::agent`.
pub use crate::runtime::agent::acp_codec::{
    session_config_id_for_value, session_config_values, session_model_selection_for_value,
    session_model_values,
};
pub use crate::runtime::agent::session_sink::{SessionEventSink, StateStoreSessionSink};

/// Maximum time we wait for `initialize` to return before declaring the agent
/// unresponsive. Headless ACP agents handshake in milliseconds; anything more
/// than this is a configuration or compatibility problem.
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(15);

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
    LegacyModel,
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

/// Our owned view of the `initialize` response. Mirrors the protocol shape
/// but is independent of the SDK's `AgentCapabilities` type so our
/// `GET /v1/agent/capabilities` JSON contract stays stable across SDK
/// minor-version churn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCapabilitiesDto {
    pub protocol_version: u16,
    /// Raw JSON object of the agent's advertised capabilities. We surface it
    /// verbatim so clients can read every field today without the daemon
    /// growing a struct for each one. Named accessors land alongside the
    /// session API.
    pub capabilities: Value,
    /// `agentInfo.name` if the agent provided it. The spec says `SHOULD`,
    /// not `MUST`, so this is best-effort.
    pub agent_name: Option<String>,
    pub agent_title: Option<String>,
    pub agent_version: Option<String>,
}

impl AgentCapabilitiesDto {
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(self).map_err(|err| StackError::AgentInitializeFailed {
            reason: format!("failed to serialize agent capabilities: {err}"),
        })
    }

    /// Whether the agent advertised the `load_session` capability in its
    /// `initialize` response. Used to gate `POST /v1/sessions/{id}/load`.
    pub fn supports_load_session(&self) -> bool {
        self.capabilities
            .get("loadSession")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    pub fn supports_list_sessions(&self) -> bool {
        self.supports_session_capability("list")
    }

    pub fn supports_resume_session(&self) -> bool {
        self.supports_session_capability("resume")
    }

    pub fn supports_close_session(&self) -> bool {
        self.supports_session_capability("close")
    }

    fn supports_session_capability(&self, name: &str) -> bool {
        self.capabilities
            .get("sessionCapabilities")
            .and_then(Value::as_object)
            .and_then(|caps| caps.get(name))
            .is_some_and(Value::is_object)
    }

    fn from_initialize_response(response: &InitializeResponse) -> Result<Self> {
        // The SDK's `AgentCapabilities` is a typed struct that may rename
        // fields between minor versions; serialize through serde_json to keep
        // our durable storage and API contract independent of that surface.
        let raw_caps = serde_json::to_value(&response.agent_capabilities).map_err(|err| {
            StackError::AgentInitializeFailed {
                reason: format!("failed to serialize agent capabilities: {err}"),
            }
        })?;
        let protocol_version = serde_json::to_value(&response.protocol_version)
            .ok()
            .and_then(|v| v.as_u64())
            .and_then(|v| u16::try_from(v).ok())
            .unwrap_or(1);
        let (agent_name, agent_title, agent_version) = match serde_json::to_value(response) {
            Ok(Value::Object(map)) => {
                let info = map.get("agentInfo").cloned().unwrap_or(Value::Null);
                (
                    info.get("name").and_then(Value::as_str).map(str::to_owned),
                    info.get("title").and_then(Value::as_str).map(str::to_owned),
                    info.get("version")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                )
            }
            _ => (None, None, None),
        };
        Ok(Self {
            protocol_version,
            capabilities: raw_caps,
            agent_name,
            agent_title,
            agent_version,
        })
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
    session_model: Option<String>,
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
    idle: Notify,
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

    async fn wait_idle(&self) {
        loop {
            if self.active.load(Ordering::SeqCst) == 0 {
                return;
            }
            self.idle.notified().await;
        }
    }
}

impl Drop for NotificationGuard {
    fn drop(&mut self) {
        if self.drain.active.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.drain.idle.notify_waiters();
        }
    }
}

impl AcpBridge {
    /// Spawn `[agent].command` and complete the ACP `initialize` handshake.
    ///
    /// `env` is the resolved secret-name -> value map for `[agent].env`. We
    /// resolve the command path first, then `env_clear()` so only managed
    /// runtime context and explicitly resolved secrets reach the child.
    pub async fn spawn(
        agent: &AgentConfig,
        env: HashMap<String, String>,
        cwd: PathBuf,
        sink: Arc<dyn SessionEventSink>,
        permissions: Option<PermissionService>,
    ) -> Result<Self> {
        let command_path = resolve_command_path(&agent.command, &cwd).ok_or_else(|| {
            StackError::AgentInitializeFailed {
                reason: format!("agent command `{}` not found on PATH", agent.command),
            }
        })?;
        let mut command = Command::new(command_path);
        command
            .args(&agent.args)
            .current_dir(&cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            // stderr is the agent's log channel per
            // docs/ref/acp/protocol/transports.md:28; inherit so it shows up
            // alongside our daemon logs without an extra plumbing layer.
            .stderr(std::process::Stdio::inherit())
            .env_clear();
        // Runtime context is intentionally narrow: managed PATH so adapters
        // can find registry-installed harnesses, and HOME so agent wrappers
        // can find their own config/cache directories.
        if let Some(path) = agent_process_path() {
            command.env("PATH", path);
        }
        forward_host_env_tokio(&mut command, "HOME");
        for (name, value) in &env {
            if matches!(name.as_str(), "PATH" | "HOME") {
                tracing::warn!(
                    name = %name,
                    "refusing to inject `{name}` from `[agent].env` into agent process: reserved",
                );
                continue;
            }
            command.env(name, value);
        }
        // Fresh process group so a future SIGTERM-during-shutdown also
        // reaches MCP/tool grandchildren the agent forks.
        #[cfg(unix)]
        command.process_group(0);
        command.kill_on_drop(true);

        let mut child = command
            .spawn()
            .map_err(|source| StackError::AgentSpawnFailed { source })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| StackError::AgentInitializeFailed {
                reason: "agent stdin was not piped".to_owned(),
            })?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| StackError::AgentInitializeFailed {
                reason: "agent stdout was not piped".to_owned(),
            })?;

        let transport =
            agent_client_protocol::ByteStreams::new(stdin.compat_write(), stdout.compat());

        let (init_tx, init_rx) = oneshot::channel::<
            std::result::Result<(InitializeResponse, ConnectionTo<Agent>), String>,
        >();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let (exit_tx, exit_rx) = watch::channel(None);
        let planned_shutdown = Arc::new(AtomicBool::new(false));
        let planned_shutdown_for_task = Arc::clone(&planned_shutdown);
        let spawn_pid = child.id();

        // Notifications get persisted to the durable event log keyed by
        // session_id and then fanned out live through the event hub.
        // Non-session notifications are still logged-and-dropped.
        let notification_drain = Arc::new(NotificationDrain::default());
        let notification_sink = sink.clone();
        let notification_drain_for_task = Arc::clone(&notification_drain);
        let bridge_sink = sink.clone();

        // The SDK's Client.builder().connect_with(...) future drives the IO
        // loop until the closure returns. We spawn it as a tokio task so the
        // bridge handle can outlive the call site, and we use a oneshot
        // shutdown signal to ask the closure to wrap up cleanly.
        let permissions_for_task = permissions.clone();
        let connection_exit_tx = exit_tx.clone();
        let connection_task: JoinHandle<()> = tokio::spawn(async move {
            let run = Client
                .builder()
                .on_receive_request(
                    async move |request: RequestPermissionRequest, responder, _cx| {
                        let outcome = match permissions_for_task.as_ref() {
                            Some(service) => resolve_acp_permission(service, request).await,
                            None => {
                                // No permission service attached — tests that
                                // never decide must not block the agent.
                                RequestPermissionOutcome::Cancelled
                            }
                        };
                        responder.respond(RequestPermissionResponse::new(outcome))
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_notification(
                    async move |notification: AgentNotification, _cx| {
                        match notification {
                            AgentNotification::SessionNotification(session_note) => {
                                enqueue_session_notification(
                                    notification_sink.clone(),
                                    Arc::clone(&notification_drain_for_task),
                                    &session_note,
                                );
                            }
                            other => {
                                tracing::debug!(
                                    method = %other.method(),
                                    "acp bridge dropped non-session notification"
                                );
                            }
                        }
                        Ok(())
                    },
                    agent_client_protocol::on_receive_notification!(),
                )
                .connect_with(transport, async move |cx: ConnectionTo<Agent>| {
                    let response = cx
                        .send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await
                        .map_err(|err| err.to_string());
                    match response {
                        Ok(response) => {
                            // Send the connection handle out so the bridge
                            // can dispatch session methods after this closure
                            // parks. `cx` is Clone, so we keep the original
                            // here for any future closure-internal use.
                            let _ = init_tx.send(Ok((response, cx.clone())));
                        }
                        Err(reason) => {
                            let _ = init_tx.send(Err(reason));
                            // Returning an error from the closure tears down
                            // the connection. The caller has already seen
                            // the initialize failure via the oneshot.
                            return Err(agent_client_protocol::Error::internal_error());
                        }
                    }
                    // Hold the connection open until shutdown is signaled.
                    // Errors here are non-fatal: a dropped shutdown sender
                    // (e.g. bridge dropped without explicit shutdown) means
                    // "tear down now."
                    let _ = shutdown_rx.await;
                    Ok(())
                })
                .await;
            let planned = planned_shutdown_for_task.load(Ordering::SeqCst);
            let exit = match run {
                Ok(()) if planned => AcpBridgeExit {
                    pid: spawn_pid,
                    planned,
                    reason: AcpBridgeExitReason::Shutdown,
                    message: None,
                    exit_status: None,
                },
                Ok(()) => AcpBridgeExit {
                    pid: spawn_pid,
                    planned,
                    reason: AcpBridgeExitReason::ConnectionEnded,
                    message: None,
                    exit_status: None,
                },
                Err(err) => {
                    tracing::warn!(error = ?err, "acp bridge connection task exited with error");
                    AcpBridgeExit {
                        pid: spawn_pid,
                        planned,
                        reason: AcpBridgeExitReason::ConnectionError,
                        message: Some(err.to_string()),
                        exit_status: None,
                    }
                }
            };
            let _ = connection_exit_tx.send(Some(exit));
        });

        let (init_response, connection) = match timeout(INITIALIZE_TIMEOUT, init_rx).await {
            Ok(Ok(Ok((response, connection)))) => (response, connection),
            Ok(Ok(Err(reason))) => {
                fail_spawn(&mut child, connection_task).await;
                return Err(StackError::AgentInitializeFailed { reason });
            }
            Ok(Err(_)) => {
                fail_spawn(&mut child, connection_task).await;
                return Err(StackError::AgentInitializeFailed {
                    reason: "connection ended before initialize completed".to_owned(),
                });
            }
            Err(_) => {
                fail_spawn(&mut child, connection_task).await;
                return Err(StackError::AgentInitializeFailed {
                    reason: format!(
                        "initialize did not return within {}s",
                        INITIALIZE_TIMEOUT.as_secs()
                    ),
                });
            }
        };

        let capabilities = AgentCapabilitiesDto::from_initialize_response(&init_response)?;
        let session_model = agent.provider.as_ref().and_then(|provider| {
            if agent.id == "goose" {
                provider.model.clone()
            } else {
                None
            }
        });
        let child = Arc::new(TokioMutex::new(Some(child)));
        spawn_child_exit_watcher(
            Arc::clone(&child),
            Arc::clone(&planned_shutdown),
            exit_tx,
            spawn_pid,
        );

        Ok(Self {
            child,
            capabilities,
            connection: TokioMutex::new(Some(connection)),
            shutdown_tx: TokioMutex::new(Some(shutdown_tx)),
            connection_task: TokioMutex::new(Some(connection_task)),
            planned_shutdown,
            exit_rx,
            spawn_pid,
            sink: bridge_sink,
            notification_drain,
            session_model,
        })
    }

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

    async fn connection(&self) -> Result<ConnectionTo<Agent>> {
        let guard = self.connection.lock().await;
        guard.as_ref().cloned().ok_or(StackError::AgentNotRunning)
    }

    /// `session/new`. Always supported per ACP baseline.
    pub async fn new_session(
        &self,
        cwd: PathBuf,
        mcp_servers: Vec<McpServer>,
    ) -> Result<NewSessionResponse> {
        let connection = self.connection().await?;
        let mut request = NewSessionRequest::new(cwd);
        request.mcp_servers = mcp_servers;
        let response = connection
            .send_request(request)
            .block_task()
            .await
            .map_err(|err| StackError::AgentRequestFailed {
                method: "session/new",
                message: err.to_string(),
            })?;
        if let Some(model) = self.session_model.as_deref() {
            connection
                .send_request(SetSessionConfigOptionRequest::new(
                    response.session_id.clone(),
                    "model",
                    model.to_owned(),
                ))
                .block_task()
                .await
                .map_err(|err| StackError::AgentRequestFailed {
                    method: "session/set_config_option",
                    message: err.to_string(),
                })?;
        }
        Ok(response)
    }

    /// `session/list`. Requires the `sessionCapabilities.list` capability.
    pub async fn list_sessions(&self) -> Result<Vec<SessionInfo>> {
        if !self.capabilities.supports_list_sessions() {
            return Err(StackError::AgentUnsupportedCapability {
                name: "session/list",
            });
        }
        let connection = self.connection().await?;
        let mut sessions = Vec::new();
        let mut cursor = None;
        let mut seen_cursors = HashSet::new();
        loop {
            let request = ListSessionsRequest::new().cursor(cursor.clone());
            let response: ListSessionsResponse = connection
                .send_request(request)
                .block_task()
                .await
                .map_err(|err| StackError::AgentRequestFailed {
                    method: "session/list",
                    message: err.to_string(),
                })?;
            sessions.extend(response.sessions);
            let Some(next_cursor) = response.next_cursor else {
                return Ok(sessions);
            };
            if !seen_cursors.insert(next_cursor.clone()) {
                return Err(StackError::AgentRequestFailed {
                    method: "session/list",
                    message: format!("agent returned repeated pagination cursor `{next_cursor}`"),
                });
            }
            cursor = Some(next_cursor);
        }
    }

    pub async fn set_session_config_option(
        &self,
        session_id: SessionId,
        config_id: &str,
        value: &str,
    ) -> Result<SetSessionConfigOptionResponse> {
        let connection = self.connection().await?;
        let request =
            SetSessionConfigOptionRequest::new(session_id, config_id.to_owned(), value.to_owned());
        connection
            .send_request(request)
            .block_task()
            .await
            .map_err(|err| StackError::AgentRequestFailed {
                method: "session/set_config_option",
                message: err.to_string(),
            })
    }

    pub async fn set_session_model(
        &self,
        session_id: SessionId,
        model_id: &str,
    ) -> Result<SetSessionModelResponse> {
        let connection = self.connection().await?;
        let request = SetSessionModelRequest::new(session_id, model_id.to_owned());
        connection
            .send_request(request)
            .block_task()
            .await
            .map_err(|err| StackError::AgentRequestFailed {
                method: "session/set_model",
                message: err.to_string(),
            })
    }

    /// `session/load`. Requires the `loadSession` capability.
    pub async fn load_session(
        &self,
        session_id: SessionId,
        cwd: PathBuf,
        mcp_servers: Vec<McpServer>,
    ) -> Result<()> {
        if !self.capabilities.supports_load_session() {
            return Err(StackError::AgentUnsupportedCapability {
                name: "session/load",
            });
        }
        let connection = self.connection().await?;
        let request = LoadSessionRequest::new(session_id, cwd).mcp_servers(mcp_servers);
        connection
            .send_request(request)
            .block_task()
            .await
            .map_err(|err| StackError::AgentRequestFailed {
                method: "session/load",
                message: err.to_string(),
            })?;
        Ok(())
    }

    /// `session/resume`. Gated by the `unstable_session_resume` SDK feature
    /// (enabled at the workspace level). The agent may still reject if it
    /// does not implement resume — that surfaces as `agent.request_failed`.
    pub async fn resume_session(
        &self,
        session_id: SessionId,
        cwd: PathBuf,
        mcp_servers: Vec<McpServer>,
    ) -> Result<()> {
        if !self.capabilities.supports_resume_session() {
            return Err(StackError::AgentUnsupportedCapability {
                name: "session/resume",
            });
        }
        let connection = self.connection().await?;
        let request = ResumeSessionRequest::new(session_id, cwd).mcp_servers(mcp_servers);
        connection
            .send_request(request)
            .block_task()
            .await
            .map_err(|err| StackError::AgentRequestFailed {
                method: "session/resume",
                message: err.to_string(),
            })?;
        Ok(())
    }

    /// `session/close`. Gated by the `unstable_session_close` SDK feature.
    pub async fn close_session(&self, session_id: SessionId) -> Result<()> {
        if !self.capabilities.supports_close_session() {
            return Err(StackError::AgentUnsupportedCapability {
                name: "session/close",
            });
        }
        let connection = self.connection().await?;
        let request = CloseSessionRequest::new(session_id);
        connection
            .send_request(request)
            .block_task()
            .await
            .map_err(|err| StackError::AgentRequestFailed {
                method: "session/close",
                message: err.to_string(),
            })?;
        Ok(())
    }

    /// `session/prompt`. Awaits the turn's final response.
    ///
    /// On error, runs the inference-failure classifier so upstream HTTP
    /// failures (5xx, 429, etc.) become a typed `InferenceRequestFailed`
    /// variant; everything else falls back to `AgentRequestFailed`. The raw
    /// `err.to_string()` is never persisted: 4xx/5xx paths surface only the
    /// vetted reason label, and the generic fallback uses a sanitized message
    /// to avoid leaking URLs / headers / bodies / secrets into the state row.
    pub async fn prompt_session(&self, request: PromptRequest) -> Result<StopReason> {
        let connection = self.connection().await?;
        match connection.send_request(request).block_task().await {
            Ok(response) => Ok(response.stop_reason),
            Err(err) => {
                let classified = inference_failure::classify(&err);
                Err(map_prompt_error(classified))
            }
        }
    }

    /// `session/cancel` is a fire-and-forget notification.
    pub async fn cancel_session(&self, session_id: SessionId) -> Result<()> {
        let connection = self.connection().await?;
        connection
            .send_notification(CancelNotification::new(session_id))
            .map_err(|err| StackError::AgentRequestFailed {
                method: "session/cancel",
                message: err.to_string(),
            })?;
        Ok(())
    }

    /// Gracefully tear down the agent: signal the connection task to return,
    /// then close stdin / wait / SIGKILL the child on a bounded timeline.
    /// Returns the exit status if available. Idempotent: a second call sees
    /// every field already `None` and returns `Ok(None)`.
    pub async fn shutdown(&self) -> Result<Option<i32>> {
        self.planned_shutdown.store(true, Ordering::SeqCst);
        // Clear the cloneable handle so any in-flight session calls fail
        // fast with `AgentNotRunning` rather than hanging on a dead IO loop.
        {
            let mut guard = self.connection.lock().await;
            *guard = None;
        }
        if let Some(tx) = self.shutdown_tx.lock().await.take() {
            let _ = tx.send(());
        }
        let task = self.connection_task.lock().await.take();
        if let Some(mut task) = task {
            // Wait for the SDK closure to return. If it doesn't within the
            // grace window, abort the task explicitly so the IO loop does
            // not continue running detached after shutdown returns.
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
        self.notification_drain.wait_idle().await;
        // Drain queued `session/update` writes after the connection task has
        // stopped and every accepted notification append task has finished
        // enqueueing its row. Only then is it safe to close the sink.
        self.sink.flush().await;

        let mut child = match self.child.lock().await.take() {
            Some(child) => child,
            None => return Ok(None),
        };

        // First try to let the child notice stdin closure and exit on its
        // own. If it doesn't, escalate to a process-group SIGKILL so any
        // grandchildren the agent forked (MCP servers, tool subprocesses)
        // also die with the daemon — the bridge spawned with
        // `process_group(0)`, so the child is its own pgid leader.
        let status = match timeout(SHUTDOWN_GRACE, child.wait()).await {
            Ok(Ok(status)) => Some(status),
            Ok(Err(err)) => {
                tracing::warn!(error = ?err, "acp bridge: wait failed");
                kill_tokio_process_group(&mut child);
                None
            }
            Err(_) => {
                kill_tokio_process_group(&mut child);
                let _ = child.wait().await.ok();
                None
            }
        };

        Ok(status.and_then(|s| s.code()))
    }
}

fn spawn_child_exit_watcher(
    child: Arc<TokioMutex<Option<Child>>>,
    planned_shutdown: Arc<AtomicBool>,
    exit_tx: watch::Sender<Option<AcpBridgeExit>>,
    spawn_pid: Option<u32>,
) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(CHILD_EXIT_POLL_INTERVAL).await;
            let exit_status = {
                let mut guard = child.lock().await;
                let Some(child) = guard.as_mut() else {
                    return;
                };
                match child.try_wait() {
                    Ok(Some(status)) => {
                        *guard = None;
                        Some(status.code())
                    }
                    Ok(None) => None,
                    Err(err) => {
                        tracing::warn!(error = ?err, "acp bridge child exit poll failed");
                        None
                    }
                }
            };
            let Some(exit_status) = exit_status else {
                continue;
            };
            let planned = planned_shutdown.load(Ordering::SeqCst);
            let _ = exit_tx.send(Some(AcpBridgeExit {
                pid: spawn_pid,
                planned,
                reason: AcpBridgeExitReason::ProcessExited,
                message: None,
                exit_status,
            }));
            return;
        }
    });
}

/// Translate a classified prompt failure into the appropriate `StackError`
/// variant. Only the classifier's vetted fields (status code + static reason
/// label) cross into the error; the raw upstream message is dropped so the
/// state row carries no URLs / headers / bodies / secrets.
fn map_prompt_error(classified: Classified) -> StackError {
    match classified.class {
        FailureClass::Inference5xx | FailureClass::Inference4xx => match classified.status_code {
            Some(code) if code != 0 => StackError::InferenceRequestFailed {
                status_code: code,
                reason_category: classified.reason_category,
            },
            // Defensive fallback: classifier returned an inference class but no
            // status code. Treat as a generic agent failure rather than
            // persisting `status_code = 0`, which would be a meaningless row.
            _ => StackError::AgentRequestFailed {
                method: "session/prompt",
                message: "prompt request failed".to_owned(),
            },
        },
        _ => StackError::AgentRequestFailed {
            method: "session/prompt",
            message: "prompt request failed".to_owned(),
        },
    }
}

/// Spawn-error path cleanup: abort the SDK task, kill the entire process
/// group, then reap the child. Without the pgroup kill, any grandchildren
/// the agent forked between spawn and initialize-failure survive the
/// failure, defeating the whole point of `process_group(0)`.
async fn fail_spawn(child: &mut tokio::process::Child, connection_task: JoinHandle<()>) {
    connection_task.abort();
    let _ = connection_task.await;
    kill_tokio_process_group(child);
    let _ = child.wait().await;
}

/// Resolve a configured command path the same way process spawning will:
/// absolute paths as-is, relative paths with a slash against `cwd`, and bare
/// names through the daemon's current PATH before the child environment is
/// cleared.
pub(crate) fn resolve_command_path(command: &str, cwd: &Path) -> Option<PathBuf> {
    if command.is_empty() {
        return None;
    }
    let as_path = Path::new(command);
    if as_path.is_absolute() {
        return if as_path.is_file() {
            Some(as_path.to_path_buf())
        } else {
            None
        };
    }
    if command.contains('/') {
        let candidate = cwd.join(command);
        return if candidate.is_file() {
            Some(candidate)
        } else {
            None
        };
    }
    for dir in command_search_paths() {
        let candidate = dir.join(command);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn agent_process_path() -> Option<std::ffi::OsString> {
    let paths = command_search_paths();
    if paths.is_empty() {
        None
    } else {
        std::env::join_paths(paths).ok()
    }
}

fn command_search_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        paths.push(PathBuf::from(home).join(".local").join("bin"));
    }
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    paths
}
