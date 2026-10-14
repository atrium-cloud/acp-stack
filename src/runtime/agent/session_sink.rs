//! Sink abstraction for ACP `session/update` notifications and the
//! `StateStore`-backed implementation used by the daemon.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::schema::{
    MaybeUndefined,
    v1::{AvailableCommandInput, SessionUpdate},
};
use tokio::sync::{Mutex as TokioMutex, oneshot};
use tokio::task::JoinHandle;

use crate::runtime::agent::session_changes::SessionChangesHandle;
use crate::state::{
    MAX_SESSION_AVAILABLE_COMMANDS, SessionAvailableCommand, StateHandle, StateStore,
};

/// Sink for ACP `session/update` notifications. `append` must accept events in
/// order, `drain` must resolve once every earlier `append` is durable, and
/// `flush` must drain any background writer, or a fast shutdown drops
/// in-flight writes.
pub trait SessionEventSink: Send + Sync + 'static {
    fn capture_session_update<'a>(
        &'a self,
        agent_session_id: &'a str,
        update: &'a SessionUpdate,
    ) -> futures::future::BoxFuture<'a, bool> {
        let _ = update;
        Box::pin(async move {
            let _ = agent_session_id;
            true
        })
    }

    fn local_session_id<'a>(
        &'a self,
        agent_session_id: &'a str,
    ) -> futures::future::BoxFuture<'a, Option<String>> {
        Box::pin(async move { Some(agent_session_id.to_owned()) })
    }

    /// Drop any remembered local session id for `agent_session_id`, after its local
    /// row was deleted.
    fn forget_session<'a>(
        &'a self,
        agent_session_id: &'a str,
    ) -> futures::future::BoxFuture<'a, ()> {
        let _ = agent_session_id;
        Box::pin(async {})
    }

    /// Remove and return the adapter's own `messageId` from the most recent
    /// `agent_message_chunk` seen on this session. It is kept in memory rather
    /// than written per chunk because a turn produces thousands of them and
    /// only the last one anchors a fork point.
    fn take_last_agent_message_id<'a>(
        &'a self,
        agent_session_id: &'a str,
    ) -> futures::future::BoxFuture<'a, Option<String>> {
        let _ = agent_session_id;
        Box::pin(async move { None })
    }

    /// Default working directory for `terminal/create` requests that omit
    /// `cwd`; `None` makes callers fall back to the workspace root.
    fn session_cwd<'a>(
        &'a self,
        agent_session_id: &'a str,
    ) -> futures::future::BoxFuture<'a, Option<String>> {
        let _ = agent_session_id;
        Box::pin(async move { None })
    }

    fn append<'a>(
        &'a self,
        session_id: &'a str,
        kind: &'a str,
        payload_json: &'a str,
    ) -> futures::future::BoxFuture<'a, ()>;

    /// Wait until every `append` accepted before this call has been written.
    /// The sink stays open afterwards.
    fn drain<'a>(&'a self) -> futures::future::BoxFuture<'a, ()> {
        Box::pin(async {})
    }

    fn flush<'a>(&'a self) -> futures::future::BoxFuture<'a, ()> {
        Box::pin(async {})
    }
}

/// `SessionEventSink` backed by the daemon's real `StateStore`. Writes flow
/// through a bounded channel into one background writer task; the bound is
/// what stops a runaway agent from exhausting daemon memory.
pub struct StateStoreSessionSink {
    target_id: String,
    state: StateHandle,
    session_changes: SessionChangesHandle,
    tx: TokioMutex<Option<tokio::sync::mpsc::Sender<SessionWrite>>>,
    writer: TokioMutex<Option<JoinHandle<()>>>,
    /// Newest `agent_message_chunk` `messageId` per agent session id, keyed by
    /// the adapter's own session id. One entry per open session, replaced on
    /// every chunk and taken when the turn settles.
    last_agent_message_ids: TokioMutex<HashMap<String, String>>,
    local_session_ids: TokioMutex<LocalSessionIds>,
}

