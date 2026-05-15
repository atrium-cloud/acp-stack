//! Command Gateway — daemon-mediated shell execution.
//!
//! Responsibilities:
//!   * Resolve a submitted command against `[permissions]` policy (deny/review
//!     glob lists) before any subprocess is spawned.
//!   * Spawn a child via `workspace.default_shell -c <cmd>` with cwd resolved
//!     under `workspace.root` and env restricted to `commands.env_allowlist`.
//!     Process-group leader so a hung grandchild is reaped on cancel/timeout.
//!   * Stream stdout/stderr as `command.stdout` / `command.stderr` events into
//!     SQLite via `StateStore::append_command_output`. Each chunk is also fed
//!     to the `commands.{id}` WebSocket topic. A per-command byte cap stops
//!     persistence (but not draining) once exceeded.
//!   * Track running commands so `POST /v1/commands/{id}/cancel` can SIGTERM
//!     the process group, wait `commands.cancel_grace`, then SIGKILL.
//!
//! What this is NOT: a permissions-approval queue. Phase 1 only honors static
//! `deny` and `review` glob lists. Full review/approval lands later.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::{Mutex as TokioMutex, oneshot, watch};
use tokio::time::{Instant, sleep, timeout};

use crate::config::{Config, PermissionsConfig, parse_duration_string};
use crate::error::{Result, StackError};
use crate::events::EventHub;
use crate::permissions::{NewPermission, PermissionOutcome, PermissionService, PermissionSource};
use crate::state::{CommandRecord, CommandStatus, NewCommandRecord, StateStore};

/// Inputs for `CommandGateway::submit`. Mirror the HTTP request body shape
/// (`docs/specs/api/api.md#commands`), pre-parsed by the handler.
#[derive(Debug, Clone)]
pub struct SubmitRequest {
    pub command: String,
    pub cwd: Option<String>,
    pub env: Option<HashMap<String, String>>,
    pub timeout_override: Option<String>,
}

/// Outcome of evaluating a submitted command against `[permissions]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PolicyDecision {
    Allow,
    Review,
    Deny,
}

struct RunningCommand {
    cancel_tx: watch::Sender<bool>,
}

#[derive(Clone)]
pub struct CommandGateway {
    state: Arc<TokioMutex<StateStore>>,
    event_hub: EventHub,
    config: Arc<Config>,
    running: Arc<TokioMutex<HashMap<String, RunningCommand>>>,
    permissions: PermissionService,
    /// Map command id → pending permission id, so cancel() can also cancel the
    /// permission row when a caller cancels a command that is still awaiting
    /// approval. Cleared by the supervisor task once the decision lands.
    awaiting_permission: Arc<TokioMutex<HashMap<String, String>>>,
}

impl CommandGateway {
    pub fn new(
        state: Arc<TokioMutex<StateStore>>,
        event_hub: EventHub,
        config: Arc<Config>,
        permissions: PermissionService,
    ) -> Self {
        Self {
            state,
            event_hub,
            config,
            running: Arc::new(TokioMutex::new(HashMap::new())),
            permissions,
            awaiting_permission: Arc::new(TokioMutex::new(HashMap::new())),
        }
    }

