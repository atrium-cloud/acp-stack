//! Client-terminal substrate for the ACP bridge (`terminal/*` methods).
//!
//! Concurrency model: each terminal is driven by a single owning task that
//! holds the `tokio::process::Child` and `select!`s over natural exit and a
//! kill channel. The registry never holds the `Child` — only a
//! `TerminalHandle` of cheap shared endpoints (output buffer, exit watch,
//! kill sender) — so `terminal/output`, `terminal/wait_for_exit`, and
//! `terminal/kill` never contend for the process under a mutex, and a pending
//! `wait()` is interrupted only by the kill signal the owner itself selects
//! on.
//!
//! Memory: the in-memory replay buffer is trimmed to the byte limit as chunks
//! arrive (keeping the newest bytes, per the ACP spec's truncation direction),
//! so a chatty child the agent never polls cannot grow daemon memory. The
//! untrimmed stream still flows to the durable command log when a command row
//! is attached.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use agent_client_protocol::schema::v1::{
    CreateTerminalRequest, CreateTerminalResponse, EnvVariable, KillTerminalRequest,
    KillTerminalResponse, ReleaseTerminalRequest, ReleaseTerminalResponse, TerminalExitStatus,
    TerminalId, TerminalOutputRequest, TerminalOutputResponse, WaitForTerminalExitRequest,
    WaitForTerminalExitResponse,
};
use tokio::process::Child;
use tokio::sync::{Mutex as TokioMutex, mpsc, watch};
use tokio::time::Instant;

use crate::events::EventHub;
use crate::runtime::mediation::commands::exec::{
    GraceKillOutcome, kill_with_grace, sandboxed_program, spawn_child,
};
use crate::runtime::mediation::commands::output::{
    OutputChunk, POST_WAIT_DRAIN_BUDGET, read_stream,
};
use crate::runtime::mediation::commands::policy::resolve_cwd_under_workspace;
use crate::runtime::mediation::commands::process::kill_process_group_pid;
use crate::state::{
    CommandOrigin, CommandStatus, EVENT_SOURCE_COMMAND, NewCommandRecord, StateStore,
};

use super::acp_bridge::agent_process_path;
use super::session_sink::SessionEventSink;

type AcpError = agent_client_protocol::Error;

// CONSTANTS

/// In-memory replay cap applied when the agent omits `outputByteLimit` on
/// `terminal/create`, so a long-running command can never buffer unbounded
/// output in daemon memory.
pub(crate) const DEFAULT_TERMINAL_OUTPUT_BYTE_LIMIT: u64 = 1024 * 1024;

/// Hard ceiling on the in-memory replay buffer regardless of the
/// `outputByteLimit` the agent requests. Without a clamp an agent asking for
/// a huge limit re-opens the unbounded buffering the default exists to
/// prevent. The durable command log is capped separately by state config.
pub(crate) const MAX_TERMINAL_OUTPUT_BYTE_LIMIT: u64 = 10 * 1024 * 1024;

/// SIGTERM -> SIGKILL escalation window for `terminal/kill`, release-of-a-
/// running-terminal, and bridge shutdown.
pub(crate) const TERMINAL_KILL_GRACE: Duration = Duration::from_secs(2);

/// Capacity of the per-terminal output mpsc between the pipe readers and the
/// buffer pump. Mirrors the command gateway's bound: backpressure, not growth.
const OUTPUT_CHANNEL_CAPACITY: usize = 64;

/// Prefix for terminal ids minted by this client.
const TERMINAL_ID_PREFIX: &str = "term_";

/// Rolling output buffer for one terminal. `truncated` latches once any byte
/// has been dropped to honor the limit.
#[derive(Debug, Default)]
pub(crate) struct TerminalBuffer {
    pub(crate) data: String,
    pub(crate) truncated: bool,
}

impl TerminalBuffer {
    /// Append a chunk, then trim in place to `limit` retaining the NEWEST
    /// bytes at a char boundary. Trimming here (not at read time) is what
    /// bounds memory for output the agent never polls.
    fn append_capped(&mut self, chunk: &str, limit: u64) {
        self.data.push_str(chunk);
        let cutoff = newest_cutoff(&self.data, limit);
        if cutoff > 0 {
            self.data.drain(..cutoff);
            self.truncated = true;
        }
    }
}

/// Shared endpoints for one live (or exited-but-unreleased) terminal. The
/// durable command row id stays with the owning task, which finalizes it.
pub(crate) struct TerminalHandle {
    pub(crate) buffer: Arc<TokioMutex<TerminalBuffer>>,
    pub(crate) exit_rx: watch::Receiver<Option<TerminalExitStatus>>,
    kill_tx: mpsc::Sender<Duration>,
    pub(crate) output_byte_limit: u64,
}

impl TerminalHandle {
    /// Ask the owning task to terminate the child (SIGTERM, `grace`, then
    /// SIGKILL). Idempotent: once the owner has returned the channel is
    /// closed and the send fails harmlessly — the exit watch is already set.
    /// During the owner's post-exit drain the send instead parks in the
    /// buffer unread (bounded by the drain budget); the child is already
    /// dead by then.
    pub(crate) async fn request_kill(&self, grace: Duration) {
        if self.kill_tx.send(grace).await.is_err() {
            tracing::debug!("terminal kill requested after owner exit; already reaped");
        }
    }

