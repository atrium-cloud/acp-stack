//! Client-terminal substrate for the ACP bridge (`terminal/*` methods): one
//! owning task per terminal holds the `Child`; the registry holds only cheap
//! shared endpoints (buffer, exit watch, kill sender).

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

/// In-memory replay cap applied when the agent omits `outputByteLimit`.
pub(crate) const DEFAULT_TERMINAL_OUTPUT_BYTE_LIMIT: u64 = 1024 * 1024;

/// Hard ceiling on the in-memory replay buffer regardless of the requested
/// `outputByteLimit`; the durable command log is capped separately.
pub(crate) const MAX_TERMINAL_OUTPUT_BYTE_LIMIT: u64 = 10 * 1024 * 1024;

/// SIGTERM -> SIGKILL escalation window for kill, release, and shutdown.
pub(crate) const TERMINAL_KILL_GRACE: Duration = Duration::from_secs(2);

const OUTPUT_CHANNEL_CAPACITY: usize = 64;

const TERMINAL_ID_PREFIX: &str = "term_";

/// Rolling output buffer for one terminal; `truncated` latches once any byte
/// has been dropped.
#[derive(Debug, Default)]
pub(crate) struct TerminalBuffer {
    pub(crate) data: String,
    pub(crate) truncated: bool,
}

impl TerminalBuffer {
    /// Append a chunk, then trim in place to `limit` retaining the NEWEST
    /// bytes at a char boundary. Trimming here, not at read time, is what
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

/// Shared endpoints for one live (or exited-but-unreleased) terminal.
pub(crate) struct TerminalHandle {
    pub(crate) buffer: Arc<TokioMutex<TerminalBuffer>>,
    pub(crate) exit_rx: watch::Receiver<Option<TerminalExitStatus>>,
    kill_tx: mpsc::Sender<Duration>,
    pub(crate) output_byte_limit: u64,
}

impl TerminalHandle {
    /// Ask the owning task to terminate the child (SIGTERM, `grace`, then
    /// SIGKILL). Idempotent: a failed send means the owner already reaped.
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
                // Owner panicked before publishing; surface a bare status
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

/// Durable command-log target for client terminals: the store command rows
/// land in, plus the hub the `commands.{id}` topic fans out through.
#[derive(Clone)]
pub struct TerminalCommandLog {
    pub state: Arc<TokioMutex<StateStore>>,
    pub event_hub: EventHub,
}

/// Durable command-log attachment for one terminal.
pub(crate) struct TerminalPersistence {
    pub(crate) command_log: TerminalCommandLog,
    pub(crate) command_id: String,
}

impl TerminalRegistry {
    /// Take ownership of a freshly spawned child and return the minted
    /// terminal id, or `None` (after killing the child) once `drain_all` has
    /// closed the registry. The entries lock MUST stay held from the closed
    /// check through the insert, so a concurrent `drain_all` either sees the
    /// new terminal or the register sees `closed` — otherwise shutdown leaks
    /// an orphan process.
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

    /// Remove and return the handle (`terminal/release`).
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

    /// Kill-and-release every live terminal and refuse future registrations.
    /// Needed on shutdown because terminal children live in their own process
    /// groups, so the agent-process-group kill never reaches them.
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

/// Single owner of the `Child`. Publishes the exit status LAST, after the
/// post-exit drain and command-row finalize, so a `terminal/wait_for_exit`
/// response guarantees the output and command log are already complete.
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

    // Distinguishes owner-caused exits (recorded `cancelled`) from natural
    // signal deaths like OOM kill or segfault (recorded `failed`).
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
                    // cancellation.
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

    // Reap descendants that inherited the pipes.
    if let Some(pid) = pid {
        kill_process_group_pid(pid);
    }

    // Drain the remaining chunks BEFORE finalizing, so the exit status is
    // never observable while output is still in flight. Bounded because a
    // `setsid`/`nohup` descendant that escaped the group kill holds the pipes
    // open forever.
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
        let (command_status, event_kind) = if canceled {
            (CommandStatus::Canceled, "command.cancelled")
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
        tracing::debug!("terminal exit published after release; no listeners");
    }
}

/// Append one chunk to the capped in-memory buffer and mirror the untrimmed
/// stream into the durable command log and `commands.{id}` topic.
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

/// Persist and publish a terminal lifecycle transition on `commands.{id}`.
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

/// Everything the `terminal/*` handlers need.
pub(crate) struct TerminalHandlerContext {
    pub(crate) registry: Arc<TerminalRegistry>,
    pub(crate) workspace_root: PathBuf,
    /// Boot-time home captured at bridge spawn: terminal children must see the
    /// daemon's resolved HOME, not whatever the process env holds at request
    /// time.
    pub(crate) home: PathBuf,
    pub(crate) sandbox: crate::config::SandboxConfig,
    pub(crate) network_provider: Option<crate::extensions::NetworkProviderExtension>,
    /// `None` (e.g. discovery probes) means terminals work but leave no
    /// `commands` rows behind.
    pub(crate) command_log: Option<TerminalCommandLog>,
    pub(crate) sink: Arc<dyn SessionEventSink>,
}

/// `terminal/create`: spawn the requested program in a clean session env
/// under the agent's sandbox profile and hand the child to an owning task.
/// Executes directly by design: the VM is the security boundary, and agents
/// send `session/request_permission` separately when their policy needs it.
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