    /// Validate, persist a `commands` row, and spawn the supervisor task.
    /// Returns the freshly-inserted record (status = pending → running once
    /// the supervisor confirms the spawn).
    pub async fn submit(&self, request: SubmitRequest) -> Result<CommandRecord> {
        // 1. Policy. `deny` rejects synchronously; `review`/`locked` route
        //    through the permission pipeline so an out-of-band approver can
        //    decide before the subprocess is spawned. The row is still
        //    inserted in `pending` so the caller has an id to poll/cancel.
        let decision = evaluate_policy(&request.command, &self.config.permissions);
        let mode = self.config.permissions.mode.as_str();
        let review_flagged = matches!(decision, PolicyDecision::Review) && mode == "auto";
        let needs_approval = match decision {
            PolicyDecision::Deny => {
                return Err(StackError::CommandDenied {
                    reason: "matched [permissions].deny pattern",
                });
            }
            PolicyDecision::Review => mode == "supervised" || mode == "locked",
            PolicyDecision::Allow => mode == "locked",
        };

        // 2. cwd resolution under workspace.root (must stay inside).
        let resolved_cwd = match &request.cwd {
            Some(cwd) => Some(resolve_cwd_under_workspace(
                Path::new(&self.config.workspace.root),
                cwd,
            )?),
            None => None,
        };

        // 3. env allow-list enforcement. Reject any name that is not on the
        //    configured allow-list, so submitting a request cannot inject an
        //    arbitrary env name into the child.
        if let Some(env) = &request.env {
            for name in env.keys() {
                if !self
                    .config
                    .commands
                    .env_allowlist
                    .iter()
                    .any(|allowed| allowed == name)
                {
                    return Err(StackError::CommandEnvNotAllowed { name: name.clone() });
                }
            }
        }

        // Persist only the env *names* in the durable row. Values commonly
        // carry credentials (API tokens, OAuth secrets); storing them in
        // SQLite would expand the secret-at-rest surface beyond the
        // age-encrypted secret store. Names are still useful for audit —
        // "this command was given $GITHUB_TOKEN" — without leaking values.
        let env_json = match &request.env {
            Some(env) if !env.is_empty() => {
                let mut names: Vec<&String> = env.keys().collect();
                names.sort(); // stable serialization for diff/audit
                Some(
                    serde_json::to_string(&names).map_err(|_| StackError::CommandDenied {
                        reason: "env names could not be serialized",
                    })?,
                )
            }
            _ => None,
        };

        // 4. Resolve per-command timeout.
        let timeout_duration = match &request.timeout_override {
            Some(text) => parse_duration_string(text).ok_or(StackError::InvalidDurationField {
                field: "command.timeout",
            })?,
            None => parse_duration_string(&self.config.commands.default_timeout).ok_or(
                StackError::InvalidDurationField {
                    field: "commands.default_timeout",
                },
            )?,
        };

        let cancel_grace = parse_duration_string(&self.config.commands.cancel_grace).ok_or(
            StackError::InvalidDurationField {
                field: "commands.cancel_grace",
            },
        )?;

        // 5. Insert the pending row.
        let cwd_owned = resolved_cwd
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned());
        let record = {
            let store = self.state.lock().await;
            store.append_command(NewCommandRecord {
                command: &request.command,
                cwd: cwd_owned.as_deref(),
                env_json: env_json.as_deref(),
            })?
        };

        // 6. Register the cancel channel and spawn the supervisor.
        let (cancel_tx, cancel_rx) = watch::channel(false);
        {
            let mut running = self.running.lock().await;
            running.insert(record.id.clone(), RunningCommand { cancel_tx });
        }

        // 7. If policy needs approval, create a pending permission row tied to
        //    this command. The row's `detail_json` lists the command, cwd, and
        //    env *names* — never values — so the durable record cannot leak
        //    secrets even if the events table is replicated downstream.
        let pending_permission = if needs_approval {
            let env_names: Vec<String> = request
                .env
                .as_ref()
                .map(|env| {
                    let mut names: Vec<String> = env.keys().cloned().collect();
                    names.sort();
                    names
                })
                .unwrap_or_default();
            let (perm_record, perm_rx) = self
                .permissions
                .request(NewPermission {
                    source: PermissionSource::Command,
                    requester: Some(format!("command:{}", record.id)),
                    subject_id: Some(record.id.clone()),
                    detail: json!({
                        "command": request.command,
                        "cwd": cwd_owned,
                        "env_names": env_names,
                        "policy_decision": match decision {
                            PolicyDecision::Review => "review",
                            PolicyDecision::Allow => "locked-default",
                            PolicyDecision::Deny => "deny",
                        },
                    }),
                })
                .await?;
            self.awaiting_permission
                .lock()
                .await
                .insert(record.id.clone(), perm_record.id.clone());
            Some(perm_rx)
        } else {
            None
        };

        let task = SupervisorTask {
            state: self.state.clone(),
            event_hub: self.event_hub.clone(),
            running: self.running.clone(),
            awaiting_permission: self.awaiting_permission.clone(),
            command_id: record.id.clone(),
            shell: self.config.workspace.default_shell.clone(),
            command: request.command.clone(),
            cwd: cwd_owned,
            env: request.env.clone(),
            workspace_root: self.config.workspace.root.clone(),
            timeout_duration,
            cancel_grace,
            cancel_rx,
            max_output_bytes: self.config.commands.max_output_bytes as usize,
            review_flagged,
            permission_rx: pending_permission,
        };
        tokio::spawn(task.run());

