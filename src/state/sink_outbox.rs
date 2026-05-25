//! Local delivery outbox for the Supabase logging sink.
//!
//! Every persistence call site that runs while external logging is enabled
//! enqueues an outbox row in the same transaction that writes the source row,
//! so a crash between the source INSERT and the enqueue cannot drop a row
//! from the delivery pipeline. The background worker selects pending rows
//! ordered by `(status, next_attempt_at, created_at)` and POSTs them to
//! PostgREST with `Prefer: resolution=merge-duplicates,return=minimal`,
//! making replay idempotent.
//!
//! Source rows can change (UPDATEs on `sessions`, `commands`, prompts, ...).
//! The outbox key is `"{source_table}:{source_id}"`, so an UPSERT flips a
//! previously-sent row back to `pending` and clears retry bookkeeping; the
//! worker re-uploads the latest row contents and PostgREST's
//! `merge-duplicates` collapses duplicates server-side.

use crate::error::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value};

use super::core::StateStore;

/// Most recent failure window summary. Fields: window-start RFC3339,
/// failure count, last error message (if any), last observed-at RFC3339.
pub type FailureSummary = (String, i64, Option<String>, String);

/// One row pulled from the outbox by the sink worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxRow {
    /// `"{source_table}:{source_id}"` — both the primary key and the
    /// upsert handle for replay.
    pub id: String,
    pub source_table: String,
    pub source_id: String,
    pub created_at: String,
    pub attempts: i64,
}

fn outbox_id(source_table: &str, source_id: &str) -> String {
    format!("{source_table}:{source_id}")
}

/// Enqueue (or re-flag) a source row for delivery. Idempotent under
/// concurrent retries: the ON CONFLICT branch resets retry bookkeeping so
/// the worker re-uploads on the next poll.
pub fn enqueue(
    conn: &Connection,
    source_table: &str,
    source_id: &str,
    created_at: &str,
) -> Result<()> {
    let id = outbox_id(source_table, source_id);
    conn.execute(
        r#"
        INSERT INTO sink_outbox
            (id, source_table, source_id, created_at, status, attempts,
             next_attempt_at, last_error, last_attempt_at)
        VALUES (?1, ?2, ?3, ?4, 'pending', 0, NULL, NULL, NULL)
        ON CONFLICT(id) DO UPDATE SET
            status          = 'pending',
            attempts        = 0,
            next_attempt_at = NULL,
            last_error      = NULL,
            last_attempt_at = NULL,
            created_at      = excluded.created_at
        "#,
        params![id, source_table, source_id, created_at],
    )?;
    Ok(())
}