    let env = terminal_environment(&context.home, &request.env);
    let (program, args) = sandboxed_program(
        Path::new(&request.command),
        &request.args,
        &context.sandbox,
        context.network_provider.as_ref(),
        &context.workspace_root,
        &context.home,
    )
    .map_err(AcpError::into_internal_error)?;

    // Insert the durable row before spawning so even a failed spawn leaves an
    // audit trail.
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
                // stays `pending` forever.
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
            mark_failed("create during bridge shutdown").await;
            return Err(AcpError::internal_error().data(serde_json::json!({
                "reason": "agent bridge is shutting down; terminal registry closed",
            })));
        }
    };
    Ok(CreateTerminalResponse::new(TerminalId::new(terminal_id)))
}

/// `terminal/output`: current buffered output, the truncation flag, and the
/// exit status once exited.
pub(crate) async fn handle_terminal_output(
    registry: &TerminalRegistry,
    request: TerminalOutputRequest,
) -> std::result::Result<TerminalOutputResponse, AcpError> {
    let handle = lookup(registry, &request.session_id.0, &request.terminal_id.0).await?;
    let buffer = handle.buffer.lock().await;
    // Re-applied at the read boundary so the response cannot exceed the limit
    // even mid-append.
    let (output, cut_now) = keep_newest(&buffer.data, handle.output_byte_limit);
    let truncated = buffer.truncated || cut_now;
    Ok(TerminalOutputResponse::new(output.to_owned(), truncated).exit_status(handle.exit_status()))
}

/// `terminal/wait_for_exit`: park until the owning task publishes the exit
/// status.
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

fn render_command_line(command: &str, args: &[String]) -> String {
    if args.is_empty() {
        return command.to_owned();
    }
    format!("{command} {}", args.join(" "))
}

/// Env names only: values commonly carry credentials and must never be
/// written to the command log.
fn env_names_json(env: &[EnvVariable]) -> Option<String> {
    if env.is_empty() {
        return None;
    }
    let mut names: Vec<&str> = env.iter().map(|variable| variable.name.as_str()).collect();
    names.sort_unstable();
    serde_json::to_string(&names).ok()
}

/// Clean session environment for a terminal child: managed PATH, HOME, and
/// the vars the agent supplied. Never the `[agent].env` secrets injected into
/// the agent process itself — a client terminal must not expose provider API
/// keys to arbitrary shell commands.
pub(crate) fn terminal_environment(
    home: &Path,
    agent_env: &[EnvVariable],
) -> HashMap<String, String> {
    let mut env = HashMap::new();
    if let Some(path) = agent_process_path(home) {
        match path.into_string() {
            Ok(path) => {
                env.insert("PATH".to_owned(), path);
            }
            Err(_) => {
                tracing::warn!("managed PATH is not valid UTF-8; omitting from terminal env");
            }
        }
    }
    env.insert("HOME".to_owned(), home.to_string_lossy().into_owned());
    // Agent-provided vars win over the managed defaults; the spec gives the
    // agent control of the child env.
    for variable in agent_env {
        env.insert(variable.name.clone(), variable.value.clone());
    }
    env
}

/// Requested `outputByteLimit` (default when omitted), clamped to the ceiling.
pub(crate) fn effective_output_byte_limit(requested: Option<u64>) -> u64 {
    requested
        .unwrap_or(DEFAULT_TERMINAL_OUTPUT_BYTE_LIMIT)
        .min(MAX_TERMINAL_OUTPUT_BYTE_LIMIT)
}

/// Return the tail of `buffer` that fits in `limit` bytes plus whether
/// anything was dropped. ACP truncation keeps the NEWEST bytes; Zed keeps the
/// head, which the spec says to drop.
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

/// Map a reaped process status to the ACP exit shape.
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
mod tests;