    /// Wait until the owning task publishes the exit status.
    pub(crate) async fn wait_for_exit(&self) -> TerminalExitStatus {
        let mut exit_rx = self.exit_rx.clone();
        match exit_rx.wait_for(|status| status.is_some()).await {
            Ok(status) => status.clone().unwrap_or_default(),
            Err(_) => {
                // The owner dropped its sender without publishing — only
                // possible if the owning task panicked. Surface a bare status
                // rather than hanging the agent's RPC forever.
                tracing::warn!("terminal owner task dropped exit channel without publishing");
                TerminalExitStatus::new()
            }
        }
    }

    pub(crate) fn exit_status(&self) -> Option<TerminalExitStatus> {
        self.exit_rx.borrow().clone()
    }
}

/// Live terminals for one bridge, keyed by (agent session id, terminal id).
/// Everything is dropped together on bridge shutdown via `drain_all`, which
/// also latches `closed` so a `terminal/create` racing shutdown cannot
/// register a child the drain snapshot never saw.
#[derive(Default)]
pub(crate) struct TerminalRegistry {
    entries: TokioMutex<RegistryEntries>,
    next_terminal: AtomicU64,
}

#[derive(Default)]
struct RegistryEntries {
    terminals: HashMap<(String, String), Arc<TerminalHandle>>,
    closed: bool,
}

/// Durable command-log target for client terminals: the SQLite store the
/// command rows land in, plus the live-event hub the `commands.{id}` topic
/// fans out through. Public because it is a parameter of `AcpBridge::spawn`;
/// re-exported through `acp_bridge`.
#[derive(Clone)]
pub struct TerminalCommandLog {
    pub state: Arc<TokioMutex<StateStore>>,
    pub event_hub: EventHub,
}

/// Durable command-log attachment for one terminal: the owner mirrors chunks
/// into `append_command_output` and finalizes the row on exit, publishing
/// each step on the `commands.{id}` live topic.
pub(crate) struct TerminalPersistence {
    pub(crate) command_log: TerminalCommandLog,
    pub(crate) command_id: String,
}

impl TerminalRegistry {
    /// Take ownership of a freshly spawned child: wire the pipe readers and
    /// the owning task, insert the handle, and return the minted terminal id.
    /// Returns `None` — after killing the child — when the registry has been
    /// closed by `drain_all`, so a create racing bridge shutdown cannot leave
    /// an orphan process behind. The entries lock is held from the closed
    /// check through the insert (everything between is synchronous), so a
    /// concurrent `drain_all` either sees the new terminal or the register
    /// sees `closed`.
    pub(crate) async fn register(
        self: &Arc<Self>,
        session_id: &str,
        mut child: Child,
        output_byte_limit: u64,
        persistence: Option<TerminalPersistence>,
    ) -> Option<String> {
        let mut entries = self.entries.lock().await;
        if entries.closed {
            drop(entries);
            kill_with_grace(&mut child, Duration::ZERO).await;
            return None;
        }

        let terminal_id = format!(
            "{TERMINAL_ID_PREFIX}{}",
            self.next_terminal.fetch_add(1, Ordering::Relaxed)
        );
        let buffer = Arc::new(TokioMutex::new(TerminalBuffer::default()));
        let (exit_tx, exit_rx) = watch::channel(None);
        let (kill_tx, kill_rx) = mpsc::channel::<Duration>(1);

        let (chunk_tx, chunk_rx) = mpsc::channel::<OutputChunk>(OUTPUT_CHANNEL_CAPACITY);
        let mut reader_handles = Vec::with_capacity(2);
        if let Some(pipe) = child.stdout.take() {
            reader_handles.push(tokio::spawn(read_stream(pipe, "stdout", chunk_tx.clone())));
        }
        if let Some(pipe) = child.stderr.take() {
            reader_handles.push(tokio::spawn(read_stream(pipe, "stderr", chunk_tx.clone())));
        }
        // Drop our clone so the owner's `recv` sees `None` once both readers
        // hit EOF, instead of waiting on a sender nobody will use.
        drop(chunk_tx);

        tokio::spawn(own_terminal(
            child,
            chunk_rx,
            reader_handles,
            Arc::clone(&buffer),
            output_byte_limit,
            kill_rx,
            exit_tx,
            persistence,
        ));

        let handle = Arc::new(TerminalHandle {
            buffer,
            exit_rx,
            kill_tx,
            output_byte_limit,
        });
        entries
            .terminals
            .insert((session_id.to_owned(), terminal_id.clone()), handle);
        Some(terminal_id)
    }

    pub(crate) async fn get(
        &self,
        session_id: &str,
        terminal_id: &str,
    ) -> Option<Arc<TerminalHandle>> {
        self.entries
            .lock()
            .await
            .terminals
            .get(&(session_id.to_owned(), terminal_id.to_owned()))
            .map(Arc::clone)
    }

    /// Remove and return the handle (`terminal/release`). Subsequent lookups
    /// on the id fail, which callers surface as resource-not-found.
    pub(crate) async fn remove(
        &self,
        session_id: &str,
        terminal_id: &str,
    ) -> Option<Arc<TerminalHandle>> {
        self.entries
            .lock()
            .await
            .terminals
            .remove(&(session_id.to_owned(), terminal_id.to_owned()))
    }

    /// Kill-and-release every live terminal and refuse all future
    /// registrations. Called on bridge shutdown and crash teardown: terminal
    /// children live in their own process groups, so the agent-process-group
    /// kill never reaches them.
    pub(crate) async fn drain_all(&self) {
        let handles: Vec<Arc<TerminalHandle>> = {
            let mut entries = self.entries.lock().await;
            entries.closed = true;
            entries.terminals.drain().map(|(_, h)| h).collect()
        };
        for handle in handles {
            if handle.exit_status().is_none() {
                handle.request_kill(TERMINAL_KILL_GRACE).await;
            }
            handle.wait_for_exit().await;
        }
    }
}