/// Local session id per agent session id on this target, so a streamed update
/// reaches the writer without a state read. Only hits are cached; a session
/// delete forgets its entry because a re-imported agent session gets a new id.
#[derive(Default)]
struct LocalSessionIds {
    ids: HashMap<String, String>,
    /// Bumped by every forget, so a lookup whose read raced one does not cache
    /// a row the forget's delete removed.
    generation: u64,
}

/// Maximum wall time `drain` waits for the writer to reach its barrier. A
/// prompt submission awaits this, so a wedged writer costs the caller a bounded
/// delay and an out-of-order log row rather than a hung request.
const SESSION_EVENT_DRAIN_BUDGET: Duration = Duration::from_secs(5);

/// `sessionUpdate` tags of the updates projected onto the session row.
const SESSION_INFO_UPDATE_KIND: &str = "session_info_update";
const AVAILABLE_COMMANDS_UPDATE_KIND: &str = "available_commands_update";
const CONFIG_OPTION_UPDATE_KIND: &str = "config_option_update";

enum SessionWrite {
    Row(SessionEventRow),
    /// Ordering barrier: acknowledged once every row queued before it is
    /// durable, since the writer handles the channel strictly in order.
    Barrier(oneshot::Sender<()>),
}

struct SessionEventRow {
    session_id: String,
    kind: String,
    payload_json: String,
}

/// The `sessionUpdate` tag of a recorded `session/update` payload.
fn session_update_kind(payload: &serde_json::Value) -> Option<&str> {
    payload
        .get("update")?
        .get("sessionUpdate")
        .and_then(serde_json::Value::as_str)
}

/// The typed ACP update inside a recorded payload whose tag is `expected_kind`.
fn typed_session_update(
    payload: &serde_json::Value,
    expected_kind: &str,
) -> crate::error::Result<Option<SessionUpdate>> {
    if session_update_kind(payload) != Some(expected_kind) {
        return Ok(None);
    }
    let Some(update) = payload.get("update") else {
        return Ok(None);
    };
    serde_json::from_value::<SessionUpdate>(update.clone())
        .map(Some)
        .map_err(|err| crate::error::StackError::StateInvalidJson {
            field: "session.update.update",
            reason: err.to_string(),
        })
}

