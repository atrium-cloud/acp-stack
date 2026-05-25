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
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::{Mutex as TokioMutex, oneshot, watch};
use tokio::time::{Instant, sleep, timeout};

use crate::error::Result;
use crate::events::EventHub;
use crate::runtime::mediation::permissions::PermissionOutcome;
use crate::runtime::process_runner::kill_tokio_process_group;
use crate::state::{CommandStatus, StateStore};

use super::RunningCommand;
use super::output::{
    BOUNDED_READ_CHUNK_BYTES, OptionFlatten, Outcome, OutputChunk, OutputCounter,
    POST_WAIT_DRAIN_BUDGET, floor_char_boundary, utf8_split_boundary,
};
use super::process::{kill_process_group_pid, send_terminate};

pub(super) struct SupervisorTask {
    pub(super) state: Arc<TokioMutex<StateStore>>,
    pub(super) event_hub: EventHub,
    pub(super) running: Arc<TokioMutex<HashMap<String, RunningCommand>>>,
    pub(super) awaiting_permission: Arc<TokioMutex<HashMap<String, String>>>,
    pub(super) command_id: String,
    pub(super) shell: String,
    pub(super) command: String,
    pub(super) cwd: Option<String>,
    pub(super) env: Option<HashMap<String, String>>,
    pub(super) workspace_root: String,
    pub(super) timeout_duration: Duration,
    pub(super) cancel_grace: Duration,
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
                kill_tokio_process_group(child);
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
                kill_tokio_process_group(child);
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

    async fn publish_status_event(&self, kind: &'static str, data: Value) {
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