/// Single owner of the `Child`: pumps output chunks while waiting for natural
/// exit or a kill request, reaps stray descendants by process group, drains
/// the remaining pipe output (bounded), finalizes the durable command row when
/// one is attached, and only then publishes the exit status — so a
/// `terminal/wait_for_exit` response guarantees the output visible through
/// `terminal/output` and the command log is complete.
#[allow(clippy::too_many_arguments)]
async fn own_terminal(
    mut child: Child,
    mut chunk_rx: mpsc::Receiver<OutputChunk>,
    reader_handles: Vec<tokio::task::JoinHandle<()>>,
    buffer: Arc<TokioMutex<TerminalBuffer>>,
    output_byte_limit: u64,
    mut kill_rx: mpsc::Receiver<Duration>,
    exit_tx: watch::Sender<Option<TerminalExitStatus>>,
    persistence: Option<TerminalPersistence>,
) {
    // Capture the pid before `wait()` reaps the child; needed for the
    // post-exit process-group kill of descendants holding the pipes open.
    let pid = child.id().map(|id| id as i32);
    let started = Instant::now();
    let mut seq: u64 = 0;

    // Kill-intent latch: distinguishes exits the owner caused (terminal/kill,
    // release of a running terminal, shutdown drain) from natural signal
    // deaths (OOM kill, segfault), so the command row can record `canceled`
    // for the former — matching the gateway's operator-cancel mapping —
    // while genuine failures stay `failed`.
    let mut canceled = false;
    let status = loop {
        tokio::select! {
            wait_result = child.wait() => break match wait_result {
                Ok(status) => exit_status_of(status),
                Err(error) => {
                    tracing::warn!(error = %error, "terminal child wait failed");
                    TerminalExitStatus::new()
                }
            },
            Some(grace) = kill_rx.recv() => {
                break match kill_with_grace(&mut child, grace).await {
                    GraceKillOutcome::ExitedWithinGrace(Ok(status)) => {
                        canceled = true;
                        exit_status_of(status)
                    }
                    // A wait error after SIGTERM is an anomaly, not a clean
                    // cancellation — the gateway maps the same outcome to
                    // Failed too.
                    GraceKillOutcome::ExitedWithinGrace(Err(error)) => {
                        tracing::warn!(error = %error, "terminal child wait failed after SIGTERM");
                        TerminalExitStatus::new().signal("SIGTERM".to_owned())
                    }
                    GraceKillOutcome::KilledAfterGrace => {
                        canceled = true;
                        TerminalExitStatus::new().signal("SIGKILL".to_owned())
                    }
                };
            }
            // Disabled (not terminated) once both readers hit EOF: recv()
            // returning `None` fails the `Some` pattern and select keeps
            // waiting on the other branches.
            Some(chunk) = chunk_rx.recv() => {
                append_chunk(&buffer, output_byte_limit, persistence.as_ref(), &mut seq, chunk)
                    .await;
            }
        }
    };

    // Reap descendants that inherited the pipes (same rationale as the
    // command supervisor's post-wait group kill).
    if let Some(pid) = pid {
        kill_process_group_pid(pid);
    }

    // Drain the remaining chunks BEFORE finalizing, so the exit status is
    // never observable while output is still in flight. Bounded like the
    // gateway's post-wait drain: a `setsid`/`nohup` descendant that escaped
    // the group kill holds the pipes open forever, and only aborting the
    // readers lets the owner move on.
    let drain_deadline = Instant::now() + POST_WAIT_DRAIN_BUDGET;
    let mut drained_within_budget = true;
    loop {
        let now = Instant::now();
        if now >= drain_deadline {
            drained_within_budget = false;
            break;
        }
        match tokio::time::timeout(drain_deadline - now, chunk_rx.recv()).await {
            Ok(Some(chunk)) => {
                append_chunk(
                    &buffer,
                    output_byte_limit,
                    persistence.as_ref(),
                    &mut seq,
                    chunk,
                )
                .await;
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
                tracing::warn!(error = %error, "terminal output reader task did not exit cleanly");
            }
        }
    } else {
        tracing::warn!(
            "terminal output drain exceeded budget; aborting reader tasks (detached descendant likely)",
        );
        for handle in reader_handles {
            handle.abort();
        }
    }

    if let Some(persistence) = &persistence {
        let duration_ms = i64::try_from(started.elapsed().as_millis()).ok();
        // Mirrors the gateway's finalize mapping, including canceled rows
        // carrying no exit status.
        let (command_status, event_kind) = if canceled {
            (CommandStatus::Canceled, "command.canceled")
        } else {
            match (&status.exit_code, &status.signal) {
                (Some(0), _) => (CommandStatus::Exited, "command.exited"),
                _ => (CommandStatus::Failed, "command.failed"),
            }
        };
        let exit_code = if canceled {
            None
        } else {
            status.exit_code.and_then(|code| i32::try_from(code).ok())
        };
        let finish_result = {
            let store = persistence.command_log.state.lock().await;
            store.finish_command(
                &persistence.command_id,
                command_status,
                exit_code,
                duration_ms,
            )
        };
        match finish_result {
            Ok(()) => {
                publish_lifecycle_event(
                    persistence,
                    event_kind,
                    serde_json::json!({
                        "command_id": persistence.command_id,
                        "status": command_status.as_str(),
                        "exit_status": exit_code,
                        "duration_ms": duration_ms,
                    }),
                )
                .await;
            }
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    command_id = %persistence.command_id,
                    "failed to finalize terminal command row",
                );
            }
        }
    }

    if exit_tx.send(Some(status)).is_err() {
        // All receivers dropped: the terminal was released before exit. The
        // process is reaped either way; nothing is waiting on the status.
        tracing::debug!("terminal exit published after release; no listeners");
    }
}