        Ok(record)
    }

    pub async fn get(&self, id: &str) -> Result<CommandRecord> {
        let store = self.state.lock().await;
        store
            .get_command(id)?
            .ok_or_else(|| StackError::CommandNotFound { id: id.to_owned() })
    }

    pub async fn list(&self, limit: u32) -> Result<Vec<CommandRecord>> {
        let store = self.state.lock().await;
        store.query_commands(limit)
    }

    /// Signal the running command to cancel. The supervisor task is
    /// responsible for issuing SIGTERM, waiting `cancel_grace`, and SIGKILLing
    /// if the child has not exited. Returns the latest stored row. If the
    /// command is still awaiting a permission decision, also cancels the
    /// permission row so its durable status reflects the operator's intent.
    pub async fn cancel(&self, id: &str) -> Result<CommandRecord> {
        // Cancel the permission row first if any — the supervisor's select!
        // on perm_rx will resolve as Canceled and finalize the command row
        // without ever spawning a child.
        let perm_id = self.awaiting_permission.lock().await.remove(id);
        if let Some(perm_id) = perm_id {
            if let Err(error) = self.permissions.cancel(&perm_id, "command-canceled").await {
                tracing::warn!(
                    error = %error,
                    command_id = %id,
                    permission_id = %perm_id,
                    "failed to cancel pending permission alongside command cancel",
                );
            }
        }
        let sender = {
            let running = self.running.lock().await;
            running.get(id).map(|entry| entry.cancel_tx.clone())
        };
        match sender {
            Some(tx) => {
                if let Err(error) = tx.send(true) {
                    // The supervisor task dropped its receiver while we held
                    // a live entry — a race between supervisor teardown and a
                    // simultaneous cancel. Surface it: the project's
                    // error-handling rule forbids silent discard.
                    tracing::warn!(
                        error = %error,
                        command_id = %id,
                        "command cancel signal could not be delivered",
                    );
                }
            }
            None => {
                // No live supervisor: either the command never ran or it
                // already finished. Surface 404 if there is no row at all;
                // otherwise let the caller see the terminal state.
                let store = self.state.lock().await;
                return store
                    .get_command(id)?
                    .ok_or_else(|| StackError::CommandNotFound { id: id.to_owned() });
            }
        }
        let store = self.state.lock().await;
        store
            .get_command(id)?
            .ok_or_else(|| StackError::CommandNotFound { id: id.to_owned() })
    }
}

struct SupervisorTask {
    state: Arc<TokioMutex<StateStore>>,
    event_hub: EventHub,
    running: Arc<TokioMutex<HashMap<String, RunningCommand>>>,
    awaiting_permission: Arc<TokioMutex<HashMap<String, String>>>,
    command_id: String,
    shell: String,
    command: String,
    cwd: Option<String>,
    env: Option<HashMap<String, String>>,
    workspace_root: String,
    timeout_duration: Duration,
    cancel_grace: Duration,
    cancel_rx: watch::Receiver<bool>,
    max_output_bytes: usize,
    review_flagged: bool,
    permission_rx: Option<oneshot::Receiver<PermissionOutcome>>,
}

