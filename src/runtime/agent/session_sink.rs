//! Sink abstraction for ACP `session/update` notifications and the
//! `StateStore`-backed implementation used by the daemon.
//!
//! Extracted from `acp_bridge.rs` so the bridge file can focus on the live
//! ACP connection lifecycle. The trait and the writer plumbing have no
//! dependency on the bridge struct itself — they only need a `StateStore`
//! handle; the runtime `EventHub` is reached transitively through the store.

use std::str::FromStr;
use std::sync::Arc;

use agent_client_protocol::schema::{MaybeUndefined, v1::SessionUpdate};
use tokio::sync::Mutex as TokioMutex;
use tokio::task::JoinHandle;

use crate::runtime::agent::session_changes::SessionChangesHandle;
use crate::state::{PromptStatus, StateStore};

/// Sink for ACP `session/update` notifications. The bridge writes through this
/// trait instead of holding a `StateStore` directly, so tests can substitute
/// an in-memory sink without standing up a SQLite file.
///
/// `append` returns a future so a real implementation can durably persist the
/// event before the notification handler returns; otherwise a fast shutdown
/// would drop in-flight writes. `flush` waits for any background writer task
/// owned by the sink to drain; the bridge calls it during graceful shutdown.
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

    /// The locally recorded cwd of the session, used as the default working
    /// directory for `terminal/create` requests that omit `cwd`. `None`
    /// (sinks without session state) makes callers fall back to the
    /// workspace root.
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

    fn flush<'a>(&'a self) -> futures::future::BoxFuture<'a, ()> {
        Box::pin(async {})
    }
}

/// `SessionEventSink` backed by the daemon's real `StateStore`.
///
/// Session-update writes flow through a **bounded** mpsc channel into a
/// single background writer task. The bound provides backpressure: a noisy
/// agent that emits updates faster than SQLite drains them blocks at
/// `append`, which yields back to the SDK's notification handler and lets
/// the event loop tick (it never spin-waits, since `send` is async). Without
/// the bound a runaway agent could exhaust daemon memory before any HTTP
/// limit kicks in.
///
/// `flush()` drops the sender, the writer task drains the remaining queue,
/// and we await it during graceful shutdown so no notification rows are lost.
pub struct StateStoreSessionSink {
    target_id: String,
    state: Arc<TokioMutex<StateStore>>,
    session_changes: SessionChangesHandle,
    tx: TokioMutex<Option<tokio::sync::mpsc::Sender<SessionEventRow>>>,
    writer: TokioMutex<Option<JoinHandle<()>>>,
}

struct SessionEventRow {
    session_id: String,
    kind: String,
    payload_json: String,
}

