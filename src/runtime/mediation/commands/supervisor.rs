//! Long-running task that owns one submitted command end-to-end.
//!
//! Lifecycle:
//!   1. If `[permissions].mode` required approval, wait on the permission
//!      oneshot (concurrently with the cancel watch). Denial/cancel/expiry
//!      finalize the row without ever spawning.
//!   2. Spawn `workspace.default_shell -c <command>` under a fresh process
//!      group with `kill_on_drop(true)`. Mark the row `running`.
//!   3. Multiplex `cancel_rx`, the timeout deadline, `child.wait()`, and the
//!      output mpsc — sending SIGTERM (then SIGKILL after `cancel_grace`)
//!      on the cancel/timeout branches.
//!   4. After the direct child exits, SIGKILL the process group by captured
//!      pid to reap descendants holding the pipes open, drain the channel
//!      under a hard budget, then finalize the row with the terminal status.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::{Mutex as TokioMutex, oneshot, watch};
use tokio::time::{Instant, sleep};

use crate::error::Result;
use crate::events::EventHub;
use crate::runtime::mediation::permissions::{PermissionOutcome, PermissionService};
use crate::state::{CommandStatus, StateStore};

use super::RunningCommand;
use super::exec::{GraceKillOutcome, kill_with_grace, sandboxed_program};
use super::output::{
    OptionFlatten, Outcome, OutputChunk, OutputCounter, POST_WAIT_DRAIN_BUDGET,
    floor_char_boundary, read_stream,
};
use super::policy::ResolvedCommandCwd;
use super::process::kill_process_group_pid;

pub(super) struct SupervisorTask {
    pub(super) state: Arc<TokioMutex<StateStore>>,
    pub(super) event_hub: EventHub,
    pub(super) running: Arc<TokioMutex<HashMap<String, RunningCommand>>>,
    pub(super) awaiting_permission: Arc<TokioMutex<HashMap<String, String>>>,
    pub(super) permissions: PermissionService,
    pub(super) command_id: String,
    pub(super) shell: String,
    pub(super) command: String,
    pub(super) sandbox: crate::config::SandboxConfig,
    pub(super) network_provider: Option<crate::extensions::NetworkProviderExtension>,
    pub(super) workspace_root: std::path::PathBuf,
    pub(super) cwd: ResolvedCommandCwd,
    pub(super) env: Option<HashMap<String, String>>,
    pub(super) timeout_duration: Duration,
    pub(super) cancel_grace: Duration,
    pub(super) progress_interval: Duration,
    pub(super) cancel_rx: watch::Receiver<bool>,
    pub(super) max_output_bytes: usize,
    pub(super) review_flagged: bool,
    pub(super) permission_rx: Option<oneshot::Receiver<PermissionOutcome>>,
}