/// Append one chunk to the capped in-memory buffer, mirror the untrimmed
/// stream into the durable command log when attached, and fan it out on the
/// `commands.{id}` live topic — the same shape the command gateway publishes.
async fn append_chunk(
    buffer: &Arc<TokioMutex<TerminalBuffer>>,
    output_byte_limit: u64,
    persistence: Option<&TerminalPersistence>,
    seq: &mut u64,
    chunk: OutputChunk,
) {
    buffer
        .lock()
        .await
        .append_capped(&chunk.data, output_byte_limit);
    if let Some(persistence) = persistence {
        let append_result = {
            let store = persistence.command_log.state.lock().await;
            store.append_command_output(&persistence.command_id, &chunk.stream, *seq, &chunk.data)
        };
        match append_result {
            Ok(event) => {
                persistence.command_log.event_hub.publish_command_event(
                    &persistence.command_id,
                    &event,
                    serde_json::json!({
                        "event_id": event.id,
                        "created_at": event.created_at,
                        "command_id": persistence.command_id,
                        "stream": chunk.stream,
                        "seq": *seq,
                        "data": chunk.data,
                    }),
                );
            }
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    command_id = %persistence.command_id,
                    "failed to persist terminal output chunk to command log",
                );
            }
        }
        *seq += 1;
    }
}

/// Persist and publish a terminal lifecycle transition on `commands.{id}`,
/// mirroring the gateway's status events so live subscribers see mediated
/// gateway commands and ACP terminal commands uniformly.
async fn publish_lifecycle_event(
    persistence: &TerminalPersistence,
    kind: &'static str,
    data: serde_json::Value,
) {
    let payload_text = data.to_string();
    let event_result = {
        let store = persistence.command_log.state.lock().await;
        store.append_event_with_source("info", kind, EVENT_SOURCE_COMMAND, "", &payload_text)
    };
    match event_result {
        Ok(event) => {
            persistence.command_log.event_hub.publish_command_event(
                &persistence.command_id,
                &event,
                data,
            );
        }
        Err(error) => {
            tracing::warn!(
                error = %error,
                command_id = %persistence.command_id,
                "failed to persist terminal lifecycle event",
            );
        }
    }
}

/// Everything the `terminal/*` handlers need, cloned into each handler
/// closure before the bridge's connection task is spawned.
pub(crate) struct TerminalHandlerContext {
    pub(crate) registry: Arc<TerminalRegistry>,
    pub(crate) workspace_root: PathBuf,
    pub(crate) sandbox: crate::config::SandboxConfig,
    pub(crate) network_provider: Option<crate::extensions::NetworkProviderExtension>,
    /// Durable command-log target. `None` (e.g. discovery probes) means
    /// terminals still work but leave no `commands` rows behind and publish
    /// no live command events.
    pub(crate) command_log: Option<TerminalCommandLog>,
    pub(crate) sink: Arc<dyn SessionEventSink>,
}

/// `terminal/create`: spawn the requested program in a clean session env
/// under the agent's sandbox profile, record it in the durable command log
/// with an `acp` origin, and hand the child to an owning terminal task.
/// Executes directly by design — the VM is the security boundary; agents
/// send `session/request_permission` separately when their policy requires
/// review.
pub(crate) async fn handle_create_terminal(
    context: &TerminalHandlerContext,
    request: CreateTerminalRequest,
) -> std::result::Result<CreateTerminalResponse, AcpError> {
    let agent_session_id = request.session_id.0.to_string();
    let local_session_id = context
        .sink
        .local_session_id(&agent_session_id)
        .await
        .ok_or_else(|| {
            AcpError::invalid_params().data(serde_json::json!({
                "reason": format!("unknown session `{agent_session_id}`"),
            }))
        })?;

    // Default omitted cwd to the session's own cwd (which may be a
    // subdirectory of the workspace root); sinks without session state fall
    // back to the workspace root.
    let requested_cwd = match &request.cwd {
        Some(path) => path.to_string_lossy().into_owned(),
        None => match context.sink.session_cwd(&agent_session_id).await {
            Some(cwd) => cwd,
            None => context.workspace_root.to_string_lossy().into_owned(),
        },
    };
    let resolved_cwd = resolve_cwd_under_workspace(&context.workspace_root, &requested_cwd)
        .map_err(|error| {
            AcpError::invalid_params().data(serde_json::json!({
                "reason": error.to_string(),
            }))
        })?;

    let env = terminal_environment(&request.env);
    let (program, args) = sandboxed_program(
        Path::new(&request.command),
        &request.args,
        &context.sandbox,
        context.network_provider.as_ref(),
        &context.workspace_root,
    )
    .map_err(AcpError::into_internal_error)?;

    // Insert the durable row before spawning (mirrors the gateway) so even a
    // failed spawn leaves an audit trail.
    let command_id = match &context.command_log {
        Some(command_log) => {
            let rendered = render_command_line(&request.command, &request.args);
            let env_names_json = env_names_json(&request.env);
            let store = command_log.state.lock().await;
            let record = store
                .append_command(NewCommandRecord {
                    command: &rendered,
                    cwd: Some(&resolved_cwd.display_path()),
                    env_json: env_names_json.as_deref(),
                    origin: CommandOrigin::Acp,
                    session_id: Some(&local_session_id),
                })
                .map_err(AcpError::into_internal_error)?;
            Some(record.id)
        }
        None => None,
    };

    let mark_failed = async |reason: &str| {
        if let (Some(command_log), Some(command_id)) = (&context.command_log, &command_id) {
            let store = command_log.state.lock().await;
            if let Err(finish_error) =
                store.finish_command(command_id, CommandStatus::Failed, None, None)
            {
                tracing::warn!(
                    error = %finish_error,
                    command_id = %command_id,
                    "failed to record terminal {reason}",
                );
            }
        }
    };

    let spawn_result = spawn_child(
        &program,
        &args,
        &resolved_cwd,
        Some(&env),
        &context.sandbox,
        context.network_provider.as_ref(),
    );
    let child = match spawn_result {
        Ok(child) => child,
        Err(error) => {
            mark_failed("spawn failure").await;
            return Err(AcpError::into_internal_error(error));
        }
    };

    let persistence = match (&context.command_log, command_id.clone()) {
        (Some(command_log), Some(command_id)) => {
            let start_result = {
                let store = command_log.state.lock().await;
                store.start_command(&command_id)
            };
            if let Err(error) = start_result {
                // Finalize the pending row before surfacing the error, or it
                // stays `pending` forever; the child is reaped by
                // kill_on_drop when this early return drops it.
                mark_failed("start failure").await;
                return Err(AcpError::into_internal_error(error));
            }
            Some(TerminalPersistence {
                command_log: command_log.clone(),
                command_id,
            })
        }
        _ => None,
    };

    let output_byte_limit = effective_output_byte_limit(request.output_byte_limit);
    let terminal_id = match context
        .registry
        .register(&agent_session_id, child, output_byte_limit, persistence)
        .await
    {
        Some(terminal_id) => terminal_id,
        None => {
            // drain_all closed the registry between spawn and register: the
            // child was killed there; surface the shutdown to the agent.
            mark_failed("create during bridge shutdown").await;
            return Err(AcpError::internal_error().data(serde_json::json!({
                "reason": "agent bridge is shutting down; terminal registry closed",
            })));
        }
    };
    Ok(CreateTerminalResponse::new(TerminalId::new(terminal_id)))
}

