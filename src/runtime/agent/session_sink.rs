//! Sink abstraction for ACP `session/update` notifications and the
//! `StateStore`-backed implementation used by the daemon.

use std::str::FromStr;
use std::sync::Arc;

use agent_client_protocol::schema::{
    MaybeUndefined,
    v1::{AvailableCommandInput, SessionUpdate},
};
use tokio::sync::Mutex as TokioMutex;
use tokio::task::JoinHandle;

use crate::runtime::agent::session_changes::SessionChangesHandle;
use crate::state::{
    MAX_SESSION_AVAILABLE_COMMANDS, PromptStatus, SessionAvailableCommand, StateStore,
};

/// Sink for ACP `session/update` notifications. `append` must persist the
/// event before its future resolves, and `flush` must drain any background
/// writer, or a fast shutdown drops in-flight writes.
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

    fn flush<'a>(&'a self) -> futures::future::BoxFuture<'a, ()> {
        Box::pin(async {})
    }
}

/// `SessionEventSink` backed by the daemon's real `StateStore`. Writes flow
/// through a bounded channel into one background writer task; the bound is
/// what stops a runaway agent from exhausting daemon memory.
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
/// objects emitted by older agents.
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

/// Project an ACP `session_info_update`; must run only after the verbatim
/// `session.update` row is durable, since that row is the source of truth.
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

/// Project an ACP `available_commands_update`; latest-wins, and an empty list
/// still replaces.
fn project_available_commands_update(
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
        != Some("available_commands_update")
    {
        return Ok(());
    }
    let update = serde_json::from_value::<SessionUpdate>(update.clone()).map_err(|err| {
        crate::error::StackError::StateInvalidJson {
            field: "session.update.update",
            reason: err.to_string(),
        }
    })?;
    let SessionUpdate::AvailableCommandsUpdate(update) = update else {
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
        != Some("config_option_update")
    {
        return Ok(());
    }
    let update = serde_json::from_value::<SessionUpdate>(update.clone()).map_err(|err| {
        crate::error::StackError::StateInvalidJson {
            field: "session.update.update",
            reason: err.to_string(),
        }
    })?;
    let SessionUpdate::ConfigOptionUpdate(update) = update else {
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

/// Bump `updated_at` on the oldest in-flight prompt so the stale-prompt
/// sweeper does not flag an actively streaming one. ACP `session/update`
/// carries no `prompt_id`, so session-scoped is the best precision available.
fn touch_running_prompt(store: &StateStore, session_id: &str) -> crate::error::Result<()> {
    let prompts = store.in_flight_prompts_for_session(session_id)?;
    let Some(prompt) = prompts.into_iter().next() else {
        return Ok(());
    };
    // Re-passing the existing status advances `updated_at` while leaving
    // every other column intact.
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

/// Backpressure buffer for unwritten ACP session updates.
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
                        if let Err(err) = project_available_commands_update(
                            &guard,
                            &row.session_id,
                            &row.payload_json,
                        ) {
                            tracing::warn!(
                                error = %err,
                                session_id = %row.session_id,
                                "failed to apply ACP available commands update"
                            );
                        }
                        if let Err(err) = project_config_options_update(
                            &guard,
                            &row.session_id,
                            &row.payload_json,
                        ) {
                            tracing::warn!(
                                error = %err,
                                session_id = %row.session_id,
                                "failed to apply ACP config option update"
                            );
                        }
                        if let Err(err) = touch_running_prompt(&guard, &row.session_id) {
                            tracing::warn!(
                                error = %err,
                                session_id = %row.session_id,
                                "failed to re-touch running prompt on session update"
                            );
                        }
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
            // Only tool-call updates carry diff content; skipping the session
            // lookup otherwise keeps chunk-heavy streams off the store lock.
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