impl SupervisorTask {
    pub(super) async fn run(mut self) {
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
                        PermissionOutcome::Canceled {
                            reason: super::PERMISSION_REASON_COMMAND_CANCELED.to_owned(),
                        }
                    } else {
                        PermissionOutcome::Expired
                    }
                }
            };
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
                    self.deregister(super::PERMISSION_REASON_DENIED).await;
                    return;
                }
                PermissionOutcome::Canceled { reason } => {
                    self.finalize_without_spawn(
                        CommandStatus::Canceled,
                        "command.canceled",
                        json!({"command_id": self.command_id, "reason": reason}),
                    )
                    .await;
                    self.deregister(&reason).await;
                    return;
                }
                PermissionOutcome::Expired => {
                    self.finalize_without_spawn(
                        CommandStatus::Failed,
                        "command.permission_expired",
                        json!({"command_id": self.command_id}),
                    )
                    .await;
                    self.deregister(super::PERMISSION_REASON_WAITER_LOST).await;
                    return;
                }
            }
        }
        let started = Instant::now();
        if let Err(error) = self.mark_running().await {
            tracing::warn!(error = %error, command_id = %self.command_id, "failed to mark command running before spawn");
            self.deregister(super::PERMISSION_REASON_START_FAILED).await;
            return;
        }
        let spawn_result = self.spawn_child();
        let mut child = match spawn_result {
            Ok(child) => child,
            Err(error) => {
                self.record_spawn_failure(error).await;
                self.deregister(super::PERMISSION_REASON_SPAWN_FAILED).await;
                return;
            }
        };
        // Capture the pid up front. `child.wait()` reaps the child and
        // `child.id()` may return `None` afterwards — but a backgrounded
        // descendant of the shell can still hold our stdout/stderr pipes
        // open, and we need a pid for the post-wait process-group kill.
        let pid = child.id().map(|id| id as i32);

        if let Err(error) = self
            .publish_status_event("command.started", json!({"command_id": self.command_id}))
            .await
        {
            tracing::warn!(error = %error, command_id = %self.command_id, "failed to persist command started event");
            break_for_persistence_error(&mut child).await;
            self.finish_after_persistence_error(started).await;
            self.deregister(super::PERMISSION_REASON_PERSISTENCE_FAILED)
                .await;
            return;
        }
        if self.review_flagged
            && let Err(error) = self
                .publish_status_event(
                    "command.review_flagged",
                    json!({"command_id": self.command_id}),
                )
                .await
        {
            tracing::warn!(error = %error, command_id = %self.command_id, "failed to persist command review event");
            break_for_persistence_error(&mut child).await;
            self.finish_after_persistence_error(started).await;
            self.deregister(super::PERMISSION_REASON_PERSISTENCE_FAILED)
                .await;
            return;
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
        let mut next_progress_deadline = Instant::now() + self.progress_interval;
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
                _ = sleep_until(next_progress_deadline) => {
                    if let Err(error) = self.publish_progress_event().await {
                        tracing::warn!(error = %error, command_id = %self.command_id, "failed to persist command progress; terminating command");
                        break self.handle_persistence_error(&mut child).await;
                    }
                    next_progress_deadline = Instant::now() + self.progress_interval;
                }
                wait_result = child.wait() => {
                    break match wait_result {
                        Ok(status) => Outcome::Exited(status.code()),
                        Err(_) => Outcome::SpawnError,
                    };
                }
                Some(chunk) = rx.recv() => {
                    match self.handle_chunk(chunk, &mut byte_counter).await {
                        Ok(true) => {
                            next_progress_deadline = Instant::now() + self.progress_interval;
                        }
                        Ok(false) => {}
                        Err(error) => {
                            tracing::warn!(error = %error, command_id = %self.command_id, "failed to persist command output; terminating command");
                            break self.handle_persistence_error(&mut child).await;
                        }
                    }
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
                Ok(Some(chunk)) => {
                    if let Err(error) = self.handle_chunk(chunk, &mut byte_counter).await {
                        tracing::warn!(error = %error, command_id = %self.command_id, "failed to persist drained command output");
                        drained_within_budget = false;
                        break;
                    }
                }
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
            Outcome::PersistenceError => {
                (CommandStatus::Failed, None, "command.persistence_failed")
            }
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
            self.deregister(super::PERMISSION_REASON_PERSISTENCE_FAILED)
                .await;
            return;
        }

        if let Err(error) = self
            .publish_status_event(
                kind,
                json!({
                    "command_id": self.command_id,
                    "status": status.as_str(),
                    "exit_status": exit_status,
                    "duration_ms": duration_ms,
                }),
            )
            .await
        {
            tracing::warn!(error = %error, command_id = %self.command_id, "failed to persist terminal command event");
        }

        self.deregister(super::PERMISSION_REASON_COMMAND_FINISHED)
            .await;
    }

    fn spawn_child(&self) -> std::result::Result<tokio::process::Child, std::io::Error> {
        let shell_args = vec!["-c".to_owned(), self.command.clone()];
        let (program, args) = sandboxed_program(
            std::path::Path::new(&self.shell),
            &shell_args,
            &self.sandbox,
            self.network_provider.as_ref(),
            &self.workspace_root,
        )?;
        super::exec::spawn_child(
            &program,
            &args,
            &self.cwd,
            self.env.as_ref(),
            &self.sandbox,
            self.network_provider.as_ref(),
        )
    }

    async fn mark_running(&self) -> Result<()> {
        let store = self.state.lock().await;
        store.start_command(&self.command_id)
    }

    async fn handle_chunk(&self, chunk: OutputChunk, counter: &mut OutputCounter) -> Result<bool> {
        if counter.exhausted {
            // Already past the cap: drop without persisting; keep draining so
            // the child does not block on a full pipe buffer.
            return Ok(false);
        }
        let remaining = counter.remaining();
        let bytes = chunk.data.as_bytes();
        if bytes.len() > remaining {
            // First overflow boundary: record what fits, then truncate.
            let cutoff = floor_char_boundary(&chunk.data, remaining);
            let head = &chunk.data[..cutoff];
            let mut persisted_progress = false;
            if !head.is_empty() {
                self.persist_chunk(&chunk.stream, counter.seq, head).await?;
                counter.seq += 1;
                counter.used += head.len();
                persisted_progress = true;
            }
            counter.exhausted = true;
            {
                let store = self.state.lock().await;
                store.mark_command_truncated(&self.command_id)
            }?;
            self.publish_status_event(
                "command.output_truncated",
                json!({"command_id": self.command_id}),
            )
            .await?;
            return Ok(persisted_progress);
        }
        self.persist_chunk(&chunk.stream, counter.seq, &chunk.data)
            .await?;
        counter.seq += 1;
        counter.used += bytes.len();
        Ok(true)
    }

    async fn persist_chunk(&self, stream: &str, seq: u64, data: &str) -> Result<()> {
        let event = {
            let store = self.state.lock().await;
            store.append_command_output(&self.command_id, stream, seq, data)
        }?;
        self.event_hub.publish_command_event(
            &self.command_id,
            &event,
            json!({
                "event_id": event.id,
                "created_at": event.created_at,
                "command_id": self.command_id,
                "stream": stream,
                "seq": seq,
                "data": data,
            }),
        );
        Ok(())
    }

    async fn publish_progress_event(&self) -> Result<()> {
        let event = {
            let store = self.state.lock().await;
            store.append_command_progress(&self.command_id)
        }?;
        self.event_hub.publish_command_event(
            &self.command_id,
            &event,
            json!({"command_id": self.command_id}),
        );
        Ok(())
    }

    async fn handle_cancel(&self, child: &mut tokio::process::Child) -> Outcome {
        match kill_with_grace(child, self.cancel_grace).await {
            GraceKillOutcome::ExitedWithinGrace(Ok(_)) | GraceKillOutcome::KilledAfterGrace => {
                Outcome::Canceled
            }
            GraceKillOutcome::ExitedWithinGrace(Err(_)) => Outcome::SpawnError,
        }
    }

    async fn handle_timeout(&self, child: &mut tokio::process::Child) -> Outcome {
        kill_with_grace(child, self.cancel_grace).await;
        Outcome::TimedOut
    }

    async fn handle_persistence_error(&self, child: &mut tokio::process::Child) -> Outcome {
        break_for_persistence_error(child).await;
        Outcome::PersistenceError
    }

    async fn finish_after_persistence_error(&self, started: Instant) {
        let duration_ms = i64::try_from(started.elapsed().as_millis()).ok();
        if let Err(error) = {
            let store = self.state.lock().await;
            store.finish_command(&self.command_id, CommandStatus::Failed, None, duration_ms)
        } {
            tracing::warn!(error = %error, command_id = %self.command_id, "failed to finalize command after persistence error");
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
            store.append_event_with_source(
                "error",
                "command.spawn_failed",
                crate::state::EVENT_SOURCE_COMMAND,
                &message,
                &payload_text,
            )
        } {
            self.event_hub
                .publish_command_event(&self.command_id, &event, payload);
        }
    }

    async fn publish_status_event(&self, kind: &'static str, data: Value) -> Result<()> {
        let payload_text = data.to_string();
        let event = {
            let store = self.state.lock().await;
            store.append_event_with_source(
                "info",
                kind,
                crate::state::EVENT_SOURCE_COMMAND,
                "",
                &payload_text,
            )
        }?;
        self.event_hub
            .publish_command_event(&self.command_id, &event, data);
        Ok(())
    }

    /// Remove this command from the live registries and settle any dependent
    /// permission that is still pending. Every exit path of `run()` funnels
    /// through here, so a command can never reach a terminal status while its
    /// permission row stays approvable. `permission_reason` names the cause
    /// recorded in `permission_decisions.reason`; on the normal paths the row
    /// was already decided and the cancel is a race-safe no-op.
    async fn deregister(&self, permission_reason: &str) {
        {
            let mut running = self.running.lock().await;
            running.remove(&self.command_id);
        }
        let permission_id = self
            .awaiting_permission
            .lock()
            .await
            .remove(&self.command_id);
        let Some(permission_id) = permission_id else {
            return;
        };
        match self
            .permissions
            .cancel_if_pending(&permission_id, permission_reason)
            .await
        {
            Ok(true) => {
                tracing::warn!(
                    command_id = %self.command_id,
                    permission_id = %permission_id,
                    reason = permission_reason,
                    "canceled permission left pending by command teardown",
                );
            }
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    command_id = %self.command_id,
                    permission_id = %permission_id,
                    reason = permission_reason,
                    "failed to settle pending permission during command teardown",
                );
            }
        }
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
            store.append_event_with_source(
                "info",
                kind,
                crate::state::EVENT_SOURCE_COMMAND,
                "",
                &payload_text,
            )
        };
        if let Ok(event) = event {
            self.event_hub
                .publish_command_event(&self.command_id, &event, payload);
        }
    }
}

