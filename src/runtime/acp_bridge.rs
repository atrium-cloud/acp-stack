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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agent_client_protocol::schema::{
    AgentNotification, CancelNotification, CloseSessionRequest, InitializeRequest,
    InitializeResponse, LoadSessionRequest, McpServer, NewSessionRequest, NewSessionResponse,
    PermissionOptionId, PromptRequest, ProtocolVersion, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, ResumeSessionRequest,
    SelectedPermissionOutcome, SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory,
    SessionConfigSelectOptions, SessionId, SessionNotification, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, SetSessionModelRequest, SetSessionModelResponse, StopReason,
};
use agent_client_protocol::{Agent, Client, ConnectionTo};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::process::{Child, Command};
use tokio::sync::{Mutex as TokioMutex, Notify, oneshot};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::config::AgentConfig;
use crate::error::{Result, StackError};
use crate::events::EventHub;
use crate::permissions::{NewPermission, PermissionOutcome, PermissionService, PermissionSource};
use crate::state::StateStore;

/// Maximum time we wait for `initialize` to return before declaring the agent
/// unresponsive. Headless ACP agents handshake in milliseconds; anything more
/// than this is a configuration or compatibility problem.
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(15);

/// Maximum time we wait between sending the shutdown signal and SIGKILLing
/// the agent child. The closure should return immediately once the oneshot
/// fires; if it does not, the child is misbehaving and we cut losses.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

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

    fn matches(self, category: &SessionConfigOptionCategory) -> bool {
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

/// Sink for ACP `session/update` notifications. The bridge writes through this
/// trait instead of holding a `StateStore` directly, so tests can substitute
/// an in-memory sink without standing up a SQLite file.
///
/// `append` returns a future so a real implementation can durably persist the
/// event before the notification handler returns; otherwise a fast shutdown
/// would drop in-flight writes. `flush` waits for any background writer task
/// owned by the sink to drain; the bridge calls it during graceful shutdown.
pub trait SessionEventSink: Send + Sync + 'static {
    fn append<'a>(
        &'a self,
        session_id: &'a str,
        kind: &'a str,
        payload_json: &'a str,
    ) -> futures::future::BoxFuture<'a, ()>;

    fn flush<'a>(&'a self) -> futures::future::BoxFuture<'a, ()> {
        Box::pin(async {})
    }
}

/// `SessionEventSink` backed by the daemon's real `StateStore`.
///
/// Session-update writes flow through a **bounded** mpsc channel into a
/// single background writer task. The bound provides backpressure: a noisy
/// agent that emits updates faster than SQLite drains them blocks at
/// `append`, which yields back to the SDK's notification handler and lets
/// the event loop tick (it never spin-waits, since `send` is async). Without
/// the bound a runaway agent could exhaust daemon memory before any HTTP
/// limit kicks in.
///
/// `flush()` drops the sender, the writer task drains the remaining queue,
/// and we await it during graceful shutdown so no notification rows are lost.
pub struct StateStoreSessionSink {
    tx: TokioMutex<Option<tokio::sync::mpsc::Sender<SessionEventRow>>>,
    writer: TokioMutex<Option<JoinHandle<()>>>,
}

struct SessionEventRow {
    session_id: String,
    kind: String,
    payload_json: String,
}

