//! Long-running task that owns one submitted command end-to-end: await any required permission,
//! spawn the shell under a fresh process group, multiplex cancel/timeout/wait/output, then reap the
//! group, drain output under a budget, and finalize the row.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::{Mutex as TokioMutex, oneshot, watch};
use tokio::time::{Instant, sleep};

use crate::error::Result;
use crate::events::EventHub;
use crate::runtime::mediation::permissions::{PermissionOutcome, PermissionService};
use crate::state::{CommandStatus, Event, StateHandle, StateStore};

use super::RunningCommand;
use super::exec::{GRACE_ESCALATION_SIGNAL, GraceKillOutcome, kill_with_grace, sandboxed_program};
use super::output::{
    OptionFlatten, Outcome, OutputChunk, OutputCounter, POST_WAIT_DRAIN_BUDGET,
    floor_char_boundary, read_stream,
};
use super::policy::ResolvedCommandCwd;
use super::process::{exit_signal, kill_process_group_pid};

pub(super) struct SupervisorTask {
    pub(super) state: StateHandle,
    pub(super) event_hub: EventHub,
    pub(super) running: Arc<TokioMutex<HashMap<String, RunningCommand>>>,
    pub(super) awaiting_permission: Arc<TokioMutex<HashMap<String, String>>>,
    pub(super) permissions: PermissionService,
    pub(super) command_id: String,
    pub(super) shell: String,
    pub(super) command: String,
    pub(super) sandbox: crate::runtime::sandbox::SandboxProfile,
    /// Set once the child spawns under `off` with a workload identity; dropped with the task.
    pub(super) cgroup: Option<crate::runtime::sandbox::WorkloadCgroup>,
    pub(super) network_provider: Option<crate::extensions::NetworkProviderExtension>,
    pub(super) workspace_root: std::path::PathBuf,
    pub(super) home: std::path::PathBuf,
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
        // The cancel watch is selected alongside the permission receiver so an in-flight cancel
        // settles both rows even when no operator ever decides.
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
                        "command.cancelled",
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
            tracing::warn!(error = %crate::error::report(&error), command_id = %self.command_id, "failed to mark command running before spawn");
            self.deregister(super::PERMISSION_REASON_START_FAILED).await;
            return;
        }
        let spawn_result = self.spawn_child();
        let mut child = match spawn_result {
            Ok((child, cgroup)) => {
                self.cgroup = cgroup;
                child
            }
            Err(error) => {
                self.record_spawn_failure(error).await;
                self.deregister(super::PERMISSION_REASON_SPAWN_FAILED).await;
                return;
            }
        };
        // Capture the pid up front: `child.wait()` reaps the child, after which `child.id()` may
        // return `None` and the post-wait process-group kill would have no target.
        let pid = child.id().map(|id| id as i32);

        if let Err(error) = self
            .publish_status_event("command.started", json!({"command_id": self.command_id}))
            .await
        {
            tracing::warn!(error = %crate::error::report(&error), command_id = %self.command_id, "failed to persist command started event");
            break_for_persistence_error(&mut child, self.cgroup.as_ref()).await;
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
            tracing::warn!(error = %crate::error::report(&error), command_id = %self.command_id, "failed to persist command review event");
            break_for_persistence_error(&mut child, self.cgroup.as_ref()).await;
            self.finish_after_persistence_error(started).await;
            self.deregister(super::PERMISSION_REASON_PERSISTENCE_FAILED)
                .await;
            return;
        }

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let mut byte_counter = OutputCounter::new(self.max_output_bytes);

        // Readers send bounded chunks, never a full unbounded line, so a `yes`-style command cannot
        // grow memory past `BOUNDED_READ_CHUNK_BYTES` per pending chunk.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<OutputChunk>(64);
        let mut reader_handles = Vec::with_capacity(2);
        if let Some(pipe) = stdout {
            reader_handles.push(tokio::spawn(read_stream(pipe, "stdout", tx.clone())));
        }
        if let Some(pipe) = stderr {
            reader_handles.push(tokio::spawn(read_stream(pipe, "stderr", tx.clone())));
        }
        // Drop the supervisor's clone or the drain loop below never sees the channel close.
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
                        tracing::warn!(error = %crate::error::report(&error), command_id = %self.command_id, "failed to persist command progress; terminating command");
                        break self.handle_persistence_error(&mut child).await;
                    }
                    next_progress_deadline = Instant::now() + self.progress_interval;
                }
                wait_result = child.wait() => {
                    break match wait_result {
                        Ok(status) => Outcome::Exited {
                            code: status.code(),
                            signal: exit_signal(&status),
                        },
                        Err(error) => self.wait_failed(&error, "command child wait failed"),
                    };
                }
                Some(chunk) = rx.recv() => {
                    match self.handle_chunk(chunk, &mut byte_counter).await {
                        Ok(true) => {
                            next_progress_deadline = Instant::now() + self.progress_interval;
                        }
                        Ok(false) => {}
                        Err(error) => {
                            tracing::warn!(error = %crate::error::report(&error), command_id = %self.command_id, "failed to persist command output; terminating command");
                            break self.handle_persistence_error(&mut child).await;
                        }
                    }
                }
            }
        };

        // Reap descendants that inherited the child's stdout/stderr: `sleep 999 & echo done` keeps
        // the readers alive through pipe inheritance and would wedge the row in `running` forever.
        if let Some(pid) = pid {
            kill_process_group_pid(pid);
        }
        if let Some(cgroup) = &self.cgroup {
            cgroup.kill();
        }

        // Drain BEFORE awaiting the reader join handles: joining first deadlocks, because a reader
        // blocked in `tx.send()` on a full mpsc cannot exit until the channel is pumped. The hard
        // cap keeps a detached descendant that escaped the kill above from wedging the task.
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
                        tracing::warn!(error = %crate::error::report(&error), command_id = %self.command_id, "failed to persist drained command output");
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
                        error = %crate::error::report(&error),
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
        let mut signal = None;
        let mut reason = None;
        let (status, exit_status, kind) = match outcome {
            Outcome::Exited {
                code,
                signal: exit_signal,
            } => {
                signal = exit_signal;
                if code == Some(0) {
                    (CommandStatus::Exited, code, "command.exited")
                } else {
                    (CommandStatus::Failed, code, "command.failed")
                }
            }
            Outcome::Canceled {
                signal: kill_signal,
            } => {
                signal = kill_signal;
                (CommandStatus::Canceled, None, "command.cancelled")
            }
            Outcome::TimedOut {
                signal: kill_signal,
            } => {
                signal = kill_signal;
                (CommandStatus::Failed, None, "command.timeout")
            }
            Outcome::WaitFailed {
                reason: wait_reason,
            } => {
                reason = Some(wait_reason);
                (CommandStatus::Failed, None, "command.failed")
            }
            Outcome::PersistenceError => {
                (CommandStatus::Failed, None, "command.persistence_failed")
            }
        };

        let command_id = self.command_id.clone();
        let finish_exit_status = exit_status.flatten_to_i32();
        let payload = json!({
            "command_id": self.command_id,
            "status": status.as_str(),
            "exit_status": exit_status,
            "signal": signal,
            "reason": reason,
            "duration_ms": duration_ms,
        });
        let payload_text = payload.to_string();
        let finished = self
            .state
            .write(move |store| {
                store.finish_command(&command_id, status, finish_exit_status, duration_ms)?;
                Ok(append_command_event(
                    store,
                    &command_id,
                    "info",
                    kind,
                    "",
                    &payload_text,
                ))
            })
            .await;
        match finished {
            Ok(event) => self.publish_appended_event(event, payload),
            Err(error) => {
                tracing::warn!(error = %crate::error::report(&error), command_id = %self.command_id, "failed to finalize command row");
                self.deregister(super::PERMISSION_REASON_PERSISTENCE_FAILED)
                    .await;
                return;
            }
        }

        self.deregister(super::PERMISSION_REASON_COMMAND_FINISHED)
            .await;
    }

    fn spawn_child(
        &self,
    ) -> std::result::Result<
        (
            tokio::process::Child,
            Option<crate::runtime::sandbox::WorkloadCgroup>,
        ),
        std::io::Error,
    > {
        let shell_args = vec!["-c".to_owned(), self.command.clone()];
        let (program, args) = sandboxed_program(
            std::path::Path::new(&self.shell),
            &shell_args,
            &self.sandbox,
            self.network_provider.as_ref(),
            &self.workspace_root,
            &self.home,
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
        let command_id = self.command_id.clone();
        self.state
            .write(move |store| store.start_command(&command_id))
            .await
    }

    async fn handle_chunk(&self, chunk: OutputChunk, counter: &mut OutputCounter) -> Result<bool> {
        if counter.exhausted {
            // Past the cap: drop without persisting, but keep draining so the child does not block
            // on a full pipe buffer.
            return Ok(false);
        }
        let remaining = counter.remaining();
        let bytes = chunk.data.as_bytes();
        if bytes.len() > remaining {
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
            let command_id = self.command_id.clone();
            let payload = json!({"command_id": self.command_id});
            let payload_text = payload.to_string();
            let event = self
                .state
                .write(move |store| {
                    store.mark_command_truncated(&command_id)?;
                    Ok(append_command_event(
                        store,
                        &command_id,
                        "info",
                        "command.output_truncated",
                        "",
                        &payload_text,
                    ))
                })
                .await?;
            self.publish_appended_event(event, payload);
            return Ok(persisted_progress);
        }
        self.persist_chunk(&chunk.stream, counter.seq, &chunk.data)
            .await?;
        counter.seq += 1;
        counter.used += bytes.len();
        Ok(true)
    }

    async fn persist_chunk(&self, stream: &str, seq: u64, data: &str) -> Result<()> {
        let command_id = self.command_id.clone();
        let output_stream = stream.to_owned();
        let output_data = data.to_owned();
        let event = self
            .state
            .write(move |store| {
                store.append_command_output(&command_id, &output_stream, seq, &output_data)
            })
            .await?;
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
        let command_id = self.command_id.clone();
        let event = self
            .state
            .write(move |store| store.append_command_progress(&command_id))
            .await?;
        self.event_hub.publish_command_event(
            &self.command_id,
            &event,
            json!({"command_id": self.command_id}),
        );
        Ok(())
    }

    async fn handle_cancel(&self, child: &mut tokio::process::Child) -> Outcome {
        match kill_with_grace(child, self.cancel_grace, self.cgroup.as_ref()).await {
            GraceKillOutcome::ExitedWithinGrace(Ok(status)) => Outcome::Canceled {
                signal: exit_signal(&status),
            },
            GraceKillOutcome::KilledAfterGrace => Outcome::Canceled {
                signal: Some(GRACE_ESCALATION_SIGNAL.to_owned()),
            },
            GraceKillOutcome::ExitedWithinGrace(Err(error)) => {
                self.wait_failed(&error, "command child wait failed after cancel SIGTERM")
            }
        }
    }

    async fn handle_timeout(&self, child: &mut tokio::process::Child) -> Outcome {
        let signal = match kill_with_grace(child, self.cancel_grace, self.cgroup.as_ref()).await {
            GraceKillOutcome::ExitedWithinGrace(Ok(status)) => exit_signal(&status),
            GraceKillOutcome::KilledAfterGrace => Some(GRACE_ESCALATION_SIGNAL.to_owned()),
            GraceKillOutcome::ExitedWithinGrace(Err(error)) => {
                // The timeout verdict stands; the process group was already signalled.
                tracing::warn!(
                    error = %crate::error::report(&error),
                    command_id = %self.command_id,
                    "command child wait failed after timeout SIGTERM",
                );
                None
            }
        };
        Outcome::TimedOut { signal }
    }

    /// Log a failed `wait()` and carry its reason onto the terminal command event.
    fn wait_failed(&self, error: &std::io::Error, context: &'static str) -> Outcome {
        tracing::warn!(
            error = %crate::error::report(error),
            command_id = %self.command_id,
            "{context}; recording the command as failed",
        );
        Outcome::WaitFailed {
            reason: crate::error::persisted_report(error),
        }
    }

    async fn handle_persistence_error(&self, child: &mut tokio::process::Child) -> Outcome {
        break_for_persistence_error(child, self.cgroup.as_ref()).await;
        Outcome::PersistenceError
    }

    async fn finish_after_persistence_error(&self, started: Instant) {
        let duration_ms = i64::try_from(started.elapsed().as_millis()).ok();
        let command_id = self.command_id.clone();
        if let Err(error) = self
            .state
            .write(move |store| {
                store.finish_command(&command_id, CommandStatus::Failed, None, duration_ms)
            })
            .await
        {
            tracing::warn!(error = %crate::error::report(&error), command_id = %self.command_id, "failed to finalize command after persistence error");
        }
    }

    async fn record_spawn_failure(&self, error: std::io::Error) {
        let reason = crate::error::persisted_report(&error);
        // Same shape as the terminal path's `command.spawn_failed`.
        let payload = json!({
            "command_id": self.command_id,
            "status": CommandStatus::Failed.as_str(),
            "exit_status": null,
            "signal": null,
            "reason": reason,
            "duration_ms": null,
        });
        let payload_text = payload.to_string();
        let command_id = self.command_id.clone();
        // The row and its event are written independently, so a failed row still records
        // why the command never ran.
        let event = self
            .state
            .write(move |store| {
                if let Err(error) =
                    store.finish_command(&command_id, CommandStatus::Failed, None, None)
                {
                    tracing::warn!(error = %crate::error::report(&error), command_id = %command_id, "failed to record command spawn failure");
                }
                Ok(append_command_event(
                    store,
                    &command_id,
                    "error",
                    "command.spawn_failed",
                    &reason,
                    &payload_text,
                ))
            })
            .await;
        match event {
            Ok(event) => self.publish_appended_event(event, payload),
            Err(append_error) => {
                tracing::warn!(
                    error = %crate::error::report(&append_error),
                    spawn_error = %crate::error::report(&error),
                    command_id = %self.command_id,
                    "failed to persist command.spawn_failed event",
                );
            }
        }
    }

    async fn publish_status_event(&self, kind: &'static str, data: Value) -> Result<()> {
        let payload_text = data.to_string();
        let event = self
            .state
            .write(move |store| {
                store.append_event_with_source(
                    "info",
                    kind,
                    crate::state::EVENT_SOURCE_COMMAND,
                    "",
                    &payload_text,
                )
            })
            .await?;
        self.event_hub
            .publish_command_event(&self.command_id, &event, data);
        Ok(())
    }

    /// Fan out an event appended inside a write job, once that job committed.
    fn publish_appended_event(&self, event: Option<Event>, payload: Value) {
        if let Some(event) = event {
            self.event_hub
                .publish_command_event(&self.command_id, &event, payload);
        }
    }

    /// Remove this command from the live registries and settle any still-pending permission. Every
    /// exit path of `run()` MUST funnel through here, or a command could reach a terminal status
    /// while its permission row stays approvable.
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
                    "cancelled permission left pending by command teardown",
                );
            }
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(
                    error = %crate::error::report(&error),
                    command_id = %self.command_id,
                    permission_id = %permission_id,
                    reason = permission_reason,
                    "failed to settle pending permission during command teardown",
                );
            }
        }
    }

    /// Settle a command row that never reached the spawn step.
    async fn finalize_without_spawn(
        &self,
        status: CommandStatus,
        kind: &'static str,
        payload: Value,
    ) {
        let command_id = self.command_id.clone();
        let payload_text = payload.to_string();
        let event = self
            .state
            .write(move |store| {
                if let Err(error) = store.finish_command(&command_id, status, None, None) {
                    tracing::warn!(error = %crate::error::report(&error), command_id = %command_id, "failed to finalize command without spawn");
                }
                Ok(append_command_event(
                    store,
                    &command_id,
                    "info",
                    kind,
                    "",
                    &payload_text,
                ))
            })
            .await;
        match event {
            Ok(event) => self.publish_appended_event(event, payload),
            Err(error) => {
                tracing::warn!(
                    error = %crate::error::report(&error),
                    command_id = %self.command_id,
                    event_kind = kind,
                    "failed to persist command event for a command that never spawned",
                );
            }
        }
    }
}