/// `terminal/output`: current buffered output (already bounded during
/// accumulation), the truncation flag, and the exit status once exited.
pub(crate) async fn handle_terminal_output(
    registry: &TerminalRegistry,
    request: TerminalOutputRequest,
) -> std::result::Result<TerminalOutputResponse, AcpError> {
    let handle = lookup(registry, &request.session_id.0, &request.terminal_id.0).await?;
    let buffer = handle.buffer.lock().await;
    // The pump already trims during accumulation; this re-applies the same
    // invariant at the read boundary so the response can never exceed the
    // limit even mid-append.
    let (output, cut_now) = keep_newest(&buffer.data, handle.output_byte_limit);
    let truncated = buffer.truncated || cut_now;
    Ok(TerminalOutputResponse::new(output.to_owned(), truncated).exit_status(handle.exit_status()))
}

/// `terminal/wait_for_exit`: park until the owning task publishes the exit
/// status. Multiple concurrent waiters all resolve from the same watch.
pub(crate) async fn handle_wait_for_terminal_exit(
    registry: &TerminalRegistry,
    request: WaitForTerminalExitRequest,
) -> std::result::Result<WaitForTerminalExitResponse, AcpError> {
    let handle = lookup(registry, &request.session_id.0, &request.terminal_id.0).await?;
    let status = handle.wait_for_exit().await;
    Ok(WaitForTerminalExitResponse::new(status))
}

/// `terminal/kill`: terminate the child but keep the terminal registered so
/// output stays readable until `terminal/release`.
pub(crate) async fn handle_kill_terminal(
    registry: &TerminalRegistry,
    request: KillTerminalRequest,
) -> std::result::Result<KillTerminalResponse, AcpError> {
    let handle = lookup(registry, &request.session_id.0, &request.terminal_id.0).await?;
    if handle.exit_status().is_none() {
        handle.request_kill(TERMINAL_KILL_GRACE).await;
    }
    // Await the reap so the response guarantees the process is gone and a
    // subsequent terminal/output already carries the exit status.
    handle.wait_for_exit().await;
    Ok(KillTerminalResponse::new())
}

/// `terminal/release`: kill if still running and drop all terminal state.
/// Subsequent calls on the id are resource-not-found errors.
pub(crate) async fn handle_release_terminal(
    registry: &TerminalRegistry,
    request: ReleaseTerminalRequest,
) -> std::result::Result<ReleaseTerminalResponse, AcpError> {
    let handle = registry
        .remove(&request.session_id.0, &request.terminal_id.0)
        .await
        .ok_or_else(|| AcpError::resource_not_found(None))?;
    if handle.exit_status().is_none() {
        handle.request_kill(TERMINAL_KILL_GRACE).await;
        handle.wait_for_exit().await;
    }
    Ok(ReleaseTerminalResponse::new())
}

async fn lookup(
    registry: &TerminalRegistry,
    session_id: &str,
    terminal_id: &str,
) -> std::result::Result<Arc<TerminalHandle>, AcpError> {
    registry
        .get(session_id, terminal_id)
        .await
        .ok_or_else(|| AcpError::resource_not_found(None))
}

/// Human-readable command line for the durable command log.
fn render_command_line(command: &str, args: &[String]) -> String {
    if args.is_empty() {
        return command.to_owned();
    }
    format!("{command} {}", args.join(" "))
}

/// Env *names* only, sorted, mirroring the gateway: values commonly carry
/// credentials and must not expand the secret-at-rest surface.
fn env_names_json(env: &[EnvVariable]) -> Option<String> {
    if env.is_empty() {
        return None;
    }
    let mut names: Vec<&str> = env.iter().map(|variable| variable.name.as_str()).collect();
    names.sort_unstable();
    serde_json::to_string(&names).ok()
}