impl SupervisorTask {
    async fn run(mut self) {
        // If a permission was required, wait for the decision (or a cancel)
        // before spawning the child. The cancel watch is consulted alongside
        // the permission receiver so an in-flight cancel resolves the
        // permission row + the command row even if no operator decides.
        if let Some(rx) = self.permission_rx.take() {
            let outcome: PermissionOutcome = tokio::select! {
                outcome = rx => match outcome {
                    Ok(value) => value,
                    Err(_) => PermissionOutcome::Expired,
                },
                changed = self.cancel_rx.changed() => {
                    if changed.is_ok() && *self.cancel_rx.borrow() {
                        PermissionOutcome::Canceled { reason: "command-canceled".to_owned() }
                    } else {
                        PermissionOutcome::Expired
                    }
                }
            };
            self.awaiting_permission
                .lock()
                .await
                .remove(&self.command_id);
            match outcome {
                PermissionOutcome::Approved { .. } => {
                    // fallthrough to spawn
                }
                PermissionOutcome::Denied { .. } => {
                    self.finalize_without_spawn(
                        CommandStatus::Failed,
                        "command.permission_denied",
                        json!({"command_id": self.command_id, "reason": "permission denied"}),
                    )
                    .await;
                    self.deregister().await;
                    return;
                }
                PermissionOutcome::Canceled { reason } => {
                    self.finalize_without_spawn(
                        CommandStatus::Canceled,
                        "command.canceled",
                        json!({"command_id": self.command_id, "reason": reason}),
                    )
                    .await;
                    self.deregister().await;
                    return;
                }
                PermissionOutcome::Expired => {
                    self.finalize_without_spawn(
                        CommandStatus::Failed,
                        "command.permission_expired",
                        json!({"command_id": self.command_id}),
                    )
                    .await;
                    self.deregister().await;
                    return;
                }
            }
        }
        let started = Instant::now();
        let spawn_result = self.spawn_child();
        let mut child = match spawn_result {
            Ok(child) => child,
            Err(error) => {
                self.record_spawn_failure(error).await;
                self.deregister().await;
                return;
            }
        };
        // Capture the pid up front. `child.wait()` reaps the child and
        // `child.id()` may return `None` afterwards — but a backgrounded
        // descendant of the shell can still hold our stdout/stderr pipes
        // open, and we need a pid for the post-wait process-group kill.
        let pid = child.id().map(|id| id as i32);

        // Transition row to `running`.
        if let Err(error) = self.mark_running().await {
            tracing::warn!(error = %error, command_id = %self.command_id, "failed to mark command running");
        }
        self.publish_status_event("command.started", json!({"command_id": self.command_id}))
            .await;
        if self.review_flagged {
            self.publish_status_event(
                "command.review_flagged",
                json!({"command_id": self.command_id}),
            )
            .await;
        }

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let mut byte_counter = OutputCounter::new(self.max_output_bytes);

        // Spawn one reader task per pipe. Readers send bounded chunks through
        // the mpsc — never a full unbounded line — so a `yes`-style command
        // cannot grow memory past `BOUNDED_READ_CHUNK_BYTES` per pending
        // chunk. Channel capacity of 64 bounds the in-flight queue too.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<OutputChunk>(64);
        let mut reader_handles = Vec::with_capacity(2);
        if let Some(pipe) = stdout {
            reader_handles.push(tokio::spawn(read_stream(pipe, "stdout", tx.clone())));
        }
        if let Some(pipe) = stderr {
            reader_handles.push(tokio::spawn(read_stream(pipe, "stderr", tx.clone())));
        }
        // Drop the supervisor's clone so once the readers exit the channel
        // becomes closed and the drain loop below terminates deterministically.
        drop(tx);

        let deadline = started + self.timeout_duration;
        let outcome = loop {
            tokio::select! {
                biased;

                changed = self.cancel_rx.changed() => {
                    if changed.is_err() {
                        continue;
                    }
                    if !*self.cancel_rx.borrow() {
                        continue;
                    }
                    break self.handle_cancel(&mut child).await;
                }
                _ = sleep_until(deadline) => {
                    break self.handle_timeout(&mut child).await;
                }
                wait_result = child.wait() => {
                    break match wait_result {
                        Ok(status) => Outcome::Exited(status.code()),
                        Err(_) => Outcome::SpawnError,
                    };
                }
                Some(chunk) = rx.recv() => {
                    self.handle_chunk(chunk, &mut byte_counter).await;
                }
            }
        };

        // The direct child has exited (or been killed). Reap any descendants
        // that inherited its stdout/stderr — e.g. `sleep 999 & echo done`
        // backgrounds `sleep`, whose pipe inheritance keeps the readers alive
        // and would otherwise wedge the row in `running` forever. SIGKILL is
        // sent to the whole process group; harmless if no descendant is left.
        if let Some(pid) = pid {
            kill_process_group_pid(pid);
        }

        // Drain the channel BEFORE awaiting reader join handles. The drain
        // pumps until the readers have dropped their `tx` clones (which they
        // do on EOF / pipe error), at which point `rx.recv()` returns `None`.
        // Joining first would deadlock the supervisor: a reader can be
        // blocked in `tx.send()` because the bounded mpsc is full, and the
        // join handle does not resolve until the reader exits, which it
        // cannot do while the channel stays full.
        //
        // Hard cap on the drain so a `setsid`/`nohup` detached descendant
        // that escaped our process group (and therefore survived the kill
        // above) cannot wedge the supervisor task forever. We abort the
        // readers on timeout, which closes their handles to the pipes and
        // lets the runtime move on.
        let drain_deadline = Instant::now() + POST_WAIT_DRAIN_BUDGET;
        let mut drained_within_budget = true;
        loop {
            let now = Instant::now();
            if now >= drain_deadline {
                drained_within_budget = false;
                break;
            }
            match tokio::time::timeout(drain_deadline - now, rx.recv()).await {
                Ok(Some(chunk)) => self.handle_chunk(chunk, &mut byte_counter).await,
                Ok(None) => break,
                Err(_) => {
                    drained_within_budget = false;
                    break;
                }
            }
        }
        if drained_within_budget {
            for handle in reader_handles {
                if let Err(error) = handle.await {
                    tracing::warn!(
                        error = %error,
                        command_id = %self.command_id,
                        "command output reader task did not exit cleanly",
                    );
                }
            }
        } else {
            tracing::warn!(
                command_id = %self.command_id,
                "command output drain exceeded budget; aborting reader tasks (detached descendant likely)",
            );
            for handle in reader_handles {
                handle.abort();
            }
        }

        let duration_ms = i64::try_from(started.elapsed().as_millis()).ok();
        let (status, exit_status, kind) = match outcome {
            Outcome::Exited(code) => {
                if code == Some(0) {
                    (CommandStatus::Exited, code, "command.exited")
                } else {
                    (CommandStatus::Failed, code, "command.failed")
                }
            }
            Outcome::Canceled => (CommandStatus::Canceled, None, "command.canceled"),
            Outcome::TimedOut => (CommandStatus::Failed, None, "command.timeout"),
            Outcome::SpawnError => (CommandStatus::Failed, None, "command.failed"),
        };

        if let Err(error) = {
            let store = self.state.lock().await;
            store.finish_command(
                &self.command_id,
                status,
                exit_status.flatten_to_i32(),
                duration_ms,
            )
        } {
            tracing::warn!(error = %error, command_id = %self.command_id, "failed to finalize command row");
        }

        self.publish_status_event(
            kind,
            json!({
                "command_id": self.command_id,
                "status": status.as_str(),
                "exit_status": exit_status,
                "duration_ms": duration_ms,
            }),
        )
        .await;

        self.deregister().await;
    }