/// Append a command lifecycle event inside the write job that settled its row. A failed
/// append is logged so the row write it accompanies still commits.
fn append_command_event(
    store: &StateStore,
    command_id: &str,
    level: &str,
    kind: &str,
    message: &str,
    payload_text: &str,
) -> Option<Event> {
    match store.append_event_with_source(
        level,
        kind,
        crate::state::EVENT_SOURCE_COMMAND,
        message,
        payload_text,
    ) {
        Ok(event) => Some(event),
        Err(error) => {
            tracing::warn!(
                error = %crate::error::report(&error),
                command_id,
                event_kind = kind,
                "failed to persist command event",
            );
            None
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

async fn break_for_persistence_error(
    child: &mut tokio::process::Child,
    cgroup: Option<&crate::runtime::sandbox::WorkloadCgroup>,
) {
    kill_with_grace(child, Duration::from_millis(250), cgroup).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AcpPromptAction, PermissionTimeoutAction};
    use crate::runtime::mediation::commands::policy::resolve_cwd_under_workspace;
    use crate::runtime::mediation::permissions::{NewPermission, PermissionSource};
    use crate::state::{CommandOrigin, NewCommandRecord, StateStore};

    struct Fixture {
        tempdir: tempfile::TempDir,
        state: StateHandle,
        permissions: PermissionService,
        awaiting_permission: Arc<TokioMutex<HashMap<String, String>>>,
    }

    fn fixture() -> Fixture {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let state_path = tempdir.path().join("state.sqlite");
        let store = StateStore::open(&state_path).expect("state open");
        store.migrate().expect("migrate");
        let state = StateHandle::start(store).expect("start state handle");
        let permissions = PermissionService::new(
            state.clone(),
            EventHub::new(),
            Duration::from_secs(60),
            PermissionTimeoutAction::Deny,
            AcpPromptAction::Ask,
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
            cgroup: None,
            network_provider: None,
            workspace_root: fixture.tempdir.path().to_path_buf(),
            home: fixture.tempdir.path().to_path_buf(),
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
        let command = command.to_owned();
        fixture
            .state
            .write(move |store| {
                store.append_command(NewCommandRecord {
                    command: &command,
                    cwd: None,
                    env_json: None,
                    origin: CommandOrigin::Operator,
                    session_id: None,
                    terminal_id: None,
                })
            })
            .await
            .expect("append command")
            .id
    }

    async fn command_row(fixture: &Fixture, command_id: &str) -> crate::state::CommandRecord {
        let command_id = command_id.to_owned();
        fixture
            .state
            .read(move |store| store.get_command(&command_id))
            .await
            .expect("get command")
            .expect("command row")
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

    /// The payload of the single `kind` event on the command log.
    async fn command_event_payload(fixture: &Fixture, kind: &str) -> Value {
        let event_kind = kind.to_owned();
        let events = fixture
            .state
            .read(move |store| {
                store.query_events(crate::state::LogFilter {
                    limit: 10,
                    kind: Some(&event_kind),
                    ..Default::default()
                })
            })
            .await
            .expect("query events");
        assert_eq!(events.len(), 1, "expected one {kind} event: {events:?}");
        serde_json::from_str(&events[0].payload_json).expect("payload is json")
    }

    #[tokio::test]
    async fn failed_truncation_event_keeps_the_truncation_and_the_command_running() {
        let fixture = fixture();
        rusqlite::Connection::open(fixture.state.path())
            .expect("second connection")
            .execute_batch(
                "CREATE TRIGGER block_truncated BEFORE INSERT ON events \
                 WHEN NEW.kind = 'command.output_truncated' \
                 BEGIN SELECT RAISE(ABORT, 'forced event failure'); END;",
            )
            .expect("trigger installed");
        // Twice the 1024-byte cap, so the output is truncated mid-stream.
        let command = "head -c 2048 /dev/zero | tr '\\0' x";
        let command_id = insert_command(&fixture, command).await;
        let (_cancel_tx, task) = task(&fixture, &command_id, command);

        task.run().await;

        let row = command_row(&fixture, &command_id).await;
        assert_eq!(row.status, "exited");
        assert!(row.truncated);
    }

    #[tokio::test]
    async fn signal_death_records_the_signal_on_the_failed_event() {
        let fixture = fixture();
        let command_id = insert_command(&fixture, "kill -9 $$").await;
        let (_cancel_tx, task) = task(&fixture, &command_id, "kill -9 $$");

        task.run().await;

        let payload = command_event_payload(&fixture, "command.failed").await;
        assert_eq!(payload["command_id"], command_id);
        assert_eq!(payload["status"], "failed");
        assert_eq!(payload["exit_status"], Value::Null);
        assert_eq!(payload["signal"], "SIGKILL");
        assert_eq!(payload["reason"], Value::Null);
    }

    #[tokio::test]
    async fn failed_terminal_event_still_finalizes_and_deregisters_the_command() {
        let fixture = fixture();
        rusqlite::Connection::open(fixture.state.path())
            .expect("second connection")
            .execute_batch(
                "CREATE TRIGGER block_exited BEFORE INSERT ON events \
                 WHEN NEW.kind = 'command.exited' \
                 BEGIN SELECT RAISE(ABORT, 'forced event failure'); END;",
            )
            .expect("trigger installed");
        let command_id = insert_command(&fixture, "true").await;
        let (cancel_tx, task) = task(&fixture, &command_id, "true");
        let running = task.running.clone();
        running
            .lock()
            .await
            .insert(command_id.clone(), RunningCommand { cancel_tx });

        task.run().await;

        assert_eq!(command_row(&fixture, &command_id).await.status, "exited");
        assert!(running.lock().await.is_empty());
        let event_kind = "command.exited".to_owned();
        let events = fixture
            .state
            .read(move |store| {
                store.query_events(crate::state::LogFilter {
                    limit: 10,
                    kind: Some(&event_kind),
                    ..Default::default()
                })
            })
            .await
            .expect("query events");
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn spawn_failure_records_the_reason_on_the_spawn_failed_event() {
        let fixture = fixture();
        let command_id = insert_command(&fixture, "true").await;
        let (_cancel_tx, mut task) = task(&fixture, &command_id, "true");
        task.shell = fixture
            .tempdir
            .path()
            .join("missing-shell")
            .to_string_lossy()
            .into_owned();

        task.run().await;

        let command = command_row(&fixture, &command_id).await;
        assert_eq!(command.status, "failed");
        let payload = command_event_payload(&fixture, "command.spawn_failed").await;
        assert_eq!(payload["command_id"], command_id);
        assert!(
            payload["reason"]
                .as_str()
                .is_some_and(|reason| !reason.is_empty()),
            "payload: {payload}"
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

        // A receiver whose sender is already gone: the in-memory waiter vanished undecided.
        let (tx, rx) = oneshot::channel::<PermissionOutcome>();
        drop(tx);
        let (_cancel_tx, mut task) = task(&fixture, &command_id, "sudo true");
        task.permission_rx = Some(rx);
        task.run().await;

        let command = command_row(&fixture, &command_id).await;
        assert_eq!(command.status, "failed");

        let permission = fixture.permissions.get(&record.id).await.expect("get");
        assert_eq!(permission.status, "cancelled");

        let permission_id = record.id.clone();
        let events = fixture
            .state
            .read(move |store| {
                store.query_permission_events(crate::state::EventFilter {
                    limit: 10,
                    permission_id: Some(&permission_id),
                    ..crate::state::EventFilter::default()
                })
            })
            .await
            .expect("query events");
        let canceled = events
            .iter()
            .find(|event| event.kind == "permission.cancelled")
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

        // Teardown must not clobber the operator's decision: `cancel_if_pending` is a no-op here.
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