/// Clean session environment for a terminal child: managed PATH (so
/// registry-installed harness tooling resolves) and HOME, plus the vars the
/// agent supplied on `terminal/create`. Never the `[agent].env` secrets that
/// are injected into the agent process itself — a client terminal must not
/// expose provider API keys to arbitrary shell commands.
pub(crate) fn terminal_environment(agent_env: &[EnvVariable]) -> HashMap<String, String> {
    let mut env = HashMap::new();
    if let Some(path) = agent_process_path() {
        match path.into_string() {
            Ok(path) => {
                env.insert("PATH".to_owned(), path);
            }
            Err(_) => {
                tracing::warn!("managed PATH is not valid UTF-8; omitting from terminal env");
            }
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        env.insert("HOME".to_owned(), home);
    }
    // Agent-provided vars win over the managed defaults: the command belongs
    // to the agent, and the spec gives it control of the child env.
    for variable in agent_env {
        env.insert(variable.name.clone(), variable.value.clone());
    }
    env
}

/// Replay-buffer byte limit for one terminal: the agent's requested
/// `outputByteLimit` (default when omitted), clamped to the hard ceiling.
pub(crate) fn effective_output_byte_limit(requested: Option<u64>) -> u64 {
    requested
        .unwrap_or(DEFAULT_TERMINAL_OUTPUT_BYTE_LIMIT)
        .min(MAX_TERMINAL_OUTPUT_BYTE_LIMIT)
}

/// Return the tail of `buffer` that fits in `limit` bytes, starting on a
/// UTF-8 char boundary, plus whether anything was dropped. The cutoff rounds
/// UP to the next boundary so the result never exceeds `limit` and never
/// splits a codepoint — the mirror of the gateway's `floor_char_boundary`,
/// because ACP truncation keeps the NEWEST bytes (Zed keeps the head; the
/// spec says drop it).
pub(crate) fn keep_newest(buffer: &str, limit: u64) -> (&str, bool) {
    let cutoff = newest_cutoff(buffer, limit);
    (&buffer[cutoff..], cutoff > 0)
}

fn newest_cutoff(buffer: &str, limit: u64) -> usize {
    if buffer.len() as u64 <= limit {
        return 0;
    }
    let mut cutoff = buffer.len() - limit as usize;
    while cutoff < buffer.len() && !buffer.is_char_boundary(cutoff) {
        cutoff += 1;
    }
    cutoff
}

/// Map a reaped process status to the ACP exit shape: `exit_code` for a
/// normal exit, `signal` name when the kernel terminated it.
fn exit_status_of(status: std::process::ExitStatus) -> TerminalExitStatus {
    let mut result = TerminalExitStatus::new();
    if let Some(code) = status.code() {
        result = result.exit_code(u32::try_from(code).ok());
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            result = result.signal(signal_name(signal));
        }
    }
    result
}