    fn spawn_child(&self) -> std::result::Result<tokio::process::Child, std::io::Error> {
        let mut cmd = Command::new(&self.shell);
        cmd.arg("-c").arg(&self.command);
        let cwd = match &self.cwd {
            Some(cwd) => cwd.clone(),
            None => self.workspace_root.clone(),
        };
        cmd.current_dir(&cwd);
        cmd.env_clear();
        if let Some(env) = &self.env {
            for (key, value) in env {
                cmd.env(key, value);
            }
        }
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        // SIGKILL the child if the supervisor task is ever dropped — daemon
        // shutdown, tokio runtime exit, or a panic in the supervisor itself.
        // Without this a running command can outlive `acps serve`, and the
        // SQLite row would stay `running` forever.
        cmd.kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);
        cmd.spawn()
    }

    async fn mark_running(&self) -> Result<()> {
        let store = self.state.lock().await;
        store.start_command(&self.command_id)
    }

    async fn handle_chunk(&self, chunk: OutputChunk, counter: &mut OutputCounter) {
        if counter.exhausted {
            // Already past the cap: drop without persisting; keep draining so
            // the child does not block on a full pipe buffer.
            return;
        }
        let remaining = counter.remaining();
        let bytes = chunk.data.as_bytes();
        if bytes.len() > remaining {
            // First overflow boundary: record what fits, then truncate.
            let cutoff = floor_char_boundary(&chunk.data, remaining);
            let head = &chunk.data[..cutoff];
            if !head.is_empty() {
                self.persist_chunk(&chunk.stream, counter.seq, head).await;
                counter.seq += 1;
                counter.used += head.len();
            }
            counter.exhausted = true;
            if let Err(error) = {
                let store = self.state.lock().await;
                store.mark_command_truncated(&self.command_id)
            } {
                tracing::warn!(error = %error, command_id = %self.command_id, "failed to mark command truncated");
            }
            self.publish_status_event(
                "command.output_truncated",
                json!({"command_id": self.command_id}),
            )
            .await;
            return;
        }
        self.persist_chunk(&chunk.stream, counter.seq, &chunk.data)
            .await;
        counter.seq += 1;
        counter.used += bytes.len();
    }

    async fn persist_chunk(&self, stream: &str, seq: u64, data: &str) {
        let event = {
            let store = self.state.lock().await;
            store.append_command_output(&self.command_id, stream, seq, data)
        };
        match event {
            Ok(event) => {
                self.event_hub.publish_command_event(
                    &self.command_id,
                    &event,
                    json!({
                        "command_id": self.command_id,
                        "stream": stream,
                        "seq": seq,
                        "data": data,
                    }),
                );
            }
            Err(error) => {
                tracing::warn!(error = %error, command_id = %self.command_id, "failed to persist command output");
            }
        }
    }

    async fn handle_cancel(&self, child: &mut tokio::process::Child) -> Outcome {
        send_terminate(child);
        match timeout(self.cancel_grace, child.wait()).await {
            Ok(Ok(_)) => Outcome::Canceled,
            Ok(Err(_)) => Outcome::SpawnError,
            Err(_) => {
                kill_process_group(child);
                let _ = child.wait().await;
                Outcome::Canceled
            }
        }
    }

    async fn handle_timeout(&self, child: &mut tokio::process::Child) -> Outcome {
        send_terminate(child);
        match timeout(self.cancel_grace, child.wait()).await {
            Ok(Ok(_)) | Ok(Err(_)) => Outcome::TimedOut,
            Err(_) => {
                kill_process_group(child);
                let _ = child.wait().await;
                Outcome::TimedOut
            }
        }
    }

    async fn record_spawn_failure(&self, error: std::io::Error) {
        let message = error.to_string();
        let payload = json!({
            "command_id": self.command_id,
            "message": message,
        });
        let payload_text = payload.to_string();
        if let Err(error) = {
            let store = self.state.lock().await;
            store.finish_command(&self.command_id, CommandStatus::Failed, None, None)
        } {
            tracing::warn!(error = %error, command_id = %self.command_id, "failed to record command spawn failure");
        }
        if let Ok(event) = {
            let store = self.state.lock().await;
            store.append_event("error", "command.spawn_failed", &message, &payload_text)
        } {
            self.event_hub
                .publish_command_event(&self.command_id, &event, payload);
        }
    }

    async fn publish_status_event(&self, kind: &'static str, data: Value) {
        let payload_text = data.to_string();
        let event = {
            let store = self.state.lock().await;
            store.append_event("info", kind, "", &payload_text)
        };
        match event {
            Ok(event) => {
                self.event_hub
                    .publish_command_event(&self.command_id, &event, data);
            }
            Err(error) => {
                tracing::warn!(error = %error, command_id = %self.command_id, "failed to publish command status event");
            }
        }
    }

    async fn deregister(&self) {
        let mut running = self.running.lock().await;
        running.remove(&self.command_id);
    }

    /// Settle a command row that never reached the spawn step. Sets the
    /// terminal status (`failed` for denied/expired, `canceled` for
    /// caller-initiated cancel) and emits the corresponding event.
    async fn finalize_without_spawn(
        &self,
        status: CommandStatus,
        kind: &'static str,
        payload: Value,
    ) {
        if let Err(error) = {
            let store = self.state.lock().await;
            store.finish_command(&self.command_id, status, None, None)
        } {
            tracing::warn!(error = %error, command_id = %self.command_id, "failed to finalize command without spawn");
        }
        let payload_text = payload.to_string();
        let event = {
            let store = self.state.lock().await;
            store.append_event("info", kind, "", &payload_text)
        };
        if let Ok(event) = event {
            self.event_hub
                .publish_command_event(&self.command_id, &event, payload);
        }
    }
}