/// Normalize standard ACP `usage_update` notifications and the legacy usage
/// objects emitted by older agents. Returns `None` when no recognized,
/// non-negative usage fields are present.
fn extract_usage_payload(session_id: &str, payload_json: &str) -> Option<serde_json::Value> {
    let value: serde_json::Value = serde_json::from_str(payload_json).ok()?;
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

    let usage = locate_usage_object(&value)?;
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

/// Apply the local projection of a standard ACP `session_info_update` after
/// its verbatim `session.update` event is durable. The raw event is the source
/// of truth; projection failure is logged by the writer and never removes it.
fn project_session_info_update(
    store: &StateStore,
    session_id: &str,
    payload_json: &str,
) -> crate::error::Result<()> {
    let payload = serde_json::from_str::<serde_json::Value>(payload_json).map_err(|err| {
        crate::error::StackError::StateInvalidJson {
            field: "session.update",
            reason: err.to_string(),
        }
    })?;
    let Some(update) = payload.get("update") else {
        return Ok(());
    };
    if update
        .get("sessionUpdate")
        .and_then(serde_json::Value::as_str)
        != Some("session_info_update")
    {
        return Ok(());
    }
    let update = serde_json::from_value::<SessionUpdate>(update.clone()).map_err(|err| {
        crate::error::StackError::StateInvalidJson {
            field: "session.update.update",
            reason: err.to_string(),
        }
    })?;
    let SessionUpdate::SessionInfoUpdate(info) = update else {
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

/// Derive a `tool.execute` event when the inbound `session/update` is a
/// `tool_call` / `tool_call_update` that identifies itself as an `execute`
/// tool. Agents that run shell through their own built-in tools (instead of
/// client terminals) still announce those runs as tool-call blocks; lifting
/// them out of the generic `session.update` stream makes agent shell activity
/// filterable in logs without payload parsing. Updates that omit `kind`
/// (ACP only requires it on the initial `tool_call`) yield `None` — the
/// derived stream marks command starts plus whatever transitions restate the
/// kind; the verbatim `session.update` rows keep the full lifecycle.
fn extract_execute_tool_call(session_id: &str, payload_json: &str) -> Option<serde_json::Value> {
    let value: serde_json::Value = serde_json::from_str(payload_json).ok()?;
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
    // The common built-in shell tools (Claude Code, OpenCode, Pi) put the
    // command line at `rawInput.command`; agents without that convention
    // still carry it in `title`.
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

/// Bump `updated_at` on the oldest `pending`/`running` prompt for the
/// session so the stale-prompt sweeper does not flag an actively
/// streaming prompt. ACP `session/update` carries no `prompt_id`, so the
/// session-scoped lookup is the best precision available; the oldest
/// in-flight prompt is the one currently producing updates. A session
/// with no in-flight prompts is a benign no-op.
fn touch_running_prompt(store: &StateStore, session_id: &str) -> crate::error::Result<()> {
    let prompts = store.in_flight_prompts_for_session(session_id)?;
    let Some(prompt) = prompts.into_iter().next() else {
        return Ok(());
    };
    // `update_prompt_status` advances `updated_at` regardless of the
    // status value. Passing the existing status (Running/Pending) plus
    // None for every other field keeps every other column intact.
    let status = PromptStatus::from_str(&prompt.status)?;
    store
        .update_prompt_status(&prompt.id, status, None, None, None, None, None)
        .map(|_| ())
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

/// Backpressure buffer for unwritten ACP session updates. Sized so a typical
/// streaming turn (text chunks, tool calls) fits comfortably without ever
/// blocking, but small enough that a pathological agent can't grow daemon
/// memory by gigabytes before SQLite catches up.
pub(crate) const SESSION_EVENT_BUFFER: usize = 1024;

impl StateStoreSessionSink {
    pub fn new(target_id: String, state: Arc<TokioMutex<StateStore>>) -> Self {
        Self::with_session_changes(target_id, state, SessionChangesHandle::new())
    }

    pub(crate) fn with_session_changes(
        target_id: String,
        state: Arc<TokioMutex<StateStore>>,
        session_changes: SessionChangesHandle,
    ) -> Self {
        // Session-update fanout now happens inside
        // `append_session_event_with_source` itself because `StateStore` owns
        // the live `EventHub`; the sink no longer needs its own handle.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<SessionEventRow>(SESSION_EVENT_BUFFER);
        let writer_state = state.clone();
        let writer = tokio::spawn(async move {
            while let Some(row) = rx.recv().await {
                let guard = writer_state.lock().await;
                match guard.append_session_event_with_source(
                    &row.session_id,
                    "info",
                    &row.kind,
                    crate::state::EVENT_SOURCE_ACP,
                    "ACP session update",
                    &row.payload_json,
                ) {
                    Ok(_event) => {
                        if let Err(err) =
                            project_session_info_update(&guard, &row.session_id, &row.payload_json)
                        {
                            tracing::warn!(
                                error = %err,
                                session_id = %row.session_id,
                                "failed to apply ACP session info update"
                            );
                        }
                        // Re-touch the in-flight prompt's `updated_at` so the
                        // stale-prompt sweeper does not flip an actively
                        // streaming prompt to `Stalled`. ACP notifications
                        // carry only `session_id`, so we pick the oldest
                        // running prompt for the session (the one the agent
                        // is currently driving) and bump it via the
                        // status-preserving update path. Failures are
                        // logged but do not block the event write.
                        if let Err(err) = touch_running_prompt(&guard, &row.session_id) {
                            tracing::warn!(
                                error = %err,
                                session_id = %row.session_id,
                                "failed to re-touch running prompt on session update"
                            );
                        }
                        // `append_session_event_with_source` now fans the
                        // persisted event out to both the `logs` topic and
                        // `sessions.{id}` itself; no explicit republish here.
                        // Persist normalized standard ACP context usage and
                        // recognized legacy token usage; ignore unknown shapes.
                        if let Some(usage) =
                            extract_usage_payload(&row.session_id, &row.payload_json)
                            && let Ok(usage_text) = serde_json::to_string(&usage)
                            && let Err(err) = guard.append_session_event_with_source(
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
                        // Agent-side shell runs announced as execute-kind
                        // tool calls get a derived `tool.execute` event so
                        // shell activity is filterable even when the agent
                        // never uses client terminals.
                        if let Some(execute) =
                            extract_execute_tool_call(&row.session_id, &row.payload_json)
                            && let Ok(execute_text) = serde_json::to_string(&execute)
                            && let Err(err) = guard.append_session_event_with_source(
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
                    }
                    Err(err) => {
                        tracing::warn!(
                            error = %err,
                            session_id = %row.session_id,
                            "failed to persist ACP session update"
                        );
                    }
                }
            }
        });
        Self {
            target_id,
            state,
            session_changes,
            tx: TokioMutex::new(Some(tx)),
            writer: TokioMutex::new(Some(writer)),
        }
    }
}

impl SessionEventSink for StateStoreSessionSink {
    fn capture_session_update<'a>(
        &'a self,
        agent_session_id: &'a str,
        update: &'a SessionUpdate,
    ) -> futures::future::BoxFuture<'a, bool> {
        Box::pin(async move {
            // Only tool-call updates can carry diff content. Skipping the
            // session lookup for everything else keeps chunk-heavy streams
            // off the state-store lock; `append` still resolves and drops
            // unknown sessions itself.
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
            let guard = self.state.lock().await;
            match guard.get_session_by_target_agent_session_id(&self.target_id, agent_session_id) {
                Ok(Some(record)) => Some(record.id),
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

    fn session_cwd<'a>(
        &'a self,
        agent_session_id: &'a str,
    ) -> futures::future::BoxFuture<'a, Option<String>> {
        Box::pin(async move {
            let guard = self.state.lock().await;
            match guard.get_session_by_target_agent_session_id(&self.target_id, agent_session_id) {
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
                .send(SessionEventRow {
                    session_id,
                    kind: kind.to_owned(),
                    payload_json: payload_json.to_owned(),
                })
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

    fn flush<'a>(&'a self) -> futures::future::BoxFuture<'a, ()> {
        Box::pin(async move {
            {
                let mut guard = self.tx.lock().await;
                // Dropping the sender lets the writer task observe EOF and
                // drain its queue before exiting. Idempotent.
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
mod tests {
    #[test]
    fn extract_usage_payload_picks_up_top_level_usage_object() {
        let payload =
            r#"{"usage": {"input_tokens": 12, "output_tokens": 34, "context_window_max": 200000}}"#;
        let usage =
            super::extract_usage_payload("sess_x", payload).expect("usage should be extracted");
        assert_eq!(usage["input_tokens"].as_i64(), Some(12));
        assert_eq!(usage["output_tokens"].as_i64(), Some(34));
        assert_eq!(usage["context_window_max"].as_i64(), Some(200000));
        assert_eq!(usage["session_id"].as_str(), Some("sess_x"));
    }

    #[test]
    fn extract_usage_payload_walks_nested_paths() {
        let payload = r#"{"update": {"usage": {"input_tokens": 5}}}"#;
        let usage =
            super::extract_usage_payload("sess_y", payload).expect("usage should be extracted");
        assert_eq!(usage["input_tokens"].as_i64(), Some(5));
        // Output tokens absent — must NOT be serialized rather than written as 0.
        assert!(usage.get("output_tokens").is_none());
    }

    #[test]
    fn extract_usage_payload_normalizes_standard_usage_update() {
        let payload = r#"{"sessionId":"agent_sess","update":{"sessionUpdate":"usage_update","used":4096,"size":32768,"cost":{"amount":1.25,"currency":"USD"}}}"#;
        let usage =
            super::extract_usage_payload("sess_local", payload).expect("standard usage extracted");
        assert_eq!(usage["session_id"], "sess_local");
        assert_eq!(usage["context_window_used"], 4096);
        assert_eq!(usage["context_window_max"], 32768);
        assert_eq!(usage["cost_amount"], 1.25);
        assert_eq!(usage["cost_currency"], "USD");
        assert!(usage.get("input_tokens").is_none());
    }

    #[test]
    fn extract_usage_payload_returns_none_when_shape_unknown() {
        assert!(super::extract_usage_payload("sess_z", "{}").is_none());
        assert!(super::extract_usage_payload("sess_z", r#"{"update":{"foo":"bar"}}"#).is_none());
        assert!(super::extract_usage_payload("sess_z", "not-json").is_none());
    }

    #[test]
    fn extract_usage_payload_rejects_negative_numbers() {
        let payload = r#"{"usage": {"input_tokens": -5, "output_tokens": 3}}"#;
        let usage = super::extract_usage_payload("s", payload).expect("partial usage");
        // Negative tokens were dropped; output tokens preserved.
        assert!(usage.get("input_tokens").is_none());
        assert_eq!(usage["output_tokens"].as_i64(), Some(3));
    }

    #[test]
    fn extract_execute_tool_call_lifts_command_from_raw_input() {
        // Serialized shape of an ACP `tool_call` update from a built-in
        // shell tool (Claude Code / OpenCode bash convention).
        let payload = r#"{"sessionId":"sess_1","update":{"sessionUpdate":"tool_call","toolCallId":"call_1","title":"uname -a","kind":"execute","status":"in_progress","rawInput":{"command":"uname -a","description":"print kernel info"}}}"#;
        let event = super::extract_execute_tool_call("sess_local", payload)
            .expect("execute tool call extracted");
        assert_eq!(event["session_id"].as_str(), Some("sess_local"));
        assert_eq!(event["tool_call_id"].as_str(), Some("call_1"));
        assert_eq!(event["status"].as_str(), Some("in_progress"));
        assert_eq!(event["title"].as_str(), Some("uname -a"));
        assert_eq!(event["command"].as_str(), Some("uname -a"));
    }

    #[test]
    fn extract_execute_tool_call_accepts_updates_that_restate_kind() {
        let payload = r#"{"sessionId":"sess_1","update":{"sessionUpdate":"tool_call_update","toolCallId":"call_1","kind":"execute","status":"completed"}}"#;
        let event = super::extract_execute_tool_call("sess_local", payload)
            .expect("execute tool call update extracted");
        assert_eq!(event["status"].as_str(), Some("completed"));
        // No rawInput on this transition: command absent, not empty.
        assert!(event.get("command").is_none());
    }

    #[test]
    fn extract_execute_tool_call_ignores_other_updates() {
        // Non-execute tool kind.
        let read_call = r#"{"update":{"sessionUpdate":"tool_call","toolCallId":"call_2","kind":"read","status":"pending"}}"#;
        assert!(super::extract_execute_tool_call("s", read_call).is_none());
        // Update without a restated kind (ACP only requires kind on the
        // initial tool_call) must not fire.
        let bare_update = r#"{"update":{"sessionUpdate":"tool_call_update","toolCallId":"call_1","status":"completed"}}"#;
        assert!(super::extract_execute_tool_call("s", bare_update).is_none());
        // Non-tool-call updates and garbage.
        let chunk = r#"{"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}}}"#;
        assert!(super::extract_execute_tool_call("s", chunk).is_none());
        assert!(super::extract_execute_tool_call("s", "not-json").is_none());
    }

    use crate::state::{NewPromptRecord, NewSessionRecord, PromptStatus, StateStore};
    use rusqlite::params;

    #[test]
    fn touch_running_prompt_advances_updated_at_on_in_flight_row() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = tempdir.path().join("state.sqlite");
        let store = StateStore::open(&path).expect("state open");
        store.migrate().expect("migrate");
        store
            .insert_session(NewSessionRecord {
                id: "sess_touch".to_owned(),
                agent_id: "fake".to_owned(),
                cwd: "/tmp".to_owned(),
                title: None,
                metadata_json: "{}".to_owned(),
            })
            .expect("session inserted");
        store
            .insert_prompt(NewPromptRecord {
                id: "prm_touch".to_owned(),
                session_id: "sess_touch".to_owned(),
                prompt_json: "[]".to_owned(),
            })
            .expect("prompt inserted");
        store
            .update_prompt_status(
                "prm_touch",
                PromptStatus::Running,
                None,
                None,
                None,
                None,
                None,
            )
            .expect("prompt flipped to running");

        // Force `updated_at` into the past so the re-touch is visible
        // even at sub-second resolution. Without this the wall-clock
        // delta between insert and touch is too small for the string
        // comparison to be reliable.
        let aged = "2020-01-01T00:00:00.000000000Z";
        let connection =
            rusqlite::Connection::open(store.path()).expect("open sqlite for age override");
        connection
            .execute(
                "UPDATE prompts SET updated_at = ?1 WHERE id = ?2",
                params![aged, "prm_touch"],
            )
            .expect("force-set updated_at");
        drop(connection);

        super::touch_running_prompt(&store, "sess_touch").expect("re-touch should succeed");

        let prompt = store
            .get_prompt("prm_touch")
            .expect("prompt lookup")
            .expect("prompt exists");
        assert_ne!(
            prompt.updated_at, aged,
            "touch_running_prompt must advance updated_at"
        );
        assert_eq!(
            prompt.status, "running",
            "touch must preserve the running status"
        );
    }

    #[tokio::test]
    async fn writer_persists_derived_tool_execute_event() {
        use crate::runtime::agent::session_sink::{SessionEventSink, StateStoreSessionSink};
        use std::sync::Arc;
        use tokio::sync::Mutex as TokioMutex;

        let tempdir = tempfile::tempdir().expect("tempdir");
        let store = StateStore::open(tempdir.path().join("state.sqlite")).expect("state open");
        store.migrate().expect("migrate");
        store
            .insert_session_for_target(
                "target_a",
                "agent_sess_1".to_owned(),
                NewSessionRecord {
                    id: "sess_local".to_owned(),
                    agent_id: "target_a".to_owned(),
                    cwd: "/tmp".to_owned(),
                    title: None,
                    metadata_json: "{}".to_owned(),
                },
            )
            .expect("session inserted");
        let state = Arc::new(TokioMutex::new(store));

        let sink = StateStoreSessionSink::new("target_a".to_owned(), state.clone());
        let payload = r#"{"sessionId":"agent_sess_1","update":{"sessionUpdate":"tool_call","toolCallId":"call_1","title":"uname -a","kind":"execute","status":"in_progress","rawInput":{"command":"uname -a"}}}"#;
        sink.append("agent_sess_1", "session.update", payload).await;
        sink.flush().await;

        let guard = state.lock().await;
        let derived = guard
            .query_events(crate::state::LogFilter {
                limit: 10,
                kind: Some("tool.execute"),
                source: Some("acp"),
                ..Default::default()
            })
            .expect("query derived events");
        assert_eq!(derived.len(), 1, "expected one derived tool.execute event");
        assert!(derived[0].payload_json.contains("\"command\":\"uname -a\""));
        assert!(
            derived[0]
                .payload_json
                .contains("\"session_id\":\"sess_local\"")
        );
        // The verbatim session.update row is still written alongside.
        let verbatim = guard
            .query_events(crate::state::LogFilter {
                limit: 10,
                kind: Some("session.update"),
                source: Some("acp"),
                ..Default::default()
            })
            .expect("query verbatim events");
        assert_eq!(verbatim.len(), 1, "expected the verbatim session.update");
    }

    #[tokio::test]
    async fn writer_persists_normalized_standard_usage_event() {
        use crate::runtime::agent::session_sink::{SessionEventSink, StateStoreSessionSink};
        use std::sync::Arc;
        use tokio::sync::Mutex as TokioMutex;

        let tempdir = tempfile::tempdir().expect("tempdir");
        let store = StateStore::open(tempdir.path().join("state.sqlite")).expect("state open");
        store.migrate().expect("migrate");
        store
            .insert_session_for_target(
                "target_a",
                "agent_sess_1".to_owned(),
                NewSessionRecord {
                    id: "sess_local".to_owned(),
                    agent_id: "target_a".to_owned(),
                    cwd: "/tmp".to_owned(),
                    title: None,
                    metadata_json: "{}".to_owned(),
                },
            )
            .expect("session inserted");
        let state = Arc::new(TokioMutex::new(store));
        let sink = StateStoreSessionSink::new("target_a".to_owned(), state.clone());
        let payload = r#"{"sessionId":"agent_sess_1","update":{"sessionUpdate":"usage_update","used":2048,"size":8192,"cost":{"amount":0.75,"currency":"EUR"}}}"#;
        sink.append("agent_sess_1", "session.update", payload).await;
        sink.flush().await;

        let guard = state.lock().await;
        let events = guard
            .query_events(crate::state::LogFilter {
                limit: 10,
                kind: Some("usage.reported"),
                source: Some("acp"),
                ..Default::default()
            })
            .expect("query usage events");
        assert_eq!(events.len(), 1);
        let usage: serde_json::Value =
            serde_json::from_str(&events[0].payload_json).expect("usage JSON");
        assert_eq!(usage["context_window_used"], 2048);
        assert_eq!(usage["context_window_max"], 8192);
        assert_eq!(usage["cost_amount"], 0.75);
        assert_eq!(usage["cost_currency"], "EUR");
    }

    #[test]
    fn session_info_updates_patch_title_and_agent_metadata() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let store = StateStore::open(tempdir.path().join("state.sqlite")).expect("state open");
        store.migrate().expect("migrate");
        store
            .insert_session_for_target(
                "target_a",
                "agent_sess_1".to_owned(),
                NewSessionRecord {
                    id: "sess_local".to_owned(),
                    agent_id: "target_a".to_owned(),
                    cwd: "/tmp".to_owned(),
                    title: Some("original".to_owned()),
                    metadata_json: r#"{"preserved":true,"agent_meta":{"old":true}}"#.to_owned(),
                },
            )
            .expect("session inserted");
        let metadata_update = serde_json::json!({
            "sessionId": "agent_sess_1",
            "update": {
                "sessionUpdate": "session_info_update",
                "updatedAt": "2026-07-20T01:02:03Z",
                "_meta": {"origin": "agent"}
            }
        });
        super::project_session_info_update(&store, "sess_local", &metadata_update.to_string())
            .expect("metadata projection");
        {
            let session = store
                .get_session("sess_local")
                .expect("session lookup")
                .expect("session exists");
            assert_eq!(session.title.as_deref(), Some("original"));
            assert_ne!(session.updated_at, "2026-07-20T01:02:03Z");
            let metadata: serde_json::Value =
                serde_json::from_str(&session.metadata_json).expect("metadata JSON");
            assert_eq!(metadata["preserved"], true);
            assert_eq!(metadata["agent_updated_at"], "2026-07-20T01:02:03Z");
            assert_eq!(metadata["agent_meta"]["origin"], "agent");
        }

        let clear_update = serde_json::json!({
            "sessionId": "agent_sess_1",
            "update": {
                "sessionUpdate": "session_info_update",
                "title": null,
                "updatedAt": null
            }
        });
        super::project_session_info_update(&store, "sess_local", &clear_update.to_string())
            .expect("clear projection");
        {
            let session = store
                .get_session("sess_local")
                .expect("session lookup")
                .expect("session exists");
            assert_eq!(session.title, None);
            let metadata: serde_json::Value =
                serde_json::from_str(&session.metadata_json).expect("metadata JSON");
            assert!(metadata["agent_updated_at"].is_null());
            assert_eq!(metadata["agent_meta"]["origin"], "agent");
            assert_eq!(metadata["preserved"], true);
        }

        let title_update = serde_json::json!({
            "sessionId": "agent_sess_1",
            "update": {
                "sessionUpdate": "session_info_update",
                "title": "renamed"
            }
        });
        super::project_session_info_update(&store, "sess_local", &title_update.to_string())
            .expect("title projection");
        let session = store
            .get_session("sess_local")
            .expect("session lookup")
            .expect("session exists");
        assert_eq!(session.title.as_deref(), Some("renamed"));
        let metadata: serde_json::Value =
            serde_json::from_str(&session.metadata_json).expect("metadata JSON");
        assert!(metadata["agent_updated_at"].is_null());
        assert_eq!(metadata["agent_meta"]["origin"], "agent");
        assert_eq!(metadata["preserved"], true);
    }

    #[tokio::test]
    async fn writer_keeps_raw_session_info_when_projection_fails() {
        use crate::runtime::agent::session_sink::{SessionEventSink, StateStoreSessionSink};
        use std::sync::Arc;
        use tokio::sync::Mutex as TokioMutex;

        let tempdir = tempfile::tempdir().expect("tempdir");
        let store = StateStore::open(tempdir.path().join("state.sqlite")).expect("state open");
        store.migrate().expect("migrate");
        store
            .insert_session_for_target(
                "target_a",
                "agent_sess_1".to_owned(),
                NewSessionRecord {
                    id: "sess_local".to_owned(),
                    agent_id: "target_a".to_owned(),
                    cwd: "/tmp".to_owned(),
                    title: Some("original".to_owned()),
                    metadata_json: "[]".to_owned(),
                },
            )
            .expect("session inserted");
        let state = Arc::new(TokioMutex::new(store));
        let sink = StateStoreSessionSink::new("target_a".to_owned(), state.clone());
        let payload = r#"{"sessionId":"agent_sess_1","update":{"sessionUpdate":"session_info_update","title":"renamed"}}"#;

        sink.append("agent_sess_1", "session.update", payload).await;
        sink.flush().await;

        let guard = state.lock().await;
        let events = guard
            .query_events(crate::state::LogFilter {
                limit: 10,
                kind: Some("session.update"),
                source: Some("acp"),
                ..Default::default()
            })
            .expect("query raw events");
        assert_eq!(events.len(), 1);
        let session = guard
            .get_session("sess_local")
            .expect("session lookup")
            .expect("session exists");
        assert_eq!(session.title.as_deref(), Some("original"));
    }

    #[tokio::test]
    async fn session_cwd_resolves_local_session_record() {
        use crate::runtime::agent::session_sink::{SessionEventSink, StateStoreSessionSink};
        use std::sync::Arc;
        use tokio::sync::Mutex as TokioMutex;

        let tempdir = tempfile::tempdir().expect("tempdir");
        let store = StateStore::open(tempdir.path().join("state.sqlite")).expect("state open");
        store.migrate().expect("migrate");
        store
            .insert_session_for_target(
                "target_a",
                "agent_sess_1".to_owned(),
                NewSessionRecord {
                    id: "sess_local".to_owned(),
                    agent_id: "target_a".to_owned(),
                    cwd: "/tmp/session-sub".to_owned(),
                    title: None,
                    metadata_json: "{}".to_owned(),
                },
            )
            .expect("session inserted");
        let state = Arc::new(TokioMutex::new(store));
        let sink = StateStoreSessionSink::new("target_a".to_owned(), state);

        assert_eq!(
            sink.session_cwd("agent_sess_1").await,
            Some("/tmp/session-sub".to_owned())
        );
        assert_eq!(sink.session_cwd("agent_sess_unknown").await, None);
    }

    #[tokio::test]
    async fn change_capture_maps_same_agent_session_id_to_each_array_target() {
        use crate::runtime::agent::session_changes::SessionChangesHandle;
        use crate::runtime::agent::session_sink::{SessionEventSink, StateStoreSessionSink};
        use agent_client_protocol::schema::v1::{
            Diff, SessionUpdate, ToolCall, ToolCallContent, ToolKind,
        };
        use std::sync::Arc;
        use tokio::sync::Mutex as TokioMutex;

        let tempdir = tempfile::tempdir().expect("tempdir");
        let store = StateStore::open(tempdir.path().join("state.sqlite")).expect("state open");
        store.migrate().expect("migrate");
        for (target_id, local_id) in [("target_a", "sess_local_a"), ("target_b", "sess_local_b")] {
            store
                .insert_session_for_target(
                    target_id,
                    "shared_agent_session".to_owned(),
                    NewSessionRecord {
                        id: local_id.to_owned(),
                        agent_id: target_id.to_owned(),
                        cwd: "/tmp".to_owned(),
                        title: None,
                        metadata_json: "{}".to_owned(),
                    },
                )
                .expect("session inserted");
        }
        let state = Arc::new(TokioMutex::new(store));
        let changes = SessionChangesHandle::new();
        let sink_a = StateStoreSessionSink::with_session_changes(
            "target_a".to_owned(),
            state.clone(),
            changes.clone(),
        );
        let sink_b = StateStoreSessionSink::with_session_changes(
            "target_b".to_owned(),
            state,
            changes.clone(),
        );
        let update_for = |new_text: &str| {
            SessionUpdate::ToolCall(
                ToolCall::new("call", "edit file")
                    .kind(ToolKind::Edit)
                    .content(vec![ToolCallContent::Diff(
                        Diff::new("/workspace/file", new_text).old_text("before"),
                    )]),
            )
        };

        assert!(
            sink_a
                .capture_session_update("shared_agent_session", &update_for("target a"))
                .await
        );
        assert!(
            sink_b
                .capture_session_update("shared_agent_session", &update_for("target b"))
                .await
        );

        let snapshot_a =
            serde_json::to_value(changes.snapshot("sess_local_a").await).expect("snapshot a JSON");
        let snapshot_b =
            serde_json::to_value(changes.snapshot("sess_local_b").await).expect("snapshot b JSON");
        assert_eq!(
            snapshot_a["tool_calls"][0]["content"][0]["newText"],
            "target a"
        );
        assert_eq!(
            snapshot_b["tool_calls"][0]["content"][0]["newText"],
            "target b"
        );
    }

    #[tokio::test]
    async fn change_capture_accepts_non_tool_updates_without_a_session_lookup() {
        use crate::runtime::agent::session_sink::{SessionEventSink, StateStoreSessionSink};
        use agent_client_protocol::schema::v1::{
            ContentBlock, ContentChunk, SessionUpdate, TextContent,
        };
        use std::sync::Arc;
        use tokio::sync::Mutex as TokioMutex;

        let tempdir = tempfile::tempdir().expect("tempdir");
        let store = StateStore::open(tempdir.path().join("state.sqlite")).expect("state open");
        store.migrate().expect("migrate");
        let sink =
            StateStoreSessionSink::new("target".to_owned(), Arc::new(TokioMutex::new(store)));

        let chunk = SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
            TextContent::new("hello"),
        )));
        assert!(
            sink.capture_session_update("agent_sess_unknown", &chunk)
                .await
        );
    }

    #[test]
    fn touch_running_prompt_is_noop_when_no_in_flight_prompt() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = tempdir.path().join("state.sqlite");
        let store = StateStore::open(&path).expect("state open");
        store.migrate().expect("migrate");
        store
            .insert_session(NewSessionRecord {
                id: "sess_empty".to_owned(),
                agent_id: "fake".to_owned(),
                cwd: "/tmp".to_owned(),
                title: None,
                metadata_json: "{}".to_owned(),
            })
            .expect("session inserted");

        // No prompt rows — re-touch must succeed without an error so the
        // ACP session sink never blocks on a benign no-op.
        super::touch_running_prompt(&store, "sess_empty").expect("noop succeeds");
    }
}
