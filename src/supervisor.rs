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
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex as TokioMutex, RwLock};

use crate::acp_bridge::{AcpBridge, AgentCapabilitiesDto, resolve_command_path};
use crate::config::AgentConfig;
use crate::error::{Result, StackError};
use crate::secrets::SecretStore;
use crate::state::StateStore;

pub struct ServerLifecycle {
    started_at: Instant,
}

impl ServerLifecycle {
    /// Record `server.starting` while the store is still a direct handle, then
    /// hand back a lifecycle handle that tracks elapsed wall time for the
    /// `server.stopped` payload.
    pub fn starting(state: &StateStore, bind: &str) -> Result<Self> {
        let payload = json!({ "bind": bind }).to_string();
        state.append_agent_lifecycle("server.starting", "acps serve starting", &payload)?;
        Ok(Self {
            started_at: Instant::now(),
        })
    }

    /// Record `server.started` after the listener is bound. Async-aware so the
    /// caller can hold the same `Arc<Mutex<StateStore>>` it later hands to
    /// axum handlers.
    pub async fn started(&self, state: &Arc<TokioMutex<StateStore>>, bind: &str) -> Result<()> {
        let payload = json!({ "bind": bind }).to_string();
        let guard = state.lock().await;
        guard.append_agent_lifecycle("server.started", "acps serve listening", &payload)?;
        Ok(())
    }

    /// Record `server.stopped` with elapsed wall time. Called from the shutdown
    /// arm after axum's graceful-shutdown future resolves.
    pub async fn stopped(&self, state: &Arc<TokioMutex<StateStore>>, reason: &str) -> Result<()> {
        let elapsed_ms = u64::try_from(self.started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
        let payload = json!({ "reason": reason, "elapsed_ms": elapsed_ms }).to_string();
        let guard = state.lock().await;
        guard.append_agent_lifecycle("server.stopped", "acps serve stopped", &payload)?;
        Ok(())
    }
}

/// In-memory state machine for the active ACP agent. `Running` owns the live
/// `AcpBridge`; `Starting` is a brief window during the initialize handshake;
/// `Stopping` is the brief window during graceful teardown.
///
/// The bridge is boxed so the enum stays small even though `Running` carries
/// it; clippy's large_enum_variant lint flagged the unboxed shape and the
/// move cost on every state transition is real.
enum AgentState {
    Stopped,
    Starting,
    Running(Box<AcpBridge>),
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
}

/// Public snapshot of the supervisor: status handlers read this without
/// touching the bridge mutex.
#[derive(Debug, Clone)]
pub struct AgentSnapshot {
    pub state: AgentStateLabel,
    pub latest_capabilities: Option<AgentCapabilitiesDto>,
    pub pid: Option<u32>,
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

        match self.do_start(agent, workspace_root, env, state).await {
            Ok((capabilities, bridge)) => {
                {
                    let mut guard = self.state.lock().await;
                    *guard = AgentState::Running(Box::new(bridge));
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

        let starting_payload = json!({
            "agent_id": agent.id,
            "command": agent.command,
        })
        .to_string();
        {
            let guard = state.lock().await;
            guard.append_agent_lifecycle(
                "agent.starting",
                "starting acp agent",
                &starting_payload,
            )?;
        }

        let bridge = match AcpBridge::spawn(agent, env, cwd).await {
            Ok(bridge) => bridge,
            Err(err) => {
                let failure_payload = json!({
                    "agent_id": agent.id,
                    "reason": err.to_string(),
                })
                .to_string();
                let guard = state.lock().await;
                let _ = guard.append_agent_lifecycle(
                    "agent.spawn_failed",
                    "agent spawn failed",
                    &failure_payload,
                );
                return Err(err);
            }
        };

        let capabilities = bridge.capabilities().clone();
        let pid = bridge.pid();
        let caps_json = capabilities.to_json()?;

        // Persist capabilities and the started event AFTER the bridge is
        // live. If any write fails, shut the bridge down before returning
        // so a failed start never leaks the child.
        let persist_result: Result<()> = {
            let guard = state.lock().await;
            let inner: Result<()> = (|| {
                guard.upsert_agent_capabilities(&agent.id, &caps_json)?;
                let started_payload = json!({
                    "agent_id": agent.id,
                    "pid": pid,
                })
                .to_string();
                guard.append_agent_lifecycle(
                    "agent.started",
                    "agent initialized",
                    &started_payload,
                )?;
                Ok(())
            })();
            inner
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
    pub async fn stop(&self, state: &Arc<TokioMutex<StateStore>>) -> Result<Option<i32>> {
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

        let started_at = Instant::now();
        let shutdown_result = (*bridge).shutdown().await;
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
        let payload = json!({
            "exit_status": exit,
            "elapsed_ms": elapsed_ms,
        })
        .to_string();
        let guard = state.lock().await;
        if let Err(err) = guard.append_agent_lifecycle("agent.stopped", "agent stopped", &payload) {
            tracing::warn!(error = %err, "failed to record agent.stopped lifecycle row");
        }

        Ok(exit)
    }

    /// Called from `acps serve` between the HTTP server returning and
    /// `ServerLifecycle::stopped`. Best-effort cleanup so we don't leak the
    /// agent process past the daemon. Errors are logged but never returned —
    /// the serve path must continue to record `server.stopped` even if the
    /// agent teardown was messy.
    pub async fn shutdown_on_serve_exit(&self, state: &Arc<TokioMutex<StateStore>>) {
        // Determine whether there's anything to stop without holding the
        // lock across the entire shutdown sequence.
        let needs_stop = matches!(*self.state.lock().await, AgentState::Running(_));
        if !needs_stop {
            return;
        }
        if let Err(err) = self.stop(state).await {
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