/// Normalize the agent's reported token/context usage if the inbound
/// `session/update` payload carries it. ACP itself has no standard shape, so
/// we recognize the conventions used by Claude and other agents: a `usage`
/// object reachable at the top level, under `update.usage`, or under
/// `prompt_response.usage`. Fields outside `input_tokens`, `output_tokens`,
/// and `context_window_max` (also accepting the legacy `context_window`
/// alias) are ignored. Returns `None` if none of those fields parse as a
/// positive integer — callers must not emit a `usage.reported` event in that
/// case because every aggregate would still be null.
fn extract_usage_payload(session_id: &str, payload_json: &str) -> Option<serde_json::Value> {
    let value: serde_json::Value = serde_json::from_str(payload_json).ok()?;
    let usage = locate_usage_object(&value)?;
    let input_tokens = read_token_field(usage, "input_tokens");
    let output_tokens = read_token_field(usage, "output_tokens");
    let context_window_max = read_token_field(usage, "context_window_max")
        .or_else(|| read_token_field(usage, "context_window"));
    if input_tokens.is_none() && output_tokens.is_none() && context_window_max.is_none() {
        return None;
    }
    let mut out = serde_json::Map::new();
    out.insert(
        "session_id".to_owned(),
        serde_json::Value::String(session_id.to_owned()),
    );
    if let Some(v) = input_tokens {
        out.insert(
            "input_tokens".to_owned(),
            serde_json::Value::Number(serde_json::Number::from(v)),
        );
    }
    if let Some(v) = output_tokens {
        out.insert(
            "output_tokens".to_owned(),
            serde_json::Value::Number(serde_json::Number::from(v)),
        );
    }
    if let Some(v) = context_window_max {
        out.insert(
            "context_window_max".to_owned(),
            serde_json::Value::Number(serde_json::Number::from(v)),
        );
    }
    Some(serde_json::Value::Object(out))
}

fn locate_usage_object(value: &serde_json::Value) -> Option<&serde_json::Value> {
    if let Some(obj) = value.get("usage") {
        if obj.is_object() {
            return Some(obj);
        }
    }
    if let Some(update) = value.get("update").and_then(|v| v.get("usage")) {
        if update.is_object() {
            return Some(update);
        }
    }
    if let Some(prompt_response) = value.get("prompt_response").and_then(|v| v.get("usage")) {
        if prompt_response.is_object() {
            return Some(prompt_response);
        }
    }
    if let Some(meta_usage) = value.get("meta").and_then(|v| v.get("usage")) {
        if meta_usage.is_object() {
            return Some(meta_usage);
        }
    }
    None
}

fn read_token_field(usage: &serde_json::Value, key: &str) -> Option<i64> {
    let raw = usage.get(key)?;
    if let Some(n) = raw.as_i64() {
        return if n >= 0 { Some(n) } else { None };
    }
    if let Some(n) = raw.as_u64() {
        return i64::try_from(n).ok();
    }
    None
}

/// Backpressure buffer for unwritten ACP session updates. Sized so a typical
/// streaming turn (text chunks, tool calls) fits comfortably without ever
/// blocking, but small enough that a pathological agent can't grow daemon
/// memory by gigabytes before SQLite catches up.
pub(crate) const SESSION_EVENT_BUFFER: usize = 1024;

impl StateStoreSessionSink {
    pub fn new(state: Arc<TokioMutex<StateStore>>, event_hub: EventHub) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<SessionEventRow>(SESSION_EVENT_BUFFER);
        let writer_event_hub = event_hub.clone();
        let writer = tokio::spawn(async move {
            while let Some(row) = rx.recv().await {
                let guard = state.lock().await;
                match guard.append_session_event_with_source(
                    &row.session_id,
                    "info",
                    &row.kind,
                    crate::state::EVENT_SOURCE_ACP,
                    "ACP session update",
                    &row.payload_json,
                ) {
                    Ok(event) => {
                        writer_event_hub.publish_session_update(
                            &row.session_id,
                            &event,
                            &row.payload_json,
                        );
                        // Best-effort token / context usage capture. ACP does
                        // not standardize a usage shape, but Claude (and
                        // others) emit it on `update.usage.*` or on prompt
                        // completion. Persist a normalized `usage.reported`
                        // event when we recognize the shape; ignore otherwise.
                        if let Some(usage) =
                            extract_usage_payload(&row.session_id, &row.payload_json)
                        {
                            if let Ok(usage_text) = serde_json::to_string(&usage) {
                                if let Err(err) = guard.append_session_event_with_source(
                                    &row.session_id,
                                    "info",
                                    "usage.reported",
                                    crate::state::EVENT_SOURCE_ACP,
                                    "agent usage reported",
                                    &usage_text,
                                ) {
                                    tracing::warn!(
                                        error = %err,
                                        session_id = %row.session_id,
                                        "failed to persist usage.reported event"
                                    );
                                }
                            }
                        }
                    }
                    Err(err) => {
                        tracing::warn!(
                            error = %err,
                            session_id = %row.session_id,
                            "failed to persist ACP session update"
                        );
                    }
                }
            }
        });
        Self {
            tx: TokioMutex::new(Some(tx)),
            writer: TokioMutex::new(Some(writer)),
        }
    }
}