#[derive(Debug)]
struct OutputChunk {
    stream: String,
    data: String,
}

#[derive(Debug)]
struct OutputCounter {
    used: usize,
    max: usize,
    exhausted: bool,
    seq: u64,
}

impl OutputCounter {
    fn new(max: usize) -> Self {
        Self {
            used: 0,
            max,
            exhausted: false,
            seq: 0,
        }
    }

    fn remaining(&self) -> usize {
        self.max.saturating_sub(self.used)
    }
}

#[derive(Debug, Clone, Copy)]
enum Outcome {
    /// `Option<i32>` so we can distinguish a kernel-signal exit (None) from a
    /// normal status code (Some).
    Exited(Option<i32>),
    Canceled,
    TimedOut,
    SpawnError,
}

trait OptionFlatten {
    fn flatten_to_i32(self) -> Option<i32>;
}

impl OptionFlatten for Option<i32> {
    fn flatten_to_i32(self) -> Option<i32> {
        self
    }
}

async fn sleep_until(deadline: Instant) {
    let now = Instant::now();
    if deadline <= now {
        return;
    }
    sleep(deadline - now).await;
}

/// Per-read upper bound on bytes the reader will hold in memory before
/// emitting a chunk. A command that emits a giant line without a newline
/// (`yes`, `dd if=/dev/zero …`) still produces chunks of at most this many
/// bytes — `[commands].max_output_bytes` then caps the cumulative total.
const BOUNDED_READ_CHUNK_BYTES: usize = 4 * 1024;

/// Hard cap on the post-`child.wait()` drain. Without this, a child that
/// detaches a grandchild via `setsid`/`nohup` keeps the stdout/stderr pipes
/// open after our process-group kill (the grandchild lives in a different
/// pgid), and the drain loop would wait for EOF forever, wedging the
/// command row in `running`. 5s is generous for legitimate finalization
/// (descendant ACKs, last buffered output) and short enough that an escaped
/// descendant doesn't keep a supervisor task alive indefinitely.
const POST_WAIT_DRAIN_BUDGET: Duration = Duration::from_secs(5);

async fn read_stream<R>(reader: R, stream: &'static str, tx: tokio::sync::mpsc::Sender<OutputChunk>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    use tokio::io::AsyncReadExt;
    let mut reader = reader;
    let mut buffer = vec![0u8; BOUNDED_READ_CHUNK_BYTES];
    // Carry partial UTF-8 sequences across reads so a 3-byte glyph split
    // across the 4 KiB boundary is decoded once, not twice with replacement
    // chars on either side. Bounded to 3 bytes max — the maximum residue
    // from any valid UTF-8 prefix.
    let mut carryover: Vec<u8> = Vec::with_capacity(4);
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => {
                if !carryover.is_empty() {
                    // Flush any leftover bytes that never completed a UTF-8
                    // sequence (a child that printed garbage and exited).
                    let chunk = String::from_utf8_lossy(&carryover).into_owned();
                    let _ = tx
                        .send(OutputChunk {
                            stream: stream.to_owned(),
                            data: chunk,
                        })
                        .await;
                }
                return;
            }
            Ok(read) => {
                let mut combined = std::mem::take(&mut carryover);
                combined.extend_from_slice(&buffer[..read]);
                let (decoded_end, residue_start) = utf8_split_boundary(&combined);
                // Bound the carryover at 3 bytes. A child emitting an endless
                // stream of stray continuation bytes (e.g. raw binary garbage
                // starting with 0x80…) would otherwise grow this buffer
                // unbounded because the boundary helper keeps deferring.
                // Flush anything longer than that as lossy text and reset.
                let (decoded_end, residue_start) = if combined.len() - residue_start > 3 {
                    (combined.len(), combined.len())
                } else {
                    (decoded_end, residue_start)
                };
                carryover = combined[residue_start..].to_vec();
                let chunk = String::from_utf8_lossy(&combined[..decoded_end]).into_owned();
                if !chunk.is_empty()
                    && tx
                        .send(OutputChunk {
                            stream: stream.to_owned(),
                            data: chunk,
                        })
                        .await
                        .is_err()
                {
                    // Receiver gone: the supervisor moved on. Exit quietly;
                    // this is the normal teardown path on cancel/timeout.
                    return;
                }
            }
            Err(error) => {
                // Surface the read failure so a broken pipe or kernel error
                // is visible in the durable trail, not silently dropped.
                tracing::warn!(error = %error, stream = stream, "command output reader hit IO error");
                return;
            }
        }
    }
}

