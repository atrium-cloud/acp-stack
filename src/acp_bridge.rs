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
//! - keeps the connection running in a dedicated task until `shutdown` is
//!   called or the supervisor is dropped.
//!
//! Session methods (`session/*`) and notification forwarding land in the
//! next batch alongside the WebSocket event bus. Right now any incoming
//! request from the agent gets the default SDK "method not found" response.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_client_protocol::schema::{
    AgentNotification, InitializeRequest, InitializeResponse, ProtocolVersion,
};
use agent_client_protocol::{Agent, Client, ConnectionTo};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::process::{Child, Command};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::config::AgentConfig;
use crate::error::{Result, StackError};

/// Maximum time we wait for `initialize` to return before declaring the agent
/// unresponsive. Headless ACP agents handshake in milliseconds; anything more
/// than this is a configuration or compatibility problem.
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(15);

/// Maximum time we wait between sending the shutdown signal and SIGKILLing
/// the agent child. The closure should return immediately once the oneshot
/// fires; if it does not, the child is misbehaving and we cut losses.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

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

/// One spawned agent + its live ACP connection. Single-use: call
/// `AcpBridge::spawn`, hold the bridge for as long as the agent should run,
/// then call `shutdown()` exactly once.
pub struct AcpBridge {
    child: Child,
    capabilities: AgentCapabilitiesDto,
    shutdown_tx: Option<oneshot::Sender<()>>,
    connection_task: Option<JoinHandle<()>>,
}

impl AcpBridge {
    /// Spawn `[agent].command` and complete the ACP `initialize` handshake.
    ///
    /// `env` is the resolved secret-name -> value map for `[agent].env`. We
    /// resolve the command path first, then `env_clear()` so only this map
    /// reaches the child — the security spec requires the runtime to never
    /// inject the full secret store or unrelated host environment.
    pub async fn spawn(
        agent: &AgentConfig,
        env: HashMap<String, String>,
        cwd: PathBuf,
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
        // Inject exactly the resolved names from `[agent].env`. The command
        // path is resolved before `env_clear()`, so the child does not need
        // inherited PATH/HOME/LANG just to start.
        for (name, value) in &env {
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

        let (init_tx, init_rx) =
            oneshot::channel::<std::result::Result<InitializeResponse, String>>();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        // The SDK's Client.builder().connect_with(...) future drives the IO
        // loop until the closure returns. We spawn it as a tokio task so the
        // bridge handle can outlive the call site, and we use a oneshot
        // shutdown signal to ask the closure to wrap up cleanly.
        let connection_task: JoinHandle<()> = tokio::spawn(async move {
            let run = Client
                .builder()
                .on_receive_notification(
                    async move |_notification: AgentNotification, _cx| {
                        // Session updates and other notifications land here.
                        // Until the WebSocket event bus exists, log-and-drop
                        // is the policy. Anything we drop today will be
                        // re-routed to the bus in the next batch.
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
                            let _ = init_tx.send(Ok(response));
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

        let init_response = match timeout(INITIALIZE_TIMEOUT, init_rx).await {
            Ok(Ok(Ok(response))) => response,
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

        Ok(Self {
            child,
            capabilities,
            shutdown_tx: Some(shutdown_tx),
            connection_task: Some(connection_task),
        })
    }

    pub fn capabilities(&self) -> &AgentCapabilitiesDto {
        &self.capabilities
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    /// Gracefully tear down the agent: signal the connection task to return,
    /// then close stdin / wait / SIGKILL the child on a bounded timeline.
    /// Returns the exit status if available.
    pub async fn shutdown(mut self) -> Result<Option<i32>> {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(mut task) = self.connection_task.take() {
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

        // First try to let the child notice stdin closure and exit on its
        // own. If it doesn't, escalate to a process-group SIGKILL so any
        // grandchildren the agent forked (MCP servers, tool subprocesses)
        // also die with the daemon — the bridge spawned with
        // `process_group(0)`, so the child is its own pgid leader.
        let status = match timeout(SHUTDOWN_GRACE, self.child.wait()).await {
            Ok(Ok(status)) => Some(status),
            Ok(Err(err)) => {
                tracing::warn!(error = ?err, "acp bridge: wait failed");
                kill_child_process_group(&mut self.child);
                None
            }
            Err(_) => {
                kill_child_process_group(&mut self.child);
                let _ = self.child.wait().await.ok();
                None
            }
        };

        Ok(status.and_then(|s| s.code()))
    }
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
    let path_env = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path_env) {
        let candidate = dir.join(command);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}