#[cfg(unix)]
fn signal_name(signal: i32) -> String {
    match signal {
        libc::SIGHUP => "SIGHUP".to_owned(),
        libc::SIGINT => "SIGINT".to_owned(),
        libc::SIGQUIT => "SIGQUIT".to_owned(),
        libc::SIGABRT => "SIGABRT".to_owned(),
        libc::SIGKILL => "SIGKILL".to_owned(),
        libc::SIGSEGV => "SIGSEGV".to_owned(),
        libc::SIGPIPE => "SIGPIPE".to_owned(),
        libc::SIGALRM => "SIGALRM".to_owned(),
        libc::SIGTERM => "SIGTERM".to_owned(),
        other => format!("SIG{other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keep_newest_retains_tail_at_char_boundary() {
        // Under limit: whole string, untruncated.
        assert_eq!(keep_newest("hello", 10), ("hello", false));
        assert_eq!(keep_newest("hello", 5), ("hello", false));
        // Over limit: newest bytes retained.
        assert_eq!(keep_newest("hello", 3), ("llo", true));
        // Multibyte: cutoff lands mid-'é' (2 bytes) and rounds UP past it,
        // so the result stays within the limit and on a boundary.
        let (kept, truncated) = keep_newest("héllo", 4);
        assert!(truncated);
        assert_eq!(kept, "llo");
        assert!(kept.len() as u64 <= 4);
        // 4-byte emoji: limit 6 can hold one emoji (4 bytes) but not one and
        // a half; cutoff rounds up to the next full glyph.
        let (kept, truncated) = keep_newest("🚀🚀🚀", 6);
        assert!(truncated);
        assert_eq!(kept, "🚀");
        // Limit 0 drops everything.
        let (kept, truncated) = keep_newest("hello", 0);
        assert_eq!(kept, "");
        assert!(truncated);
    }

    #[test]
    fn effective_output_byte_limit_clamps_agent_requests() {
        assert_eq!(
            effective_output_byte_limit(None),
            DEFAULT_TERMINAL_OUTPUT_BYTE_LIMIT
        );
        assert_eq!(effective_output_byte_limit(Some(512)), 512);
        assert_eq!(
            effective_output_byte_limit(Some(u64::MAX)),
            MAX_TERMINAL_OUTPUT_BYTE_LIMIT
        );
    }

    #[test]
    fn buffer_append_trims_to_cap_during_accumulation() {
        let mut buffer = TerminalBuffer::default();
        for index in 0..100 {
            buffer.append_capped(&format!("chunk-{index:03} "), 64);
            assert!(
                buffer.data.len() as u64 <= 64,
                "buffer exceeded cap at chunk {index}: {} bytes",
                buffer.data.len()
            );
        }
        assert!(buffer.truncated);
        // The newest chunk survived; the oldest did not.
        assert!(buffer.data.contains("chunk-099"));
        assert!(!buffer.data.contains("chunk-000"));
    }

    #[test]
    fn terminal_environment_excludes_provider_keys() {
        let agent_env = vec![EnvVariable::new("MY_FLAG", "1")];
        let env = terminal_environment(&agent_env);
        assert_eq!(env.get("MY_FLAG").map(String::as_str), Some("1"));
        // Only the managed baseline plus agent vars — nothing else can be
        // present because composition starts from an empty map, never from
        // the agent process env (which carries provider API keys).
        let allowed = ["PATH", "HOME", "MY_FLAG"];
        for key in env.keys() {
            assert!(allowed.contains(&key.as_str()), "unexpected env var {key}");
        }
    }

    #[tokio::test]
    async fn registered_terminal_captures_output_and_exit_code() {
        let cwd = std::env::temp_dir();
        let resolved = crate::runtime::mediation::commands::policy::resolve_cwd_under_workspace(
            &cwd,
            &cwd.to_string_lossy(),
        )
        .expect("resolve cwd");
        let child = crate::runtime::mediation::commands::exec::spawn_child(
            std::path::Path::new("/bin/sh"),
            &[
                "-c".to_owned(),
                "printf hi-from-terminal; exit 7".to_owned(),
            ],
            &resolved,
            None,
            &crate::config::SandboxConfig::default(),
            None,
        )
        .expect("spawn");

        let registry = Arc::new(TerminalRegistry::default());
        let terminal_id = registry
            .register("sess_test", child, DEFAULT_TERMINAL_OUTPUT_BYTE_LIMIT, None)
            .await
            .expect("register on open registry");
        let handle = registry
            .get("sess_test", &terminal_id)
            .await
            .expect("handle");

        let status = handle.wait_for_exit().await;
        assert_eq!(status.exit_code, Some(7));
        assert_eq!(status.signal, None);

        // The owner drains the pipes before publishing exit, so the full
        // output must be visible the moment wait_for_exit resolves — no
        // polling allowed here; that would mask a drain regression.
        let output = handle.buffer.lock().await.data.clone();
        assert_eq!(output, "hi-from-terminal");

        assert!(registry.remove("sess_test", &terminal_id).await.is_some());
        assert!(registry.get("sess_test", &terminal_id).await.is_none());
    }

    #[tokio::test]
    async fn kill_terminates_long_running_child_and_publishes_signal() {
        let cwd = std::env::temp_dir();
        let resolved = crate::runtime::mediation::commands::policy::resolve_cwd_under_workspace(
            &cwd,
            &cwd.to_string_lossy(),
        )
        .expect("resolve cwd");
        let child = crate::runtime::mediation::commands::exec::spawn_child(
            std::path::Path::new("/bin/sh"),
            &["-c".to_owned(), "sleep 30".to_owned()],
            &resolved,
            None,
            &crate::config::SandboxConfig::default(),
            None,
        )
        .expect("spawn");

        let registry = Arc::new(TerminalRegistry::default());
        let terminal_id = registry
            .register("sess_test", child, DEFAULT_TERMINAL_OUTPUT_BYTE_LIMIT, None)
            .await
            .expect("register on open registry");
        let handle = registry
            .get("sess_test", &terminal_id)
            .await
            .expect("handle");

        assert!(handle.exit_status().is_none());
        handle.request_kill(Duration::from_millis(200)).await;
        let status = handle.wait_for_exit().await;
        assert_eq!(status.exit_code, None);
        assert_eq!(status.signal.as_deref(), Some("SIGTERM"));
    }

    #[tokio::test]
    async fn create_terminal_defaults_cwd_to_session_cwd() {
        use agent_client_protocol::schema::v1::SessionId;

        struct CwdStubSink {
            cwd: String,
        }
        impl SessionEventSink for CwdStubSink {
            fn session_cwd<'a>(
                &'a self,
                _agent_session_id: &'a str,
            ) -> futures::future::BoxFuture<'a, Option<String>> {
                let cwd = self.cwd.clone();
                Box::pin(async move { Some(cwd) })
            }
            fn append<'a>(
                &'a self,
                _session_id: &'a str,
                _kind: &'a str,
                _payload_json: &'a str,
            ) -> futures::future::BoxFuture<'a, ()> {
                Box::pin(async {})
            }
        }

        let root = tempfile::tempdir().expect("workspace root");
        let session_dir = root.path().join("session-sub");
        std::fs::create_dir(&session_dir).expect("session subdir");

        let context = TerminalHandlerContext {
            registry: Arc::new(TerminalRegistry::default()),
            workspace_root: root.path().to_path_buf(),
            sandbox: crate::config::SandboxConfig::default(),
            network_provider: None,
            command_log: None,
            sink: Arc::new(CwdStubSink {
                cwd: session_dir.to_string_lossy().into_owned(),
            }),
        };
        // No cwd on the request: the handler must fall back to the session's
        // recorded cwd, not the workspace root.
        let request = CreateTerminalRequest::new(SessionId::new("sess_agent"), "/bin/pwd");
        let response = handle_create_terminal(&context, request)
            .await
            .expect("terminal created");

        let handle = context
            .registry
            .get("sess_agent", &response.terminal_id.0)
            .await
            .expect("handle");
        let status = handle.wait_for_exit().await;
        assert_eq!(status.exit_code, Some(0));
        let output = handle.buffer.lock().await.data.clone();
        let expected = std::fs::canonicalize(&session_dir).expect("canonical session dir");
        assert_eq!(output.trim_end(), expected.to_string_lossy().as_ref());
    }

    struct NoopStubSink;
    impl SessionEventSink for NoopStubSink {
        fn append<'a>(
            &'a self,
            _session_id: &'a str,
            _kind: &'a str,
            _payload_json: &'a str,
        ) -> futures::future::BoxFuture<'a, ()> {
            Box::pin(async {})
        }
    }

    #[tokio::test]
    async fn create_terminal_start_failure_finalizes_command_row() {
        use agent_client_protocol::schema::v1::SessionId;

        let state_dir = tempfile::tempdir().expect("tempdir");
        let db_path = state_dir.path().join("state.sqlite");
        let store = StateStore::open(db_path.clone()).expect("state open");
        store.migrate().expect("migrate");
        // Force start_command to fail while append_command (INSERT) and the
        // failure finalization (UPDATE to `failed`) still succeed: block only
        // the pending -> running transition.
        {
            let conn = rusqlite::Connection::open(&db_path).expect("second conn");
            conn.execute_batch(
                "CREATE TRIGGER block_running BEFORE UPDATE ON commands \
                 WHEN NEW.status = 'running' \
                 BEGIN SELECT RAISE(ABORT, 'forced start failure'); END;",
            )
            .expect("trigger installed");
        }
        let state = Arc::new(TokioMutex::new(store));

        let context = TerminalHandlerContext {
            registry: Arc::new(TerminalRegistry::default()),
            workspace_root: std::env::temp_dir(),
            sandbox: crate::config::SandboxConfig::default(),
            network_provider: None,
            command_log: Some(TerminalCommandLog {
                state: state.clone(),
                event_hub: EventHub::new(),
            }),
            sink: Arc::new(NoopStubSink),
        };
        let request = CreateTerminalRequest::new(SessionId::new("sess_agent"), "/bin/echo");
        handle_create_terminal(&context, request)
            .await
            .expect_err("start failure must surface as an error");

        // The row must not be left `pending`: the handler finalizes it as
        // `failed` before returning.
        let commands = state
            .lock()
            .await
            .query_commands(crate::state::CommandFilter {
                limit: 10,
                ..Default::default()
            })
            .expect("query commands");
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].status, "failed");
    }

    #[tokio::test]
    async fn kill_finalizes_command_row_as_canceled() {
        use agent_client_protocol::schema::v1::SessionId;

        let state_dir = tempfile::tempdir().expect("tempdir");
        let store = StateStore::open(state_dir.path().join("state.sqlite")).expect("state open");
        store.migrate().expect("migrate");
        let state = Arc::new(TokioMutex::new(store));

        let context = TerminalHandlerContext {
            registry: Arc::new(TerminalRegistry::default()),
            workspace_root: std::env::temp_dir(),
            sandbox: crate::config::SandboxConfig::default(),
            network_provider: None,
            command_log: Some(TerminalCommandLog {
                state: state.clone(),
                event_hub: EventHub::new(),
            }),
            sink: Arc::new(NoopStubSink),
        };
        let request = CreateTerminalRequest::new(SessionId::new("sess_agent"), "/bin/sh")
            .args(vec!["-c".to_owned(), "sleep 30".to_owned()]);
        let response = handle_create_terminal(&context, request)
            .await
            .expect("terminal created");
        let handle = context
            .registry
            .get("sess_agent", &response.terminal_id.0)
            .await
            .expect("handle");

        handle.request_kill(Duration::from_millis(200)).await;
        let status = handle.wait_for_exit().await;
        assert_eq!(status.signal.as_deref(), Some("SIGTERM"));

        // Kill-intent exits finalize as `canceled` with no exit status,
        // matching the gateway's operator-cancel mapping; the ACP-side
        // TerminalExitStatus above still carries the signal.
        let guard = state.lock().await;
        let commands = guard
            .query_commands(crate::state::CommandFilter {
                limit: 10,
                ..Default::default()
            })
            .expect("query commands");
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].status, "canceled");
        assert_eq!(commands[0].exit_status, None);
        let events = guard
            .query_events(crate::state::LogFilter {
                limit: 10,
                kind: Some("command.canceled"),
                ..Default::default()
            })
            .expect("query events");
        assert_eq!(events.len(), 1, "expected one command.canceled event");
    }

    #[tokio::test]
    async fn closed_registry_rejects_registration_and_kills_child() {
        let cwd = std::env::temp_dir();
        let resolved = crate::runtime::mediation::commands::policy::resolve_cwd_under_workspace(
            &cwd,
            &cwd.to_string_lossy(),
        )
        .expect("resolve cwd");
        let child = crate::runtime::mediation::commands::exec::spawn_child(
            std::path::Path::new("/bin/sh"),
            &["-c".to_owned(), "sleep 30".to_owned()],
            &resolved,
            None,
            &crate::config::SandboxConfig::default(),
            None,
        )
        .expect("spawn");
        let pid = child.id().expect("child pid") as i32;

        let registry = Arc::new(TerminalRegistry::default());
        registry.drain_all().await;

        let registered = registry
            .register("sess_test", child, DEFAULT_TERMINAL_OUTPUT_BYTE_LIMIT, None)
            .await;
        assert!(
            registered.is_none(),
            "closed registry must refuse registration"
        );
        // register() reaps the child before returning None, so the pid must
        // already be gone (ESRCH) — a live process here is the shutdown
        // orphan this path exists to prevent.
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        assert!(!alive, "child survived closed-registry registration");
    }
}