/// Find the longest prefix of `buf` that ends on a complete UTF-8 codepoint
/// boundary, and return `(decoded_end, residue_start)`. Trailing bytes that
/// could still form a valid codepoint (1-3 leading bytes of a multi-byte
/// sequence) are deferred into `residue_start..` for the next read to
/// complete. Invalid bytes inside an otherwise-complete prefix are kept and
/// decoded lossy by the caller.
fn utf8_split_boundary(buf: &[u8]) -> (usize, usize) {
    // Look back up to 3 bytes for an incomplete UTF-8 leading sequence.
    let len = buf.len();
    for offset in 1..=3 {
        if offset > len {
            break;
        }
        let i = len - offset;
        let byte = buf[i];
        // Continuation byte: keep scanning back.
        if byte & 0b1100_0000 == 0b1000_0000 {
            continue;
        }
        // 4-byte sequence leader
        if byte & 0b1111_1000 == 0b1111_0000 && offset < 4 {
            return (i, i);
        }
        // 3-byte sequence leader
        if byte & 0b1111_0000 == 0b1110_0000 && offset < 3 {
            return (i, i);
        }
        // 2-byte sequence leader
        if byte & 0b1110_0000 == 0b1100_0000 && offset < 2 {
            return (i, i);
        }
        // Single-byte ASCII or fully-complete multi-byte sequence: split
        // right after this byte.
        return (len, len);
    }
    // All bytes were continuations (or buffer < 1) — defer everything.
    (0, 0)
}

fn evaluate_policy(command: &str, permissions: &PermissionsConfig) -> PolicyDecision {
    if permissions
        .deny
        .iter()
        .any(|pattern| glob_match(pattern, command))
    {
        return PolicyDecision::Deny;
    }
    if permissions
        .review
        .iter()
        .any(|pattern| glob_match(pattern, command))
    {
        return PolicyDecision::Review;
    }
    PolicyDecision::Allow
}

/// Minimal shell-style glob matcher. Supports `*` (greedy, any chars including
/// none) and `?` (exactly one char). Everything else matches literally. This
/// is sufficient for the `deny = ["rm *", "shutdown"]`-style patterns the
/// spec calls out; it is NOT a full POSIX-glob implementation.
fn glob_match(pattern: &str, input: &str) -> bool {
    let pattern_bytes = pattern.as_bytes();
    let input_bytes = input.as_bytes();
    glob_match_inner(pattern_bytes, input_bytes)
}