async fn sleep_until(deadline: Instant) {
    let now = Instant::now();
    if deadline <= now {
        return;
    }
    sleep(deadline - now).await;
}

async fn break_for_persistence_error(child: &mut tokio::process::Child) {
    kill_with_grace(child, Duration::from_millis(250)).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PermissionTimeoutAction;
    use crate::runtime::mediation::commands::policy::resolve_cwd_under_workspace;
    use crate::runtime::mediation::permissions::{NewPermission, PermissionSource};
    use crate::state::{CommandOrigin, NewCommandRecord};

    struct Fixture {
        tempdir: tempfile::TempDir,
        state: Arc<TokioMutex<StateStore>>,
        permissions: PermissionService,
        awaiting_permission: Arc<TokioMutex<HashMap<String, String>>>,
    }

    fn fixture() -> Fixture {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let state_path = tempdir.path().join("state.sqlite");
        let store = StateStore::open(&state_path).expect("state open");
        store.migrate().expect("migrate");
        let state = Arc::new(TokioMutex::new(store));
        let permissions = PermissionService::new(
            state.clone(),
            EventHub::new(),
            Duration::from_secs(60),
            PermissionTimeoutAction::Deny,
        );
        Fixture {
            tempdir,
            state,
            permissions,
            awaiting_permission: Arc::new(TokioMutex::new(HashMap::new())),
        }
    }

    fn task(
        fixture: &Fixture,
        command_id: &str,
        command: &str,
    ) -> (watch::Sender<bool>, SupervisorTask) {
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let task = SupervisorTask {
            state: fixture.state.clone(),
            event_hub: EventHub::new(),
            running: Arc::new(TokioMutex::new(HashMap::new())),
            awaiting_permission: fixture.awaiting_permission.clone(),
            permissions: fixture.permissions.clone(),
            command_id: command_id.to_owned(),
            shell: "/bin/sh".to_owned(),
            command: command.to_owned(),
            sandbox: Default::default(),
            network_provider: None,
            workspace_root: fixture.tempdir.path().to_path_buf(),
            cwd: resolve_cwd_under_workspace(
                fixture.tempdir.path(),
                &fixture.tempdir.path().to_string_lossy(),
            )
            .expect("resolved cwd"),
            env: None,
            timeout_duration: Duration::from_secs(5),
            cancel_grace: Duration::from_millis(50),
            progress_interval: Duration::from_secs(5),
            cancel_rx,
            max_output_bytes: 1024,
            review_flagged: false,
            permission_rx: None,
        };
        (cancel_tx, task)
    }

    async fn insert_command(fixture: &Fixture, command: &str) -> String {
        let store = fixture.state.lock().await;
        store
            .append_command(NewCommandRecord {
                command,
                cwd: None,
                env_json: None,
                origin: CommandOrigin::Operator,
                session_id: None,
            })
            .expect("append command")
            .id
    }

    #[tokio::test]
    async fn failed_running_transition_prevents_spawn() {
        let fixture = fixture();
        let marker = fixture.tempdir.path().join("spawned");
        let command = format!("printf spawned > {}", marker.to_string_lossy());
        let (_cancel_tx, task) = task(&fixture, "cmd_missing", &command);

        task.run().await;

        assert!(
            !marker.exists(),
            "command must not spawn when durable running transition fails"
        );
    }

    #[tokio::test]
    async fn waiter_drop_cancels_dependent_permission() {
        let fixture = fixture();
        let command_id = insert_command(&fixture, "sudo true").await;
        let (record, _service_rx) = fixture
            .permissions
            .request(NewPermission {
                source: PermissionSource::Command,
                requester: Some(format!("command:{command_id}")),
                subject_id: Some(command_id.clone()),
                detail: json!({"command": "sudo true"}),
            })
            .await
            .expect("permission request");
        fixture
            .awaiting_permission
            .lock()
            .await
            .insert(command_id.clone(), record.id.clone());

        // Hand the supervisor a receiver whose sender is already gone — the
        // in-memory waiter vanished without a durable decision.
        let (tx, rx) = oneshot::channel::<PermissionOutcome>();
        drop(tx);
        let (_cancel_tx, mut task) = task(&fixture, &command_id, "sudo true");
        task.permission_rx = Some(rx);
        task.run().await;

        let command = {
            let store = fixture.state.lock().await;
            store
                .get_command(&command_id)
                .expect("get command")
                .expect("command row")
        };
        assert_eq!(command.status, "failed");

        let permission = fixture.permissions.get(&record.id).await.expect("get");
        assert_eq!(permission.status, "canceled");

        let events = {
            let store = fixture.state.lock().await;
            store
                .query_permission_events(crate::state::EventFilter {
                    limit: 10,
                    permission_id: Some(&record.id),
                    ..crate::state::EventFilter::default()
                })
                .expect("query events")
        };
        let canceled = events
            .iter()
            .find(|event| event.kind == "permission.canceled")
            .expect("canceled event");
        let payload: Value = serde_json::from_str(&canceled.payload_json).expect("payload");
        assert_eq!(
            payload["reason"],
            super::super::PERMISSION_REASON_WAITER_LOST
        );
        assert_eq!(payload["command_id"], command_id);
    }

    #[tokio::test]
    async fn approved_command_leaves_permission_approved_on_exit() {
        let fixture = fixture();
        let command_id = insert_command(&fixture, "true").await;
        let (record, rx) = fixture
            .permissions
            .request(NewPermission {
                source: PermissionSource::Command,
                requester: Some(format!("command:{command_id}")),
                subject_id: Some(command_id.clone()),
                detail: json!({"command": "true"}),
            })
            .await
            .expect("permission request");
        fixture
            .awaiting_permission
            .lock()
            .await
            .insert(command_id.clone(), record.id.clone());
        fixture
            .permissions
            .approve(&record.id, None, None, "session-key")
            .await
            .expect("approve");

        let (_cancel_tx, mut task) = task(&fixture, &command_id, "true");
        task.permission_rx = Some(rx);
        task.run().await;

        // Teardown must not clobber the operator's decision: the map entry
        // now lives until deregister, so this exercises the cancel_if_pending
        // no-op on the normal path.
        let permission = fixture.permissions.get(&record.id).await.expect("get");
        assert_eq!(permission.status, "approved");
        assert!(
            !fixture
                .awaiting_permission
                .lock()
                .await
                .contains_key(&command_id),
            "deregister must clear the awaiting-permission entry"
        );
    }
}