/// Pull at most `limit` rows that are due for delivery. Rows in `sending` are
/// excluded so a crashed worker cannot starve them forever; the worker that
/// claims them must transition back to `pending` (via `mark_failure`) or
/// `sent` (via `mark_sent`) before exiting.
pub fn next_batch(conn: &Connection, limit: usize, now: &str) -> Result<Vec<OutboxRow>> {
    let mut statement = conn.prepare(
        r#"
        SELECT id, source_table, source_id, created_at, attempts
        FROM sink_outbox
        WHERE status = 'pending'
          AND (next_attempt_at IS NULL OR next_attempt_at <= ?1)
        ORDER BY created_at ASC, id ASC
        LIMIT ?2
        "#,
    )?;
    let rows = statement.query_map(params![now, limit as i64], |row| {
        Ok(OutboxRow {
            id: row.get(0)?,
            source_table: row.get(1)?,
            source_id: row.get(2)?,
            created_at: row.get(3)?,
            attempts: row.get(4)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Mark a batch of rows as delivered. Idempotent: missing ids are silently
/// skipped (a worker that retries a partially-acked batch should not crash).
pub fn mark_sent(conn: &Connection, ids: &[String], now: &str) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let placeholders = std::iter::repeat_n("?", ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        r#"
        UPDATE sink_outbox
        SET status = 'sent', last_attempt_at = ?, last_error = NULL
        WHERE id IN ({placeholders})
        "#,
    );
    let mut bindings: Vec<rusqlite::types::Value> =
        vec![rusqlite::types::Value::Text(now.to_owned())];
    for id in ids {
        bindings.push(rusqlite::types::Value::Text(id.clone()));
    }
    conn.execute(&sql, rusqlite::params_from_iter(bindings.iter()))?;
    Ok(())
}

/// Bump retry bookkeeping for a batch that failed to deliver. The new
/// `next_attempt_at` is computed by the caller (typically exponential
/// backoff with jitter).
pub fn mark_failure(
    conn: &Connection,
    ids: &[String],
    error: &str,
    next_attempt_at: &str,
    now: &str,
) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let placeholders = std::iter::repeat_n("?", ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        r#"
        UPDATE sink_outbox
        SET attempts = attempts + 1,
            last_error = ?,
            last_attempt_at = ?,
            next_attempt_at = ?
        WHERE id IN ({placeholders})
        "#,
    );
    let mut bindings: Vec<rusqlite::types::Value> = vec![
        rusqlite::types::Value::Text(error.to_owned()),
        rusqlite::types::Value::Text(now.to_owned()),
        rusqlite::types::Value::Text(next_attempt_at.to_owned()),
    ];
    for id in ids {
        bindings.push(rusqlite::types::Value::Text(id.clone()));
    }
    conn.execute(&sql, rusqlite::params_from_iter(bindings.iter()))?;
    Ok(())
}

/// Hard upper bound on `sink_failures_summary.last_error` length. Bounds
/// state-DB growth when a Supabase response body is large and bounds the
/// payload size of the `security check` JSON response that surfaces this
/// value.
const FAILURE_SUMMARY_ERROR_CAP: usize = 512;

/// Roll a failure window into `sink_failures_summary`. The worker calls this
/// on a recurring schedule so `security check` can surface the most recent
/// failure without scanning the full outbox. Truncates `error` to a fixed
/// cap so a noisy Supabase response body cannot bloat the table or echo
/// upstream-injected payload back through the API.
pub fn record_failure_window(
    conn: &Connection,
    window_started_at: &str,
    failure_count: i64,
    error: &str,
    now: &str,
) -> Result<()> {
    let bounded_error = if error.chars().count() <= FAILURE_SUMMARY_ERROR_CAP {
        error.to_owned()
    } else {
        let mut out: String = error.chars().take(FAILURE_SUMMARY_ERROR_CAP).collect();
        out.push_str("...");
        out
    };
    conn.execute(
        r#"
        INSERT INTO sink_failures_summary
            (window_started_at, failure_count, last_error, last_observed_at)
        VALUES (?1, ?2, ?3, ?4)
        ON CONFLICT(window_started_at) DO UPDATE SET
            failure_count    = excluded.failure_count,
            last_error       = excluded.last_error,
            last_observed_at = excluded.last_observed_at
        "#,
        params![window_started_at, failure_count, bounded_error, now],
    )?;
    Ok(())
}

/// Count rows that are currently in flight or queued for retry; >0 means the
/// sink has unfinished work and `security check` should surface it.
pub fn open_failure_count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row(
        r#"
        SELECT COUNT(*)
        FROM sink_outbox
        WHERE status = 'pending' AND attempts > 0
        "#,
        [],
        |row| row.get(0),
    )?)
}

/// Fetch the most recent failure window summary so `security check` can
/// report the latest `last_error` text. Returns `None` if no failures have
/// been observed yet.
pub fn latest_failure_summary(conn: &Connection) -> Result<Option<FailureSummary>> {
    Ok(conn
        .query_row(
            r#"
            SELECT window_started_at, failure_count, last_error, last_observed_at
            FROM sink_failures_summary
            ORDER BY last_observed_at DESC, window_started_at DESC
            LIMIT 1
            "#,
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()?)
}

/// Hydrate one source row into a JSON map ready for redaction + upload.
/// Returns `None` if the row no longer exists (e.g. a delete raced ahead of
/// the worker); the caller treats that as a synthetic success and `mark_sent`s
/// the outbox row.
fn hydrate_row(
    conn: &Connection,
    source_table: &str,
    source_id: &str,
) -> Result<Option<Map<String, Value>>> {
    match source_table {
        "events" => hydrate_events(conn, source_id),
        "sessions" => hydrate_sessions(conn, source_id),
        "prompts" => hydrate_prompts(conn, source_id),
        "commands" => hydrate_commands(conn, source_id),
        "permission_requests" => hydrate_permission_requests(conn, source_id),
        "permission_decisions" => hydrate_permission_decisions(conn, source_id),
        "auth_failures" => hydrate_auth_failures(conn, source_id),
        "agent_lifecycle" => hydrate_agent_lifecycle(conn, source_id),
        other => Err(crate::error::StackError::SupabaseSinkUnknownTable {
            table: other.to_owned(),
        }),
    }
}

fn json_object_or_empty(text: Option<String>) -> Value {
    match text.and_then(|s| serde_json::from_str::<Value>(&s).ok()) {
        Some(Value::Object(m)) => Value::Object(m),
        _ => Value::Object(Map::new()),
    }
}

fn hydrate_events(conn: &Connection, id: &str) -> Result<Option<Map<String, Value>>> {
    Ok(conn
        .query_row(
            r#"
            SELECT id, created_at, level, kind, message, payload_json, source, session_id
            FROM events WHERE id = ?1
            "#,
            params![id],
            |row| {
                let mut obj = Map::new();
                obj.insert("id".into(), Value::String(row.get::<_, String>(0)?));
                obj.insert("created_at".into(), Value::String(row.get::<_, String>(1)?));
                obj.insert("level".into(), Value::String(row.get::<_, String>(2)?));
                obj.insert("kind".into(), Value::String(row.get::<_, String>(3)?));
                obj.insert("message".into(), Value::String(row.get::<_, String>(4)?));
                obj.insert(
                    "payload_json".into(),
                    json_object_or_empty(Some(row.get::<_, String>(5)?)),
                );
                obj.insert("source".into(), Value::String(row.get::<_, String>(6)?));
                let session_id: Option<String> = row.get(7)?;
                obj.insert(
                    "session_id".into(),
                    session_id.map(Value::String).unwrap_or(Value::Null),
                );
                Ok(obj)
            },
        )
        .optional()?)
}

fn hydrate_sessions(conn: &Connection, id: &str) -> Result<Option<Map<String, Value>>> {
    Ok(conn
        .query_row(
            r#"
            SELECT id, created_at, updated_at, status, agent_id, cwd, title, metadata_json
            FROM sessions WHERE id = ?1
            "#,
            params![id],
            |row| {
                let mut obj = Map::new();
                obj.insert("id".into(), Value::String(row.get::<_, String>(0)?));
                obj.insert("created_at".into(), Value::String(row.get::<_, String>(1)?));
                obj.insert("updated_at".into(), Value::String(row.get::<_, String>(2)?));
                obj.insert("status".into(), Value::String(row.get::<_, String>(3)?));
                obj.insert("agent_id".into(), Value::String(row.get::<_, String>(4)?));
                obj.insert("cwd".into(), Value::String(row.get::<_, String>(5)?));
                let title: Option<String> = row.get(6)?;
                obj.insert(
                    "title".into(),
                    title.map(Value::String).unwrap_or(Value::Null),
                );
                obj.insert(
                    "metadata_json".into(),
                    json_object_or_empty(Some(row.get::<_, String>(7)?)),
                );
                Ok(obj)
            },
        )
        .optional()?)
}

fn hydrate_prompts(conn: &Connection, id: &str) -> Result<Option<Map<String, Value>>> {
    Ok(conn
        .query_row(
            r#"
            SELECT id, session_id, created_at, updated_at, status,
                   stop_reason, error_code, error_message, prompt_json
            FROM prompts WHERE id = ?1
            "#,
            params![id],
            |row| {
                let mut obj = Map::new();
                obj.insert("id".into(), Value::String(row.get::<_, String>(0)?));
                obj.insert("session_id".into(), Value::String(row.get::<_, String>(1)?));
                obj.insert("created_at".into(), Value::String(row.get::<_, String>(2)?));
                obj.insert("updated_at".into(), Value::String(row.get::<_, String>(3)?));
                obj.insert("status".into(), Value::String(row.get::<_, String>(4)?));
                for (idx, key) in [
                    (5usize, "stop_reason"),
                    (6, "error_code"),
                    (7, "error_message"),
                ] {
                    let value: Option<String> = row.get(idx)?;
                    obj.insert(key.into(), value.map(Value::String).unwrap_or(Value::Null));
                }
                // prompt_json is an ACP block array, not an object.
                // Preserve the raw JSON value so the redactor can stamp the
                // real byte length before replacing it with the redacted
                // sentinel; json_object_or_empty would coerce arrays to {}
                // and underreport size.
                let raw_prompt: String = row.get(8)?;
                let parsed_prompt: Value = serde_json::from_str(&raw_prompt).unwrap_or(Value::Null);
                obj.insert("prompt_json".into(), parsed_prompt);
                obj.insert(
                    "_prompt_json_bytes".into(),
                    Value::Number((raw_prompt.len() as u64).into()),
                );
                Ok(obj)
            },
        )
        .optional()?)
}

fn hydrate_commands(conn: &Connection, id: &str) -> Result<Option<Map<String, Value>>> {
    Ok(conn
        .query_row(
            r#"
            SELECT id, created_at, updated_at, status, command, exit_status,
                   started_at, finished_at, cwd, env_json, duration_ms, truncated
            FROM commands WHERE id = ?1
            "#,
            params![id],
            |row| {
                let mut obj = Map::new();
                obj.insert("id".into(), Value::String(row.get::<_, String>(0)?));
                obj.insert("created_at".into(), Value::String(row.get::<_, String>(1)?));
                obj.insert("updated_at".into(), Value::String(row.get::<_, String>(2)?));
                obj.insert("status".into(), Value::String(row.get::<_, String>(3)?));
                obj.insert("command".into(), Value::String(row.get::<_, String>(4)?));
                let exit_status: Option<i64> = row.get(5)?;
                obj.insert(
                    "exit_status".into(),
                    exit_status
                        .map(|v| Value::Number(v.into()))
                        .unwrap_or(Value::Null),
                );
                for (idx, key) in [(6usize, "started_at"), (7, "finished_at"), (8, "cwd")] {
                    let value: Option<String> = row.get(idx)?;
                    obj.insert(key.into(), value.map(Value::String).unwrap_or(Value::Null));
                }
                let env_text: Option<String> = row.get(9)?;
                obj.insert("env_json".into(), json_object_or_empty(env_text));
                let duration_ms: Option<i64> = row.get(10)?;
                obj.insert(
                    "duration_ms".into(),
                    duration_ms
                        .map(|v| Value::Number(v.into()))
                        .unwrap_or(Value::Null),
                );
                let truncated: i64 = row.get(11)?;
                obj.insert("truncated".into(), Value::Number(truncated.into()));
                Ok(obj)
            },
        )
        .optional()?)
}

fn hydrate_permission_requests(conn: &Connection, id: &str) -> Result<Option<Map<String, Value>>> {
    Ok(conn
        .query_row(
            r#"
            SELECT id, created_at, updated_at, status, source,
                   requester, subject_id, detail_json, expires_at
            FROM permission_requests WHERE id = ?1
            "#,
            params![id],
            |row| {
                let mut obj = Map::new();
                obj.insert("id".into(), Value::String(row.get::<_, String>(0)?));
                obj.insert("created_at".into(), Value::String(row.get::<_, String>(1)?));
                obj.insert("updated_at".into(), Value::String(row.get::<_, String>(2)?));
                obj.insert("status".into(), Value::String(row.get::<_, String>(3)?));
                obj.insert("source".into(), Value::String(row.get::<_, String>(4)?));
                for (idx, key) in [(5usize, "requester"), (6, "subject_id"), (8, "expires_at")] {
                    let value: Option<String> = row.get(idx)?;
                    obj.insert(key.into(), value.map(Value::String).unwrap_or(Value::Null));
                }
                obj.insert(
                    "detail_json".into(),
                    json_object_or_empty(Some(row.get::<_, String>(7)?)),
                );
                Ok(obj)
            },
        )
        .optional()?)
}

fn hydrate_permission_decisions(conn: &Connection, id: &str) -> Result<Option<Map<String, Value>>> {
    Ok(conn
        .query_row(
            r#"
            SELECT id, request_id, created_at, decision, deciding_principal, reason
            FROM permission_decisions WHERE id = ?1
            "#,
            params![id],
            |row| {
                let mut obj = Map::new();
                obj.insert("id".into(), Value::String(row.get::<_, String>(0)?));
                obj.insert("request_id".into(), Value::String(row.get::<_, String>(1)?));
                obj.insert("created_at".into(), Value::String(row.get::<_, String>(2)?));
                obj.insert("decision".into(), Value::String(row.get::<_, String>(3)?));
                for (idx, key) in [(4usize, "deciding_principal"), (5, "reason")] {
                    let value: Option<String> = row.get(idx)?;
                    obj.insert(key.into(), value.map(Value::String).unwrap_or(Value::Null));
                }
                Ok(obj)
            },
        )
        .optional()?)
}

fn hydrate_auth_failures(conn: &Connection, id: &str) -> Result<Option<Map<String, Value>>> {
    Ok(conn
        .query_row(
            r#"
            SELECT id, created_at, key_kind, reason, client_ip, route, payload_json
            FROM auth_failures WHERE id = ?1
            "#,
            params![id],
            |row| {
                let mut obj = Map::new();
                obj.insert("id".into(), Value::String(row.get::<_, String>(0)?));
                obj.insert("created_at".into(), Value::String(row.get::<_, String>(1)?));
                obj.insert("key_kind".into(), Value::String(row.get::<_, String>(2)?));
                obj.insert("reason".into(), Value::String(row.get::<_, String>(3)?));
                for (idx, key) in [(4usize, "client_ip"), (5, "route")] {
                    let value: Option<String> = row.get(idx)?;
                    obj.insert(key.into(), value.map(Value::String).unwrap_or(Value::Null));
                }
                obj.insert(
                    "payload_json".into(),
                    json_object_or_empty(Some(row.get::<_, String>(6)?)),
                );
                Ok(obj)
            },
        )
        .optional()?)
}

fn hydrate_agent_lifecycle(conn: &Connection, id: &str) -> Result<Option<Map<String, Value>>> {
    Ok(conn
        .query_row(
            r#"
            SELECT id, created_at, event_kind, message, payload_json
            FROM agent_lifecycle WHERE id = ?1
            "#,
            params![id],
            |row| {
                let mut obj = Map::new();
                obj.insert("id".into(), Value::String(row.get::<_, String>(0)?));
                obj.insert("created_at".into(), Value::String(row.get::<_, String>(1)?));
                obj.insert("event_kind".into(), Value::String(row.get::<_, String>(2)?));
                obj.insert("message".into(), Value::String(row.get::<_, String>(3)?));
                obj.insert(
                    "payload_json".into(),
                    json_object_or_empty(Some(row.get::<_, String>(4)?)),
                );
                Ok(obj)
            },
        )
        .optional()?)
}

/// Public-facing surface for the Supabase sink worker. Keeps `Connection`
/// out of `runtime::logging::supabase_sink` so the worker depends only on the typed
/// state API.
impl StateStore {
    pub fn next_sink_outbox_batch(&self, limit: usize, now: &str) -> Result<Vec<OutboxRow>> {
        next_batch(self.connection(), limit, now)
    }

    pub fn hydrate_sink_outbox_row(
        &self,
        source_table: &str,
        source_id: &str,
    ) -> Result<Option<Map<String, Value>>> {
        hydrate_row(self.connection(), source_table, source_id)
    }

    pub fn mark_sink_outbox_sent(&self, ids: &[String], now: &str) -> Result<()> {
        mark_sent(self.connection(), ids, now)
    }

    pub fn mark_sink_outbox_failure(
        &self,
        ids: &[String],
        error: &str,
        next_attempt_at: &str,
        now: &str,
    ) -> Result<()> {
        mark_failure(self.connection(), ids, error, next_attempt_at, now)
    }

    pub fn record_sink_failure_window(
        &self,
        window_started_at: &str,
        failure_count: i64,
        error: &str,
        now: &str,
    ) -> Result<()> {
        record_failure_window(
            self.connection(),
            window_started_at,
            failure_count,
            error,
            now,
        )
    }

    pub fn sink_open_failure_count(&self) -> Result<i64> {
        open_failure_count(self.connection())
    }

    pub fn latest_sink_failure_summary(&self) -> Result<Option<FailureSummary>> {
        latest_failure_summary(self.connection())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::StateStore;
    use tempfile::tempdir;

    fn fresh_store() -> (tempfile::TempDir, StateStore) {
        let dir = tempdir().expect("tempdir");
        let store = StateStore::open(dir.path().join("state.sqlite")).expect("open store");
        store.migrate().expect("migrate");
        (dir, store)
    }

    #[test]
    fn enqueue_then_next_batch_returns_pending_rows() {
        let (_dir, store) = fresh_store();
        enqueue(
            store.connection(),
            "events",
            "evt_1",
            "2026-01-01T00:00:00Z",
        )
        .expect("enqueue");
        enqueue(
            store.connection(),
            "events",
            "evt_2",
            "2026-01-01T00:00:01Z",
        )
        .expect("enqueue");
        let batch = next_batch(store.connection(), 10, "2099-01-01T00:00:00Z").expect("next");
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0].id, "events:evt_1");
        assert_eq!(batch[1].id, "events:evt_2");
        assert_eq!(batch[0].attempts, 0);
    }

    #[test]
    fn enqueue_upserts_resets_retry_bookkeeping() {
        let (_dir, store) = fresh_store();
        enqueue(
            store.connection(),
            "sessions",
            "sess_1",
            "2026-01-01T00:00:00Z",
        )
        .expect("enqueue");
        mark_failure(
            store.connection(),
            &["sessions:sess_1".to_owned()],
            "boom",
            "2099-01-01T00:00:00Z",
            "2026-01-01T00:00:01Z",
        )
        .expect("mark_failure");
        // Re-enqueue (e.g. the session row was updated) should clear retry state.
        enqueue(
            store.connection(),
            "sessions",
            "sess_1",
            "2026-01-01T00:00:02Z",
        )
        .expect("re-enqueue");
        let batch = next_batch(store.connection(), 10, "2099-01-01T00:00:00Z").expect("next_batch");
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].attempts, 0);
        assert_eq!(batch[0].created_at, "2026-01-01T00:00:02Z");
    }

    #[test]
    fn next_batch_orders_by_created_at_then_id() {
        let (_dir, store) = fresh_store();
        enqueue(store.connection(), "events", "b", "2026-01-01T00:00:02Z").unwrap();
        enqueue(store.connection(), "events", "a", "2026-01-01T00:00:01Z").unwrap();
        enqueue(store.connection(), "events", "c", "2026-01-01T00:00:01Z").unwrap();
        let batch = next_batch(store.connection(), 10, "2099-01-01T00:00:00Z").unwrap();
        assert_eq!(batch[0].source_id, "a");
        assert_eq!(batch[1].source_id, "c");
        assert_eq!(batch[2].source_id, "b");
    }

    #[test]
    fn mark_sent_then_open_failure_count_is_zero() {
        let (_dir, store) = fresh_store();
        enqueue(
            store.connection(),
            "events",
            "evt_1",
            "2026-01-01T00:00:00Z",
        )
        .unwrap();
        mark_sent(
            store.connection(),
            &["events:evt_1".to_owned()],
            "2026-01-01T00:00:01Z",
        )
        .unwrap();
        assert_eq!(open_failure_count(store.connection()).unwrap(), 0);
        let batch = next_batch(store.connection(), 10, "2099-01-01T00:00:00Z").unwrap();
        assert!(batch.is_empty(), "sent rows must not appear in next_batch");
    }

    #[test]
    fn mark_failure_increments_attempts_and_gates_next_attempt() {
        let (_dir, store) = fresh_store();
        enqueue(
            store.connection(),
            "events",
            "evt_1",
            "2026-01-01T00:00:00Z",
        )
        .unwrap();
        mark_failure(
            store.connection(),
            &["events:evt_1".to_owned()],
            "5xx upstream",
            "2099-01-01T00:00:00Z",
            "2026-01-01T00:00:01Z",
        )
        .unwrap();
        assert_eq!(open_failure_count(store.connection()).unwrap(), 1);
        let batch_now = next_batch(store.connection(), 10, "2026-01-01T00:00:02Z").unwrap();
        assert!(batch_now.is_empty(), "row must be gated by next_attempt_at");
        let batch_later = next_batch(store.connection(), 10, "2099-01-02T00:00:00Z").unwrap();
        assert_eq!(batch_later.len(), 1);
        assert_eq!(batch_later[0].attempts, 1);
    }

    #[test]
    fn failure_window_summary_round_trips() {
        let (_dir, store) = fresh_store();
        record_failure_window(
            store.connection(),
            "2026-01-01T00:00:00Z",
            7,
            "5xx upstream",
            "2026-01-01T00:01:00Z",
        )
        .unwrap();
        let latest = latest_failure_summary(store.connection())
            .unwrap()
            .expect("summary present");
        assert_eq!(latest.0, "2026-01-01T00:00:00Z");
        assert_eq!(latest.1, 7);
        assert_eq!(latest.2.as_deref(), Some("5xx upstream"));
        assert_eq!(latest.3, "2026-01-01T00:01:00Z");
    }

    #[test]
    fn failure_window_summary_caps_large_error_text() {
        let (_dir, store) = fresh_store();
        let large_error = format!("{}secret-tail", "x".repeat(FAILURE_SUMMARY_ERROR_CAP + 50));
        record_failure_window(
            store.connection(),
            "2026-01-01T00:00:00Z",
            7,
            &large_error,
            "2026-01-01T00:01:00Z",
        )
        .unwrap();
        let latest = latest_failure_summary(store.connection())
            .unwrap()
            .expect("summary present");
        let stored = latest.2.expect("last_error present");
        assert!(
            stored.chars().count() <= FAILURE_SUMMARY_ERROR_CAP + 3,
            "stored error was not capped: {} chars",
            stored.chars().count()
        );
        assert!(stored.ends_with("..."));
        assert!(
            !stored.contains("secret-tail"),
            "oversized error tail leaked into summary"
        );
    }
}