fn glob_match_inner(pattern: &[u8], input: &[u8]) -> bool {
    let mut p = 0;
    let mut i = 0;
    let mut star_p: Option<usize> = None;
    let mut star_i = 0;
    while i < input.len() {
        if p < pattern.len() && (pattern[p] == input[i] || pattern[p] == b'?') {
            p += 1;
            i += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star_p = Some(p);
            star_i = i;
            p += 1;
        } else if let Some(sp) = star_p {
            p = sp + 1;
            star_i += 1;
            i = star_i;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

fn resolve_cwd_under_workspace(root: &Path, requested: &str) -> Result<std::path::PathBuf> {
    if requested.contains('\0') {
        return Err(StackError::CommandCwdOutsideWorkspace {
            requested: requested.to_owned(),
        });
    }
    let candidate = if Path::new(requested).is_absolute() {
        std::path::PathBuf::from(requested)
    } else {
        root.join(requested)
    };
    let canonical_root =
        root.canonicalize()
            .map_err(|_| StackError::CommandCwdOutsideWorkspace {
                requested: requested.to_owned(),
            })?;
    let canonical_candidate =
        candidate
            .canonicalize()
            .map_err(|_| StackError::CommandCwdOutsideWorkspace {
                requested: requested.to_owned(),
            })?;
    if !canonical_candidate.starts_with(&canonical_root) {
        return Err(StackError::CommandCwdOutsideWorkspace {
            requested: requested.to_owned(),
        });
    }
    Ok(canonical_candidate)
}

#[cfg(unix)]
fn send_terminate(child: &tokio::process::Child) {
    if let Some(pid) = child.id() {
        // SAFETY: we own the child pid; negative pid targets the whole process
        // group, which we set with `process_group(0)` at spawn time.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGTERM);
        }
    }
}

#[cfg(not(unix))]
fn send_terminate(child: &tokio::process::Child) {
    let _ = child.start_kill();
}

#[cfg(unix)]
fn kill_process_group(child: &tokio::process::Child) {
    if let Some(pid) = child.id() {
        kill_process_group_pid(pid as i32);
    }
}

#[cfg(not(unix))]
fn kill_process_group(child: &tokio::process::Child) {
    let _ = child.start_kill();
}

/// SIGKILL the process group for a captured pid. Safe to call after the
/// direct child has been reaped — the kernel may have recycled the pid, in
/// which case the kill is a harmless no-op or hits an unrelated foreground
/// group, which on a single-user runtime user owned by us is acceptable.
#[cfg(unix)]
fn kill_process_group_pid(pid: i32) {
    // SAFETY: negative pid targets the process group we created via
    // `process_group(0)` at spawn time. Caller must only pass pids it owns.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_process_group_pid(_pid: i32) {}

fn floor_char_boundary(input: &str, max: usize) -> usize {
    if max >= input.len() {
        return input.len();
    }
    let mut cutoff = max;
    while cutoff > 0 && !input.is_char_boundary(cutoff) {
        cutoff -= 1;
    }
    cutoff
}

// Unused at the moment but reserved for callers that want to bridge into the
// gateway via an oneshot. Keeps the API extensible without changing public
// surface later.
#[allow(dead_code)]
struct PendingHandle {
    tx: oneshot::Sender<Result<CommandRecord>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_match_matches_literal_and_wildcards() {
        assert!(glob_match("rm *", "rm -rf foo"));
        assert!(glob_match("shutdown", "shutdown"));
        assert!(!glob_match("shutdown", "shutdown now"));
        assert!(glob_match("shutdown*", "shutdown now"));
        assert!(glob_match("ls", "ls"));
        assert!(!glob_match("ls", "lsof"));
        assert!(glob_match("git ?ush", "git push"));
        assert!(!glob_match("git ?ush", "git status"));
        assert!(glob_match("*", ""));
        assert!(glob_match("*", "anything goes"));
    }

    #[test]
    fn evaluate_policy_prefers_deny_over_review() {
        let permissions = PermissionsConfig {
            mode: "auto".to_owned(),
            review: vec!["rm *".to_owned()],
            deny: vec!["rm *".to_owned()],
            ..PermissionsConfig::default()
        };
        assert_eq!(
            evaluate_policy("rm -rf /", &permissions),
            PolicyDecision::Deny
        );
    }

    #[test]
    fn evaluate_policy_returns_allow_for_unmatched() {
        let permissions = PermissionsConfig {
            mode: "auto".to_owned(),
            review: vec!["sudo *".to_owned()],
            deny: vec!["shutdown".to_owned()],
            ..PermissionsConfig::default()
        };
        assert_eq!(
            evaluate_policy("ls -la", &permissions),
            PolicyDecision::Allow
        );
    }

    #[test]
    fn floor_char_boundary_respects_utf8() {
        let input = "héllo";
        // 'é' is two bytes (0xC3, 0xA9). Cap at 2 should land at byte 1 (after 'h').
        assert_eq!(floor_char_boundary(input, 2), 1);
        assert_eq!(floor_char_boundary(input, 0), 0);
        assert_eq!(floor_char_boundary(input, 999), input.len());
    }

    #[test]
    fn utf8_split_boundary_defers_partial_codepoints() {
        // 'é' = [0xC3, 0xA9]. First byte alone must be deferred.
        let buf = b"a\xC3";
        assert_eq!(utf8_split_boundary(buf), (1, 1));
        // Complete 'é' should be fully consumed.
        let buf = b"a\xC3\xA9";
        assert_eq!(utf8_split_boundary(buf), (3, 3));
        // Plain ASCII is split right at the end.
        let buf = b"hello";
        assert_eq!(utf8_split_boundary(buf), (5, 5));
        // Two leading bytes of a 4-byte sequence must be deferred.
        let buf = b"\xF0\x9F"; // start of '🚀'
        assert_eq!(utf8_split_boundary(buf), (0, 0));
    }
}