/// Normalize standard ACP `usage_update` notifications and the legacy usage
/// objects emitted by older agents.
fn extract_usage_payload(session_id: &str, value: &serde_json::Value) -> Option<serde_json::Value> {
    if let Some(update) = value.get("update")
        && update
            .get("sessionUpdate")
            .and_then(serde_json::Value::as_str)
            == Some("usage_update")
    {
        let context_window_used = read_token_field(update, "used")?;
        let context_window_max = read_token_field(update, "size")?;
        let mut out = serde_json::Map::new();
        out.insert(
            "session_id".to_owned(),
            serde_json::Value::String(session_id.to_owned()),
        );
        out.insert(
            "context_window_used".to_owned(),
            serde_json::Value::Number(serde_json::Number::from(context_window_used)),
        );
        out.insert(
            "context_window_max".to_owned(),
            serde_json::Value::Number(serde_json::Number::from(context_window_max)),
        );
        if let Some(cost) = update.get("cost") {
            if let Some(amount) = cost.get("amount").and_then(serde_json::Value::as_f64)
                && let Some(amount) = serde_json::Number::from_f64(amount)
            {
                out.insert("cost_amount".to_owned(), serde_json::Value::Number(amount));
            }
            if let Some(currency) = cost.get("currency").and_then(serde_json::Value::as_str) {
                out.insert(
                    "cost_currency".to_owned(),
                    serde_json::Value::String(currency.to_owned()),
                );
            }
        }
        return Some(serde_json::Value::Object(out));
    }

    let usage = locate_usage_object(value)?;
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

/// Project an ACP `session_info_update`; must run only after the verbatim
/// `session.update` row is durable, since that row is the source of truth.
fn project_session_info_update(
    store: &StateStore,
    session_id: &str,
    payload: &serde_json::Value,
) -> crate::error::Result<()> {
    let Some(SessionUpdate::SessionInfoUpdate(info)) =
        typed_session_update(payload, SESSION_INFO_UPDATE_KIND)?
    else {
        return Ok(());
    };
    let title = match &info.title {
        MaybeUndefined::Undefined => None,
        MaybeUndefined::Null => Some(None),
        MaybeUndefined::Value(value) => Some(Some(value.as_str())),
    };
    let agent_updated_at = match &info.updated_at {
        MaybeUndefined::Undefined => None,
        MaybeUndefined::Null => Some(None),
        MaybeUndefined::Value(value) => Some(Some(value.as_str())),
    };
    store.update_session_info(session_id, title, agent_updated_at, info.meta.as_ref())
}

/// Project an ACP `available_commands_update`; latest-wins, and an empty list
/// still replaces.
fn project_available_commands_update(
    store: &StateStore,
    session_id: &str,
    payload: &serde_json::Value,
) -> crate::error::Result<()> {
    let Some(SessionUpdate::AvailableCommandsUpdate(update)) =
        typed_session_update(payload, AVAILABLE_COMMANDS_UPDATE_KIND)?
    else {
        return Ok(());
    };
    let advertised_len = update.available_commands.len();
    let mut commands: Vec<SessionAvailableCommand> = update
        .available_commands
        .into_iter()
        .map(|command| SessionAvailableCommand {
            name: command.name,
            description: command.description,
            // The input enum is non-exhaustive; unknown future variants
            // degrade to no hint rather than failing the projection.
            input_hint: command.input.and_then(|input| match input {
                AvailableCommandInput::Unstructured(input) => Some(input.hint),
                _ => None,
            }),
        })
        .collect();
    let truncated = commands.len() > MAX_SESSION_AVAILABLE_COMMANDS;
    if truncated {
        commands.truncate(MAX_SESSION_AVAILABLE_COMMANDS);
    }
    let changed = store.replace_session_available_commands(session_id, &commands)?;
    // Gated on `changed` so an agent re-advertising the same oversized list
    // every turn does not spam the log on every no-op write.
    if truncated && changed {
        tracing::warn!(
            session_id = %session_id,
            advertised = advertised_len,
            stored = MAX_SESSION_AVAILABLE_COMMANDS,
            "agent advertised more commands than the stored cap; list truncated"
        );
    }
    Ok(())
}

/// Project an ACP `config_option_update` into the per-session snapshot.
/// Unlike the set-response writers, an empty list is applied here: a
/// notification is the agent's authoritative full-set advertisement.
fn project_config_options_update(
    store: &StateStore,
    session_id: &str,
    payload: &serde_json::Value,
) -> crate::error::Result<()> {
    let Some(SessionUpdate::ConfigOptionUpdate(update)) =
        typed_session_update(payload, CONFIG_OPTION_UPDATE_KIND)?
    else {
        return Ok(());
    };
    let advertised_len = update.config_options.len();
    let mut snapshot =
        crate::runtime::agent::config_options::project_config_options(&update.config_options);
    let truncated = snapshot.len() > crate::state::MAX_SESSION_CONFIG_OPTIONS;
    if truncated {
        snapshot.truncate(crate::state::MAX_SESSION_CONFIG_OPTIONS);
    }
    let options_value = serde_json::to_value(&snapshot).map_err(|err| {
        crate::error::StackError::StateInvalidJson {
            field: "sessions.metadata_json",
            reason: err.to_string(),
        }
    })?;
    let changed = store.replace_session_config_options(session_id, options_value)?;
    if truncated && changed {
        tracing::warn!(
            session_id = %session_id,
            advertised = advertised_len,
            stored = crate::state::MAX_SESSION_CONFIG_OPTIONS,
            "agent advertised more config options than the stored cap; list truncated"
        );
    }
    Ok(())
}

/// Derive a `tool.execute` event from an `execute`-kind tool call. Updates
/// omitting `kind` yield `None`, since ACP only requires it on the initial
/// `tool_call`; the verbatim `session.update` rows keep the full lifecycle.
fn extract_execute_tool_call(
    session_id: &str,
    value: &serde_json::Value,
) -> Option<serde_json::Value> {
    let update = value.get("update")?;
    let update_kind = update.get("sessionUpdate").and_then(|v| v.as_str())?;
    if !matches!(update_kind, "tool_call" | "tool_call_update") {
        return None;
    }
    if update.get("kind").and_then(|v| v.as_str()) != Some("execute") {
        return None;
    }
    let tool_call_id = update.get("toolCallId").and_then(|v| v.as_str())?;
    let mut out = serde_json::Map::new();
    out.insert(
        "session_id".to_owned(),
        serde_json::Value::String(session_id.to_owned()),
    );
    out.insert(
        "tool_call_id".to_owned(),
        serde_json::Value::String(tool_call_id.to_owned()),
    );
    for key in ["status", "title"] {
        if let Some(text) = update.get(key).and_then(|v| v.as_str()) {
            out.insert(key.to_owned(), serde_json::Value::String(text.to_owned()));
        }
    }
    // The common built-in shell tools put the command line at
    // `rawInput.command`; agents without that convention carry it in `title`.
    if let Some(command) = update
        .get("rawInput")
        .and_then(|v| v.get("command"))
        .and_then(|v| v.as_str())
    {
        out.insert(
            "command".to_owned(),
            serde_json::Value::String(command.to_owned()),
        );
    }
    Some(serde_json::Value::Object(out))
}

fn locate_usage_object(value: &serde_json::Value) -> Option<&serde_json::Value> {
    if let Some(obj) = value.get("usage")
        && obj.is_object()
    {
        return Some(obj);
    }
    if let Some(update) = value.get("update").and_then(|v| v.get("usage"))
        && update.is_object()
    {
        return Some(update);
    }
    if let Some(prompt_response) = value.get("prompt_response").and_then(|v| v.get("usage"))
        && prompt_response.is_object()
    {
        return Some(prompt_response);
    }
    if let Some(meta_usage) = value.get("meta").and_then(|v| v.get("usage"))
        && meta_usage.is_object()
    {
        return Some(meta_usage);
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

/// Backpressure buffer for unwritten ACP session updates.
pub(crate) const SESSION_EVENT_BUFFER: usize = 1024;

/// Most queued updates the sink writes in one state job. Bounds how long one batch
/// holds the state writer while other writes wait behind it.
const SESSION_WRITE_BATCH_MAX: usize = 128;

/// Record one session update and everything derived from it in one write job. Only the
/// update row propagates its error; a failed projection or derived row is logged so the
/// recorded update survives it. The caller parses `payload` so the writer thread, which
/// commits the whole batch, never parses a full update.
fn persist_session_row(
    store: &StateStore,
    row: &SessionEventRow,
    payload: &serde_json::Value,
) -> crate::error::Result<()> {
    // The event row and the running-prompt touch share one commit; the projections
    // and derived rows below follow only for the update kinds that carry them.
    store.append_session_update(&row.session_id, &row.kind, &row.payload_json)?;
    if let Err(err) = project_session_info_update(store, &row.session_id, payload) {
        tracing::warn!(
            error = %err,
            session_id = %row.session_id,
            "failed to apply ACP session info update"
        );
    }
    if let Err(err) = project_available_commands_update(store, &row.session_id, payload) {
        tracing::warn!(
            error = %err,
            session_id = %row.session_id,
            "failed to apply ACP available commands update"
        );
    }
    if let Err(err) = project_config_options_update(store, &row.session_id, payload) {
        tracing::warn!(
            error = %err,
            session_id = %row.session_id,
            "failed to apply ACP config option update"
        );
    }
    if let Some(usage) = extract_usage_payload(&row.session_id, payload)
        && let Ok(usage_text) = serde_json::to_string(&usage)
        && let Err(err) = store.append_session_event_with_source(
            &row.session_id,
            "info",
            "usage.reported",
            crate::state::EVENT_SOURCE_ACP,
            "agent usage reported",
            &usage_text,
        )
    {
        tracing::warn!(
            error = %err,
            session_id = %row.session_id,
            "failed to persist usage.reported event"
        );
    }
    if let Some(execute) = extract_execute_tool_call(&row.session_id, payload)
        && let Ok(execute_text) = serde_json::to_string(&execute)
        && let Err(err) = store.append_session_event_with_source(
            &row.session_id,
            "info",
            "tool.execute",
            crate::state::EVENT_SOURCE_ACP,
            "agent execute tool call",
            &execute_text,
        )
    {
        tracing::warn!(
            error = %err,
            session_id = %row.session_id,
            "failed to persist tool.execute event"
        );
    }
    Ok(())
}

impl StateStoreSessionSink {
    pub fn new(target_id: String, state: StateHandle) -> Self {
        Self::with_session_changes(target_id, state, SessionChangesHandle::new())
    }

    pub(crate) fn with_session_changes(
        target_id: String,
        state: StateHandle,
        session_changes: SessionChangesHandle,
    ) -> Self {
        let (tx, rx) = tokio::sync::mpsc::channel::<SessionWrite>(SESSION_EVENT_BUFFER);
        let writer = tokio::spawn(run_session_writer(state.clone(), rx));
        Self {
            target_id,
            state,
            session_changes,
            tx: TokioMutex::new(Some(tx)),
            writer: TokioMutex::new(Some(writer)),
            last_agent_message_ids: TokioMutex::new(HashMap::new()),
            local_session_ids: TokioMutex::new(LocalSessionIds::default()),
        }
    }
}

/// Drain the sink's queue in batches and write each batch as one state job, so a
/// stream advances a batch per commit rather than a row per commit. Barriers in a
/// batch are acknowledged once the whole batch is written.
async fn run_session_writer(
    state: StateHandle,
    mut receiver: tokio::sync::mpsc::Receiver<SessionWrite>,
) {
    let mut writes = Vec::with_capacity(SESSION_WRITE_BATCH_MAX);
    while receiver
        .recv_many(&mut writes, SESSION_WRITE_BATCH_MAX)
        .await
        > 0
    {
        let mut rows = Vec::new();
        let mut barriers = Vec::new();
        for write in writes.drain(..) {
            match write {
                SessionWrite::Row(row) => {
                    // The store rejects a payload that is not JSON, so it is dropped here.
                    match serde_json::from_str::<serde_json::Value>(&row.payload_json) {
                        Ok(payload) => rows.push((row, payload)),
                        Err(err) => tracing::warn!(
                            error = %err,
                            session_id = %row.session_id,
                            "ACP session update is not JSON; dropping it"
                        ),
                    }
                }
                SessionWrite::Barrier(ack) => barriers.push(ack),
            }
        }
        if !rows.is_empty() {
            let rows = Arc::new(rows);
            // Per-row failures are handled inside the job, so a failed job means its
            // transaction failed and committed none of the rows; one retry in a fresh
            // transaction cannot duplicate them.
            let mut written = write_session_rows(&state, &rows).await;
            if let Err(err) = &written {
                tracing::warn!(
                    error = %err,
                    rows = rows.len(),
                    "a batch of ACP session updates failed; retrying it once"
                );
                written = write_session_rows(&state, &rows).await;
            }
            if let Err(err) = written {
                tracing::warn!(
                    error = %err,
                    rows = rows.len(),
                    "failed to persist a batch of ACP session updates"
                );
            }
        }
        for ack in barriers {
            if ack.send(()).is_err() {
                tracing::debug!("session event drain waiter gave up before the barrier");
            }
        }
    }
}

/// Write a batch of session updates as one state job. Each row is its own write
/// scope, so a failing row rolls back alone.
async fn write_session_rows(
    state: &StateHandle,
    rows: &Arc<Vec<(SessionEventRow, serde_json::Value)>>,
) -> crate::error::Result<()> {
    let rows = Arc::clone(rows);
    state
        .write(move |store| {
            for (row, payload) in rows.iter() {
                if let Err(err) = persist_session_row(store, row, payload) {
                    tracing::warn!(
                        error = %err,
                        session_id = %row.session_id,
                        "failed to persist ACP session update"
                    );
                }
            }
            Ok(())
        })
        .await
}

impl SessionEventSink for StateStoreSessionSink {
    fn capture_session_update<'a>(
        &'a self,
        agent_session_id: &'a str,
        update: &'a SessionUpdate,
    ) -> futures::future::BoxFuture<'a, bool> {
        Box::pin(async move {
            // An adapter that resolves a fork point against its own transcript
            // needs one of its own ids; this is the only place they arrive.
            if let SessionUpdate::AgentMessageChunk(chunk) = update
                && let Some(message_id) = chunk.message_id.as_ref()
            {
                self.last_agent_message_ids
                    .lock()
                    .await
                    .insert(agent_session_id.to_owned(), message_id.0.to_string());
            }
            // Only tool-call updates carry diff content; skipping the session
            // lookup otherwise keeps chunk-heavy streams off the state database.
            if !matches!(
                update,
                SessionUpdate::ToolCall(_) | SessionUpdate::ToolCallUpdate(_)
            ) {
                return true;
            }
            let Some(local_session_id) = self.local_session_id(agent_session_id).await else {
                return false;
            };
            self.session_changes.apply(&local_session_id, update).await;
            true
        })
    }

    fn local_session_id<'a>(
        &'a self,
        agent_session_id: &'a str,
    ) -> futures::future::BoxFuture<'a, Option<String>> {
        Box::pin(async move {
            let generation = {
                let cache = self.local_session_ids.lock().await;
                if let Some(session_id) = cache.ids.get(agent_session_id) {
                    return Some(session_id.clone());
                }
                cache.generation
            };
            let target_id = self.target_id.clone();
            let lookup_agent_session_id = agent_session_id.to_owned();
            let record = self
                .state
                .read(move |store| {
                    store.get_session_by_target_agent_session_id(
                        &target_id,
                        &lookup_agent_session_id,
                    )
                })
                .await;
            match record {
                Ok(Some(record)) => {
                    let mut cache = self.local_session_ids.lock().await;
                    // A forget during the read may have deleted this row after the
                    // read's snapshot, so the result is used once but not cached.
                    if cache.generation == generation {
                        cache
                            .ids
                            .insert(agent_session_id.to_owned(), record.id.clone());
                    }
                    Some(record.id)
                }
                Ok(None) => {
                    tracing::warn!(
                        target_id = %self.target_id,
                        agent_session_id,
                        "dropping ACP session update for unknown Array target session"
                    );
                    None
                }
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        target_id = %self.target_id,
                        agent_session_id,
                        "failed to resolve ACP session id to local session id"
                    );
                    None
                }
            }
        })
    }

    fn forget_session<'a>(
        &'a self,
        agent_session_id: &'a str,
    ) -> futures::future::BoxFuture<'a, ()> {
        Box::pin(async move {
            {
                let mut cache = self.local_session_ids.lock().await;
                cache.ids.remove(agent_session_id);
                cache.generation += 1;
            }
            // An unsettled turn's anchor must not carry over to a re-created session.
            self.last_agent_message_ids
                .lock()
                .await
                .remove(agent_session_id);
        })
    }

    fn take_last_agent_message_id<'a>(
        &'a self,
        agent_session_id: &'a str,
    ) -> futures::future::BoxFuture<'a, Option<String>> {
        Box::pin(async move {
            self.last_agent_message_ids
                .lock()
                .await
                .remove(agent_session_id)
        })
    }

    fn session_cwd<'a>(
        &'a self,
        agent_session_id: &'a str,
    ) -> futures::future::BoxFuture<'a, Option<String>> {
        Box::pin(async move {
            let target_id = self.target_id.clone();
            let lookup_agent_session_id = agent_session_id.to_owned();
            let record = self
                .state
                .read(move |store| {
                    store.get_session_by_target_agent_session_id(
                        &target_id,
                        &lookup_agent_session_id,
                    )
                })
                .await;
            match record {
                Ok(Some(record)) => Some(record.cwd),
                Ok(None) => None,
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        target_id = %self.target_id,
                        agent_session_id,
                        "failed to resolve ACP session id to local session cwd"
                    );
                    None
                }
            }
        })
    }

    fn append<'a>(
        &'a self,
        agent_session_id: &'a str,
        kind: &'a str,
        payload_json: &'a str,
    ) -> futures::future::BoxFuture<'a, ()> {
        Box::pin(async move {
            let Some(session_id) = self.local_session_id(agent_session_id).await else {
                return;
            };
            let sender = {
                let guard = self.tx.lock().await;
                match guard.as_ref() {
                    Some(tx) => tx.clone(),
                    None => {
                        tracing::warn!(
                            agent_session_id,
                            "session event sink is closed; dropping update"
                        );
                        return;
                    }
                }
            };
            if let Err(err) = sender
                .send(SessionWrite::Row(SessionEventRow {
                    session_id,
                    kind: kind.to_owned(),
                    payload_json: payload_json.to_owned(),
                }))
                .await
            {
                tracing::warn!(
                    error = %err,
                    agent_session_id,
                    "session event writer task ended; dropping update"
                );
            }
        })
    }

    fn drain<'a>(&'a self) -> futures::future::BoxFuture<'a, ()> {
        Box::pin(async move {
            let sender = {
                let guard = self.tx.lock().await;
                match guard.as_ref() {
                    Some(tx) => tx.clone(),
                    // A closed sink has already been flushed, so nothing is
                    // left to wait for.
                    None => return,
                }
            };
            let (ack_tx, ack_rx) = oneshot::channel();
            // The budget covers queueing the barrier too: a full channel
            // behind a stalled writer must not park the caller indefinitely.
            let barrier = async move {
                sender
                    .send(SessionWrite::Barrier(ack_tx))
                    .await
                    .map_err(|_| ())?;
                ack_rx.await.map_err(|_| ())
            };
            match tokio::time::timeout(SESSION_EVENT_DRAIN_BUDGET, barrier).await {
                Ok(Ok(())) => {}
                Ok(Err(())) => {
                    tracing::warn!("session event writer task ended before the drain barrier");
                }
                Err(_) => {
                    tracing::warn!(
                        budget_secs = SESSION_EVENT_DRAIN_BUDGET.as_secs(),
                        "session event writer did not reach the drain barrier in time"
                    );
                }
            }
        })
    }

    fn flush<'a>(&'a self) -> futures::future::BoxFuture<'a, ()> {
        Box::pin(async move {
            {
                let mut guard = self.tx.lock().await;
                // Dropping the sender lets the writer observe EOF and drain
                // its queue before exiting. Idempotent.
                *guard = None;
            }
            let writer = self.writer.lock().await.take();
            if let Some(task) = writer
                && let Err(err) = task.await
            {
                tracing::warn!(
                    error = ?err,
                    "session event writer task did not exit cleanly"
                );
            }
        })
    }
}

#[cfg(test)]
mod tests;