impl SessionEventSink for StateStoreSessionSink {
    fn append<'a>(
        &'a self,
        session_id: &'a str,
        kind: &'a str,
        payload_json: &'a str,
    ) -> futures::future::BoxFuture<'a, ()> {
        Box::pin(async move {
            let sender = {
                let guard = self.tx.lock().await;
                match guard.as_ref() {
                    Some(tx) => tx.clone(),
                    None => {
                        tracing::warn!(
                            session_id = %session_id,
                            "session event sink is closed; dropping update"
                        );
                        return;
                    }
                }
            };
            if let Err(err) = sender
                .send(SessionEventRow {
                    session_id: session_id.to_owned(),
                    kind: kind.to_owned(),
                    payload_json: payload_json.to_owned(),
                })
                .await
            {
                tracing::warn!(
                    error = %err,
                    session_id = %session_id,
                    "session event writer task ended; dropping update"
                );
            }
        })
    }

    fn flush<'a>(&'a self) -> futures::future::BoxFuture<'a, ()> {
        Box::pin(async move {
            {
                let mut guard = self.tx.lock().await;
                // Dropping the sender lets the writer task observe EOF and
                // drain its queue before exiting. Idempotent.
                *guard = None;
            }
            let writer = self.writer.lock().await.take();
            if let Some(task) = writer {
                if let Err(err) = task.await {
                    tracing::warn!(
                        error = ?err,
                        "session event writer task did not exit cleanly"
                    );
                }
            }
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
    child: TokioMutex<Option<Child>>,
    capabilities: AgentCapabilitiesDto,
    /// Cloneable handle for sending requests/notifications to the agent.
    /// Populated inside the connect closure before it parks on `shutdown_rx`,
    /// so callers outside the closure can dispatch session methods. Wrapped
    /// in an `Option` because `shutdown()` clears it before tearing down.
    connection: TokioMutex<Option<ConnectionTo<Agent>>>,
    shutdown_tx: TokioMutex<Option<oneshot::Sender<()>>>,
    connection_task: TokioMutex<Option<JoinHandle<()>>>,
    spawn_pid: Option<u32>,
    /// Held so `shutdown()` can flush any pending `session/update` writes the
    /// sink's background writer task has queued.
    sink: Arc<dyn SessionEventSink>,
    notification_drain: Arc<NotificationDrain>,
}

#[derive(Default)]
struct NotificationDrain {
    active: AtomicUsize,
    idle: Notify,
}

struct NotificationGuard {
    drain: Arc<NotificationDrain>,
}

impl NotificationDrain {
    fn enter(self: &Arc<Self>) -> NotificationGuard {
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
        forward_host_env(&mut command, "HOME");
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
            if let Err(err) = run {
                tracing::warn!(error = ?err, "acp bridge connection task exited with error");
            }
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
        let pid = child.id();

        Ok(Self {
            child: TokioMutex::new(Some(child)),
            capabilities,
            connection: TokioMutex::new(Some(connection)),
            shutdown_tx: TokioMutex::new(Some(shutdown_tx)),
            connection_task: TokioMutex::new(Some(connection_task)),
            spawn_pid: pid,
            sink: bridge_sink,
            notification_drain,
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
        connection
            .send_request(request)
            .block_task()
            .await
            .map_err(|err| StackError::AgentRequestFailed {
                method: "session/new",
                message: err.to_string(),
            })
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
    pub async fn prompt_session(&self, request: PromptRequest) -> Result<StopReason> {
        let connection = self.connection().await?;
        let response = connection
            .send_request(request)
            .block_task()
            .await
            .map_err(|err| StackError::AgentRequestFailed {
                method: "session/prompt",
                message: err.to_string(),
            })?;
        Ok(response.stop_reason)
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
                kill_child_process_group(&mut child);
                None
            }
            Err(_) => {
                kill_child_process_group(&mut child);
                let _ = child.wait().await.ok();
                None
            }
        };

        Ok(status.and_then(|s| s.code()))
    }
}

pub fn session_config_id_for_value(
    config_options: Option<&[SessionConfigOption]>,
    category: AgentSessionConfigCategory,
    value: &str,
) -> Result<String> {
    let Some(config_options) = config_options else {
        return Err(StackError::AgentConfigProvision {
            path: PathBuf::from("ACP session config options"),
            reason: format!(
                "agent did not advertise a `{}` session config option",
                category.id()
            ),
        });
    };
    for option in config_options {
        let category_matches = option
            .category
            .as_ref()
            .is_some_and(|option_category| category.matches(option_category));
        let id_matches = option.id.0.as_ref() == category.id();
        if (category_matches || id_matches) && session_config_option_contains_value(option, value) {
            return Ok(option.id.0.to_string());
        }
    }
    Err(StackError::AgentConfigProvision {
        path: PathBuf::from("ACP session config options"),
        reason: format!(
            "agent did not advertise `{value}` as an available `{}`",
            category.id()
        ),
    })
}

pub fn session_config_values(
    config_options: Option<&[SessionConfigOption]>,
    category: AgentSessionConfigCategory,
) -> Result<Vec<String>> {
    let Some(config_options) = config_options else {
        return Err(StackError::AgentConfigProvision {
            path: PathBuf::from("ACP session config options"),
            reason: format!(
                "agent did not advertise a `{}` session config option",
                category.id()
            ),
        });
    };
    for option in config_options {
        let category_matches = option
            .category
            .as_ref()
            .is_some_and(|option_category| category.matches(option_category));
        let id_matches = option.id.0.as_ref() == category.id();
        if category_matches || id_matches {
            let mut values = session_config_option_values(option);
            values.sort();
            values.dedup();
            return Ok(values);
        }
    }
    Err(StackError::AgentConfigProvision {
        path: PathBuf::from("ACP session config options"),
        reason: format!(
            "agent did not advertise a `{}` session config option",
            category.id()
        ),
    })
}

pub fn session_model_selection_for_value(
    response: &NewSessionResponse,
    value: &str,
) -> Result<AgentSessionModelSelection> {
    if let Some(config_options) = response.config_options.as_deref()
        && let Ok(config_id) = session_config_id_for_value(
            Some(config_options),
            AgentSessionConfigCategory::Model,
            value,
        )
    {
        return Ok(AgentSessionModelSelection::ConfigOption { config_id });
    }
    if legacy_session_model_values(response)
        .iter()
        .any(|candidate| candidate == value)
    {
        return Ok(AgentSessionModelSelection::LegacyModel);
    }
    Err(StackError::AgentConfigProvision {
        path: PathBuf::from("ACP session config options"),
        reason: format!("agent did not advertise `{value}` as an available `model`"),
    })
}

pub fn session_model_values(response: &NewSessionResponse) -> Result<Vec<String>> {
    if let Some(config_options) = response.config_options.as_deref()
        && let Ok(values) =
            session_config_values(Some(config_options), AgentSessionConfigCategory::Model)
    {
        return Ok(values);
    }
    let mut values = legacy_session_model_values(response);
    values.sort();
    values.dedup();
    if values.is_empty() {
        return Err(StackError::AgentConfigProvision {
            path: PathBuf::from("ACP session config options"),
            reason: "agent did not advertise a `model` session config option".to_owned(),
        });
    }
    Ok(values)
}

fn legacy_session_model_values(response: &NewSessionResponse) -> Vec<String> {
    response
        .models
        .as_ref()
        .map(|models| {
            models
                .available_models
                .iter()
                .map(|model| model.model_id.0.to_string())
                .collect()
        })
        .unwrap_or_default()
}

fn session_config_option_contains_value(option: &SessionConfigOption, value: &str) -> bool {
    session_config_option_values(option)
        .iter()
        .any(|candidate| candidate == value)
}

fn session_config_option_values(option: &SessionConfigOption) -> Vec<String> {
    match &option.kind {
        SessionConfigKind::Select(select) => match &select.options {
            SessionConfigSelectOptions::Ungrouped(options) => options
                .iter()
                .map(|option| option.value.0.to_string())
                .collect(),
            SessionConfigSelectOptions::Grouped(groups) => groups
                .iter()
                .flat_map(|group| group.options.iter())
                .map(|option| option.value.0.to_string())
                .collect(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// Forward a `session/request_permission` request through the durable
/// PermissionService, await the decision, and translate the result back to
/// the ACP `RequestPermissionOutcome`. Falls back to `Cancelled` on every
/// failure so the agent's prompt turn always settles deterministically.
pub(crate) async fn resolve_acp_permission(
    service: &PermissionService,
    request: RequestPermissionRequest,
) -> RequestPermissionOutcome {
    // Serialize the full request for the durable detail record. The schema
    // type is JSON-friendly; failure here only happens for non-JSON-safe
    // values, which the schema does not contain.
    let detail = match serde_json::to_value(&request) {
        Ok(value) => value,
        Err(err) => {
            tracing::warn!(error = %err, "failed to serialize permission request");
            return RequestPermissionOutcome::Cancelled;
        }
    };
    let session_id = request.session_id.0.to_string();
    let first_option_id = request
        .options
        .first()
        .map(|opt| opt.option_id.0.to_string());

    let (_record, rx) = match service
        .request(NewPermission {
            source: PermissionSource::Acp,
            requester: Some(format!("session:{session_id}")),
            subject_id: Some(session_id),
            detail,
        })
        .await
    {
        Ok(pair) => pair,
        Err(err) => {
            tracing::warn!(error = %err, "permission service rejected ACP passthrough");
            return RequestPermissionOutcome::Cancelled;
        }
    };

    match rx.await {
        Ok(PermissionOutcome::Approved { option_id, .. }) => {
            let chosen = option_id.or(first_option_id);
            match chosen {
                Some(id) => RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                    PermissionOptionId::new(id),
                )),
                None => RequestPermissionOutcome::Cancelled,
            }
        }
        _ => RequestPermissionOutcome::Cancelled,
    }
}

fn enqueue_session_notification(
    sink: Arc<dyn SessionEventSink>,
    drain: Arc<NotificationDrain>,
    note: &SessionNotification,
) {
    // Serialize the verbatim notification payload so downstream queriers can
    // reconstruct the full ACP update without re-deriving from the typed enum.
    let payload = match serde_json::to_string(&note) {
        Ok(payload) => payload,
        Err(err) => {
            tracing::warn!(error = %err, "failed to serialize session/update; dropping");
            return;
        }
    };
    let session_id = note.session_id.0.to_string();
    let guard = drain.enter();
    tokio::spawn(async move {
        sink.append(&session_id, "session.update", &payload).await;
        drop(guard);
    });
}

#[cfg(unix)]
fn kill_child_process_group(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        // SAFETY: libc::kill with a negative pid signals the process group;
        // we created this child with process_group(0), so its pid is also
        // its pgid. SIGKILL is async-signal-safe.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn kill_child_process_group(child: &mut tokio::process::Child) {
    // No portable equivalent on non-Unix; fall back to the direct child.
    let _ = child.start_kill();
}

/// Spawn-error path cleanup: abort the SDK task, kill the entire process
/// group, then reap the child. Without the pgroup kill, any grandchildren
/// the agent forked between spawn and initialize-failure survive the
/// failure, defeating the whole point of `process_group(0)`.
async fn fail_spawn(child: &mut tokio::process::Child, connection_task: JoinHandle<()>) {
    connection_task.abort();
    let _ = connection_task.await;
    kill_child_process_group(child);
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

fn forward_host_env(command: &mut Command, name: &str) {
    if let Some(value) = std::env::var_os(name) {
        command.env(name, value);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PermissionTimeoutAction;
    use crate::permissions::PermissionService;
    use crate::state::StateStore;
    use agent_client_protocol::schema::{
        PermissionOption, PermissionOptionId, PermissionOptionKind, RequestPermissionRequest,
        SessionId, ToolCallId, ToolCallUpdate, ToolCallUpdateFields,
    };
    use std::time::Duration;
    use tokio::sync::Mutex as TokioMutex;

    fn fake_request(session_id: &str) -> RequestPermissionRequest {
        RequestPermissionRequest::new(
            SessionId::new(session_id),
            ToolCallUpdate::new(ToolCallId::new("tc_1"), ToolCallUpdateFields::default()),
            vec![PermissionOption::new(
                PermissionOptionId::new("allow"),
                "Allow",
                PermissionOptionKind::AllowOnce,
            )],
        )
    }

    async fn fresh_service() -> (tempfile::TempDir, PermissionService) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.sqlite");
        let store = StateStore::open(&path).expect("open");
        store.migrate().expect("migrate");
        let state = Arc::new(TokioMutex::new(store));
        let events = EventHub::new();
        (
            dir,
            PermissionService::new(
                state,
                events,
                Duration::from_secs(60),
                PermissionTimeoutAction::Deny,
            ),
        )
    }

    #[tokio::test]
    async fn approve_passthrough_returns_selected_option() {
        let (_dir, service) = fresh_service().await;
        let request = fake_request("sess_test");
        let service_for_task = service.clone();
        let outcome_task =
            tokio::spawn(async move { resolve_acp_permission(&service_for_task, request).await });

        // Drain the new permission row + approve it.
        let mut id = None;
        for _ in 0..50 {
            let pending = service.pending(10).await.expect("pending");
            if let Some(first) = pending.first() {
                id = Some(first.id.clone());
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let perm_id = id.expect("permission row must appear");
        service
            .approve(&perm_id, Some("allow".to_owned()), None, "session-key")
            .await
            .expect("approve");

        let outcome = outcome_task.await.expect("task joins");
        match outcome {
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome { option_id, .. }) => {
                assert_eq!(option_id.0.as_ref(), "allow");
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[tokio::test]
    async fn deny_passthrough_returns_cancelled() {
        let (_dir, service) = fresh_service().await;
        let request = fake_request("sess_test");
        let service_for_task = service.clone();
        let outcome_task =
            tokio::spawn(async move { resolve_acp_permission(&service_for_task, request).await });

        let mut id = None;
        for _ in 0..50 {
            let pending = service.pending(10).await.expect("pending");
            if let Some(first) = pending.first() {
                id = Some(first.id.clone());
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let perm_id = id.expect("permission row must appear");
        service
            .deny(&perm_id, None, "session-key")
            .await
            .expect("deny");

        let outcome = outcome_task.await.expect("task joins");
        assert!(matches!(outcome, RequestPermissionOutcome::Cancelled));
    }

    #[test]
    fn session_config_helpers_validate_select_values_by_category() {
        let options: Vec<SessionConfigOption> = serde_json::from_str(
            r#"[
                {
                    "id": "agent-model",
                    "name": "Model",
                    "category": "model",
                    "type": "select",
                    "currentValue": "openai/gpt-5.5",
                    "options": [
                        {"value": "openai/gpt-5.5", "name": "GPT-5.5"},
                        {"value": "anthropic/claude-sonnet-4-5", "name": "Claude Sonnet 4.5"}
                    ]
                },
                {
                    "id": "mode",
                    "name": "Mode",
                    "category": "mode",
                    "type": "select",
                    "currentValue": "smart",
                    "options": [
                        {"value": "smart", "name": "Smart"},
                        {"value": "fast", "name": "Fast"}
                    ]
                }
            ]"#,
        )
        .expect("session config options deserialize");

        let model_id = session_config_id_for_value(
            Some(&options),
            AgentSessionConfigCategory::Model,
            "openai/gpt-5.5",
        )
        .expect("model value should be accepted");
        assert_eq!(model_id, "agent-model");
        assert_eq!(
            session_config_values(Some(&options), AgentSessionConfigCategory::Mode)
                .expect("mode values"),
            ["fast", "smart"]
        );
        let err = session_config_id_for_value(
            Some(&options),
            AgentSessionConfigCategory::Model,
            "openai/not-advertised",
        )
        .expect_err("unknown model should be rejected");
        assert!(err.to_string().contains("openai/not-advertised"));
    }

    #[test]
    fn session_model_helpers_accept_legacy_model_state() {
        let response: NewSessionResponse = serde_json::from_str(
            r#"{
                "sessionId": "sess_legacy",
                "models": {
                    "currentModelId": "opencode-go/deepseek-v4-flash",
                    "availableModels": [
                        {
                            "modelId": "opencode-go/deepseek-v4-flash",
                            "name": "DeepSeek V4 Flash"
                        }
                    ]
                }
            }"#,
        )
        .expect("legacy model state deserializes");

        assert_eq!(
            session_model_values(&response).expect("legacy model values"),
            ["opencode-go/deepseek-v4-flash"]
        );
        assert_eq!(
            session_model_selection_for_value(&response, "opencode-go/deepseek-v4-flash")
                .expect("legacy model should validate"),
            AgentSessionModelSelection::LegacyModel
        );
    }

    #[test]
    fn extract_usage_payload_picks_up_top_level_usage_object() {
        let payload =
            r#"{"usage": {"input_tokens": 12, "output_tokens": 34, "context_window_max": 200000}}"#;
        let usage =
            super::extract_usage_payload("sess_x", payload).expect("usage should be extracted");
        assert_eq!(usage["input_tokens"].as_i64(), Some(12));
        assert_eq!(usage["output_tokens"].as_i64(), Some(34));
        assert_eq!(usage["context_window_max"].as_i64(), Some(200000));
        assert_eq!(usage["session_id"].as_str(), Some("sess_x"));
    }

    #[test]
    fn extract_usage_payload_walks_nested_paths() {
        let payload = r#"{"update": {"usage": {"input_tokens": 5}}}"#;
        let usage =
            super::extract_usage_payload("sess_y", payload).expect("usage should be extracted");
        assert_eq!(usage["input_tokens"].as_i64(), Some(5));
        // Output tokens absent — must NOT be serialized rather than written as 0.
        assert!(usage.get("output_tokens").is_none());
    }

    #[test]
    fn extract_usage_payload_returns_none_when_shape_unknown() {
        assert!(super::extract_usage_payload("sess_z", "{}").is_none());
        assert!(super::extract_usage_payload("sess_z", r#"{"update":{"foo":"bar"}}"#).is_none());
        assert!(super::extract_usage_payload("sess_z", "not-json").is_none());
    }

    #[test]
    fn extract_usage_payload_rejects_negative_numbers() {
        let payload = r#"{"usage": {"input_tokens": -5, "output_tokens": 3}}"#;
        let usage = super::extract_usage_payload("s", payload).expect("partial usage");
        // Negative tokens were dropped; output tokens preserved.
        assert!(usage.get("input_tokens").is_none());
        assert_eq!(usage["output_tokens"].as_i64(), Some(3));
    }
}
