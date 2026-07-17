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
    CloseSessionRequest, CreateTerminalRequest, FileSystemCapabilities, ForkSessionResponse,
    InitializeRequest, InitializeResponse, KillTerminalRequest, ListSessionsRequest,
    ListSessionsResponse, LoadSessionRequest, McpServer, NewSessionRequest, NewSessionResponse,
    PromptRequest, PromptResponse, ReadTextFileRequest, ReleaseTerminalRequest,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    ResumeSessionRequest, SessionConfigOptionCategory, SessionConfigOptionsCapabilities,
    SessionConfigValueId, SessionId, SessionInfo, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, TerminalOutputRequest, WaitForTerminalExitRequest,
    WriteTextFileRequest,
};
use agent_client_protocol::{
    Agent, Client, ConnectionTo, JsonRpcMessage, JsonRpcRequest, UntypedMessage,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::process::{Child, Command};
use tokio::sync::{Mutex as TokioMutex, Notify, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::config::AgentConfig;
use crate::error::{Result, StackError};
use crate::runtime::agent::acp_codec::{
    enqueue_session_notification, handle_read_text_file, handle_write_text_file,
    resolve_acp_permission, spawn_session_notification_queue,
};
use crate::runtime::agent::acp_terminal::{
    TerminalHandlerContext, TerminalRegistry, handle_create_terminal, handle_kill_terminal,
    handle_release_terminal, handle_terminal_output, handle_wait_for_terminal_exit,
};
use crate::runtime::agent::inference_failure::{self, Classified};
use crate::runtime::mediation::permissions::PermissionService;
use crate::runtime::process_runner::{forward_host_env_tokio, kill_tokio_process_group};
use crate::state::FailureClass;

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

pub(crate) const KIMI_CODE_AGENT_ID: &str = "kimi";
const KIMI_API_KEY_ENV: &str = "KIMI_API_KEY";
const KIMI_MODEL_API_KEY_ENV: &str = "KIMI_MODEL_API_KEY";
const KIMI_MODEL_NAME_ENV: &str = "KIMI_MODEL_NAME";
const KIMI_MODEL_BASE_URL_ENV: &str = "KIMI_MODEL_BASE_URL";
// Kimi Code requires a model before its ACP process can initialize. Init pins
// this default into config when `--model` is not passed, and the launch env
// falls back to it when a hand-edited config omits `agent.model`. It is the
// one id available on every subscription tier, whereas `k3` is gated to
// Moderato and above.
pub(crate) const KIMI_CODE_DEFAULT_MODEL: &str = "kimi-for-coding";
// Kimi's provider default points at the general Moonshot API. Pinning the
// first-party coding endpoint is the boundary that keeps this catalog entry
// scoped to Kimi Code rather than exposing an undeclared custom-provider lane.
const KIMI_CODE_BASE_URL: &str = "https://api.kimi.com/coding/v1";

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

    pub fn supports_fork_session(&self) -> bool {
        self.supports_session_capability("fork")
    }

    pub fn supports_fork_message_id(&self) -> bool {
        let fork = self
            .capabilities
            .get("sessionCapabilities")
            .and_then(Value::as_object)
            .and_then(|caps| caps.get("fork"))
            .and_then(Value::as_object);
        fork.and_then(|fork| fork.get("messageId"))
            .is_some_and(Value::is_object)
            || fork
                .and_then(|fork| fork.get("_meta"))
                .and_then(Value::as_object)
                .and_then(|meta| meta.get("acpStack"))
                .and_then(Value::as_object)
                .and_then(|stack| stack.get("messageId"))
                .is_some_and(Value::is_object)
            || fork
                .and_then(|fork| fork.get("_meta"))
                .and_then(Value::as_object)
                .and_then(|fork| fork.get("messageId"))
                .is_some_and(Value::is_object)
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
        let protocol_version = serde_json::to_value(response.protocol_version)
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

fn build_agent_process_env(
    agent: &AgentConfig,
    mut env: HashMap<String, String>,
) -> Result<HashMap<String, String>> {
    if agent.id != KIMI_CODE_AGENT_ID {
        return Ok(env);
    }

    if let Some(name) = env
        .keys()
        .filter(|name| name.starts_with("KIMI_MODEL_"))
        .min()
    {
        return Err(StackError::AgentInitializeFailed {
            reason: format!(
                "Kimi Code launch env `{name}` is runtime-managed; configure only `{KIMI_API_KEY_ENV}` in [agent].env"
            ),
        });
    }

    let api_key = env
        .remove(KIMI_API_KEY_ENV)
        .ok_or_else(|| StackError::AgentInitializeFailed {
            reason: format!(
                "Kimi Code requires `{KIMI_API_KEY_ENV}` in [agent].env so acp-stack can construct its headless launch environment"
            ),
        })?;
    if api_key.trim().is_empty() {
        return Err(StackError::AgentInitializeFailed {
            reason: format!("Kimi Code secret `{KIMI_API_KEY_ENV}` must not be empty"),
        });
    }
    let model = agent.model.as_deref().unwrap_or(KIMI_CODE_DEFAULT_MODEL);
    if model.trim().is_empty() || model.len() != model.trim().len() {
        return Err(StackError::AgentInitializeFailed {
            reason: "Kimi Code requires a non-empty, trimmed agent.model".to_owned(),
        });
    }

    env.insert(KIMI_MODEL_API_KEY_ENV.to_owned(), api_key);
    env.insert(KIMI_MODEL_NAME_ENV.to_owned(), model.to_owned());
    env.insert(
        KIMI_MODEL_BASE_URL_ENV.to_owned(),
        KIMI_CODE_BASE_URL.to_owned(),
    );
    Ok(env)
}

impl AcpBridge {
    /// Spawn `[agent].command` and complete the ACP `initialize` handshake.
    ///
    /// `env` is the resolved secret-name -> value map for `[agent].env`. We
    /// resolve the command path first, then `env_clear()` so only managed
    /// runtime context and explicitly resolved secrets reach the child.
    ///
    /// `command_log` is the durable command-log target (state store plus live
    /// event hub) for client terminals; `None` (discovery probes, most tests)
    /// keeps terminals functional without `commands` rows or live events.
    pub async fn spawn(
        agent: &AgentConfig,
        env: HashMap<String, String>,
        cwd: PathBuf,
        sink: Arc<dyn SessionEventSink>,
        permissions: Option<PermissionService>,
        sandbox: &crate::config::SandboxConfig,
        command_log: Option<TerminalCommandLog>,
    ) -> Result<Self> {
        let env = build_agent_process_env(agent, env)?;
        let command_path = resolve_command_path(&agent.command, &cwd).ok_or_else(|| {
            StackError::AgentInitializeFailed {
                reason: format!("agent command `{}` not found on PATH", agent.command),
            }
        })?;
        // `off` is a verbatim passthrough so single-process behavior is unchanged
        // and HOME need not be resolvable; other modes wrap the spawn.
        let wrapped = if matches!(sandbox.mode, crate::config::SandboxMode::Off) {
            crate::runtime::sandbox::WrappedCommand {
                program: command_path,
                args: agent.args.clone(),
            }
        } else {
            crate::runtime::sandbox::wrap(
                sandbox,
                &command_path,
                &agent.args,
                &crate::fs_util::home_dir()?,
                &cwd,
                crate::ownership::process_euid(),
                crate::ownership::process_egid(),
            )?
        };
        let mut command = Command::new(&wrapped.program);
        command
            .args(&wrapped.args)
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
        // Network-isolated spawns get the daemon's stderr at the supervisor's
        // diagnostic fd (a no-op wrapper-detection pass for every other mode).
        #[cfg(unix)]
        let diag_handle =
            crate::runtime::sandbox::wire_supervise_diag_fd(sandbox, &mut command, &wrapped.args)
                .map_err(|source| StackError::AgentSpawnFailed { source })?;

        let spawn_result = command.spawn();
        #[cfg(unix)]
        drop(diag_handle);
        let mut child = spawn_result.map_err(|source| StackError::AgentSpawnFailed { source })?;
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
        let notification_queue = spawn_session_notification_queue(sink.clone());
        let notification_drain_for_task = Arc::clone(&notification_drain);
        let bridge_sink = sink.clone();
        let permission_sink = sink.clone();

        // One shared handler context per bridge; each terminal handler
        // closure gets its own Arc clone below (closure-capture rule).
        let terminals = Arc::new(TerminalRegistry::default());
        let terminal_context = Arc::new(TerminalHandlerContext {
            registry: Arc::clone(&terminals),
            workspace_root: cwd.clone(),
            sandbox: sandbox.clone(),
            command_log,
            sink: sink.clone(),
        });
        let create_context = Arc::clone(&terminal_context);
        let output_context = Arc::clone(&terminal_context);
        let wait_context = Arc::clone(&terminal_context);
        let kill_context = Arc::clone(&terminal_context);
        let release_context = Arc::clone(&terminal_context);
        // The fs handlers reuse the same context: they need the workspace
        // root, state, and sink but not the registry or sandbox.
        let fs_read_context = Arc::clone(&terminal_context);
        let fs_write_context = Arc::clone(&terminal_context);

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
                            Some(service) => {
                                resolve_acp_permission(service, &permission_sink, request).await
                            }
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
                // Terminal handlers offload to spawned tasks: they can park
                // for a long time (wait_for_exit on a slow child, kill grace)
                // and handler callbacks run on the connection's single event
                // loop, which must keep processing concurrent terminal calls
                // and session/update notifications meanwhile.
                .on_receive_request(
                    async move |request: CreateTerminalRequest, responder, cx| {
                        let context = Arc::clone(&create_context);
                        cx.spawn(async move {
                            match handle_create_terminal(&context, request).await {
                                Ok(response) => responder.respond(response),
                                Err(error) => responder.respond_with_error(error),
                            }
                        })
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |request: TerminalOutputRequest, responder, cx| {
                        let context = Arc::clone(&output_context);
                        cx.spawn(async move {
                            match handle_terminal_output(&context.registry, request).await {
                                Ok(response) => responder.respond(response),
                                Err(error) => responder.respond_with_error(error),
                            }
                        })
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |request: WaitForTerminalExitRequest, responder, cx| {
                        let context = Arc::clone(&wait_context);
                        cx.spawn(async move {
                            match handle_wait_for_terminal_exit(&context.registry, request).await {
                                Ok(response) => responder.respond(response),
                                Err(error) => responder.respond_with_error(error),
                            }
                        })
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |request: KillTerminalRequest, responder, cx| {
                        let context = Arc::clone(&kill_context);
                        cx.spawn(async move {
                            match handle_kill_terminal(&context.registry, request).await {
                                Ok(response) => responder.respond(response),
                                Err(error) => responder.respond_with_error(error),
                            }
                        })
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |request: ReleaseTerminalRequest, responder, cx| {
                        let context = Arc::clone(&release_context);
                        cx.spawn(async move {
                            match handle_release_terminal(&context.registry, request).await {
                                Ok(response) => responder.respond(response),
                                Err(error) => responder.respond_with_error(error),
                            }
                        })
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |request: ReadTextFileRequest, responder, cx| {
                        let context = Arc::clone(&fs_read_context);
                        cx.spawn(async move {
                            match handle_read_text_file(
                                &context.workspace_root,
                                &context.sink,
                                request,
                            )
                            .await
                            {
                                Ok(response) => responder.respond(response),
                                Err(error) => responder.respond_with_error(error),
                            }
                        })
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |request: WriteTextFileRequest, responder, cx| {
                        let context = Arc::clone(&fs_write_context);
                        cx.spawn(async move {
                            match handle_write_text_file(
                                &context.workspace_root,
                                context.command_log.as_ref().map(|log| &log.state),
                                &context.sink,
                                request,
                            )
                            .await
                            {
                                Ok(response) => responder.respond(response),
                                Err(error) => responder.respond_with_error(error),
                            }
                        })
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_notification(
                    async move |notification: AgentNotification, _cx| {
                        match notification {
                            AgentNotification::SessionNotification(session_note) => {
                                enqueue_session_notification(
                                    &notification_queue,
                                    Arc::clone(&notification_drain_for_task),
                                    session_note,
                                )
                                .await;
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
                        .send_request(
                            InitializeRequest::new(ProtocolVersion::V1)
                                .client_capabilities(client_capabilities()),
                        )
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
            terminals,
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
        Ok(response)
    }

    pub async fn fork_session(
        &self,
        session_id: SessionId,
        cwd: PathBuf,
        mcp_servers: Vec<McpServer>,
        message_id: Option<String>,
    ) -> Result<ForkSessionResponse> {
        if !self.capabilities.supports_fork_session() {
            return Err(StackError::AgentUnsupportedCapability {
                name: "session/fork",
            });
        }
        if message_id.is_some() && !self.capabilities.supports_fork_message_id() {
            return Err(StackError::AgentUnsupportedCapability {
                name: "session/fork.messageId",
            });
        }
        let connection = self.connection().await?;
        let request = StackForkSessionRequest {
            session_id,
            cwd,
            mcp_servers,
            message_id,
        };
        connection
            .send_request(request)
            .block_task()
            .await
            .map_err(|err| StackError::AgentRequestFailed {
                method: "session/fork",
                message: err.to_string(),
            })
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
        let request = SetSessionConfigOptionRequest::new(
            session_id,
            config_id.to_owned(),
            SessionConfigValueId::new(value.to_owned()),
        );
        connection
            .send_request(request)
            .block_task()
            .await
            .map_err(|err| StackError::AgentRequestFailed {
                method: "session/set_config_option",
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

    /// `session/resume`. Stable in ACP v1; gated only by the agent's
    /// advertised capability. The agent may still reject if it does not
    /// implement resume — that surfaces as `agent.request_failed`.
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

    /// `session/close`. Stable in ACP v1; gated only by the agent's
    /// advertised capability.
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
    pub async fn prompt_session(&self, request: PromptRequest) -> Result<PromptResponse> {
        let connection = self.connection().await?;
        match connection.send_request(request).block_task().await {
            Ok(response) => Ok(response),
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
        self.clear_connection().await;
        // Kill-and-release live client terminals before agent teardown: they
        // run in their own process groups, so the agent-pgroup SIGKILL below
        // would orphan them. The supervisor's crash monitor also routes
        // through shutdown(), so this covers unplanned exits too.
        self.terminals.drain_all().await;
        if let Some(tx) = self.shutdown_tx.lock().await.take() {
            let _ = tx.send(());
        }
        self.wait_connection_task().await;
        self.flush_notifications().await;

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

    /// Tear down a provisional probe by killing the process group before the
    /// client IO loop drops stdout. This keeps one-shot discovery from
    /// surfacing adapter-side broken-pipe stack traces after values were read.
    pub async fn terminate_probe(&self) -> Result<Option<i32>> {
        self.planned_shutdown.store(true, Ordering::SeqCst);
        self.clear_connection().await;
        self.terminals.drain_all().await;

        let status = match self.child.lock().await.take() {
            Some(mut child) => {
                kill_tokio_process_group(&mut child);
                match timeout(SHUTDOWN_GRACE, child.wait()).await {
                    Ok(Ok(status)) => Some(status),
                    Ok(Err(err)) => {
                        tracing::warn!(error = ?err, "acp bridge: wait failed after probe kill");
                        None
                    }
                    Err(_) => None,
                }
            }
            None => None,
        };

        if let Some(tx) = self.shutdown_tx.lock().await.take() {
            let _ = tx.send(());
        }
        self.wait_connection_task().await;
        self.flush_notifications().await;

        Ok(status.and_then(|s| s.code()))
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

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct StackForkSessionRequest {
    session_id: SessionId,
    cwd: PathBuf,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    mcp_servers: Vec<McpServer>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message_id: Option<String>,
}

impl JsonRpcMessage for StackForkSessionRequest {
    fn matches_method(method: &str) -> bool {
        method == "session/fork"
    }

    fn method(&self) -> &str {
        "session/fork"
    }

    fn to_untyped_message(
        &self,
    ) -> std::result::Result<UntypedMessage, agent_client_protocol::Error> {
        UntypedMessage::new("session/fork", self)
    }

    fn parse_message(
        method: &str,
        params: &impl Serialize,
    ) -> std::result::Result<Self, agent_client_protocol::Error> {
        if method != "session/fork" {
            return Err(agent_client_protocol::Error::method_not_found());
        }
        agent_client_protocol::util::json_cast_params(params)
    }
}

impl JsonRpcRequest for StackForkSessionRequest {
    type Response = ForkSessionResponse;
}

/// Capabilities advertised to every agent at initialize. Each flag flips only
/// once its agent->client handlers exist: advertising ahead of the handlers
/// would invite calls we cannot serve. `boolean` config options stay
/// unadvertised until `set_session_config_option` can send boolean values
/// (today it only sends `SessionConfigValueId`).
fn client_capabilities() -> ClientCapabilities {
    ClientCapabilities::new()
        .fs(FileSystemCapabilities::new()
            .read_text_file(true)
            .write_text_file(true))
        .terminal(true)
        .session(
            ClientSessionCapabilities::new()
                .config_options(SessionConfigOptionsCapabilities::new()),
        )
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

pub(super) fn agent_process_path() -> Option<std::ffi::OsString> {
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

#[cfg(test)]
mod tests {
    use super::{
        KIMI_API_KEY_ENV, KIMI_CODE_AGENT_ID, KIMI_CODE_BASE_URL, KIMI_MODEL_API_KEY_ENV,
        KIMI_MODEL_BASE_URL_ENV, KIMI_MODEL_NAME_ENV, NotificationDrain, build_agent_process_env,
    };
    use crate::config::AgentConfig;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;

    fn kimi_agent(model: Option<&str>) -> AgentConfig {
        AgentConfig {
            id: KIMI_CODE_AGENT_ID.to_owned(),
            name: "Kimi Code".to_owned(),
            command: "kimi".to_owned(),
            args: vec!["acp".to_owned()],
            cwd: None,
            env: vec![KIMI_API_KEY_ENV.to_owned()],
            expected_sha256: None,
            restart: "on-crash".to_owned(),
            mode: None,
            model: model.map(str::to_owned),
            harness_version: None,
            adapter: None,
            provider: None,
            subagent: None,
            auto_update: None,
            install: None,
        }
    }

    #[test]
    fn kimi_process_env_uses_default_model_and_hides_canonical_secret_name() {
        let env = HashMap::from([(KIMI_API_KEY_ENV.to_owned(), "secret".to_owned())]);

        let prepared = build_agent_process_env(&kimi_agent(None), env).expect("Kimi env");

        assert_eq!(
            prepared.get(KIMI_MODEL_API_KEY_ENV).map(String::as_str),
            Some("secret")
        );
        assert_eq!(
            prepared.get(KIMI_MODEL_NAME_ENV).map(String::as_str),
            Some("kimi-for-coding")
        );
        assert_eq!(
            prepared.get(KIMI_MODEL_BASE_URL_ENV).map(String::as_str),
            Some(KIMI_CODE_BASE_URL)
        );
        assert!(!prepared.contains_key(KIMI_API_KEY_ENV));
    }

    #[test]
    fn kimi_process_env_uses_explicit_model() {
        let env = HashMap::from([(KIMI_API_KEY_ENV.to_owned(), "secret".to_owned())]);

        let prepared = build_agent_process_env(&kimi_agent(Some("kimi-for-coding-highspeed")), env)
            .expect("Kimi env");

        assert_eq!(
            prepared.get(KIMI_MODEL_NAME_ENV).map(String::as_str),
            Some("kimi-for-coding-highspeed")
        );
    }

    #[test]
    fn kimi_process_env_requires_canonical_api_key() {
        let error = build_agent_process_env(&kimi_agent(None), HashMap::new())
            .expect_err("missing Kimi key must fail");

        assert!(error.to_string().contains(KIMI_API_KEY_ENV), "{error}");
    }

    #[test]
    fn kimi_process_env_rejects_empty_api_key() {
        let env = HashMap::from([(KIMI_API_KEY_ENV.to_owned(), "  ".to_owned())]);

        let error =
            build_agent_process_env(&kimi_agent(None), env).expect_err("empty Kimi key must fail");

        assert!(error.to_string().contains("must not be empty"), "{error}");
    }

    #[test]
    fn kimi_process_env_rejects_runtime_managed_values() {
        for name in [
            KIMI_MODEL_API_KEY_ENV,
            KIMI_MODEL_NAME_ENV,
            KIMI_MODEL_BASE_URL_ENV,
        ] {
            let env = HashMap::from([
                (KIMI_API_KEY_ENV.to_owned(), "secret".to_owned()),
                (name.to_owned(), "override".to_owned()),
            ]);

            let error = build_agent_process_env(&kimi_agent(None), env)
                .expect_err("managed Kimi env must fail");
            assert!(error.to_string().contains(name), "{error}");
        }
    }

    #[test]
    fn other_agent_process_env_is_unchanged() {
        let mut agent = kimi_agent(None);
        agent.id = "opencode".to_owned();
        let env = HashMap::from([("OPENAI_API_KEY".to_owned(), "secret".to_owned())]);

        assert_eq!(
            build_agent_process_env(&agent, env.clone()).expect("OpenCode env"),
            env
        );
    }

    #[tokio::test]
    async fn wait_idle_returns_immediately_when_no_guards_active() {
        let drain = Arc::new(NotificationDrain::default());
        tokio::time::timeout(Duration::from_secs(1), drain.wait_idle())
            .await
            .expect("wait_idle must not block with zero active guards");
    }

    #[tokio::test]
    async fn wait_idle_completes_when_last_guard_drops() {
        let drain = Arc::new(NotificationDrain::default());
        let first = drain.enter();
        let second = drain.enter();

        let waiter = tokio::spawn({
            let drain = Arc::clone(&drain);
            async move { drain.wait_idle().await }
        });

        // An intermediate N->1 drop must not release the waiter.
        drop(first);
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());

        drop(second);
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("wait_idle must observe the final guard drop")
            .expect("waiter task must not panic");
    }

    #[tokio::test]
    async fn wait_idle_observes_notification_fired_after_registration() {
        let drain = Arc::new(NotificationDrain::default());
        let guard = drain.enter();

        let mut waiter = Box::pin(drain.wait_idle());
        // First poll registers the waiter while a guard is still active.
        assert!(
            futures::poll!(waiter.as_mut()).is_pending(),
            "wait_idle must be pending while a guard is active"
        );

        drop(guard);
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("registered waiter must be woken by the final guard drop");
    }
}
