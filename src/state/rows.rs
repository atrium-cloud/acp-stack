//! Shared SQLite helpers used across the domain leaves.
//!
//! The `row_to_*` mappers live with the domain types they produce (e.g.
//! `row_to_session` is in `sessions.rs` next to `SessionRecord`). What stays
//! here are the cross-domain primitives: JSON payload validation against
//! SQLite's `json_valid`, and `push_event_predicates` / `collect_events` for
//! the unified `events`-table query path used by `events::query_events` and
//! its prefix-scoped variants.

use crate::error::{Result, StackError};
use rusqlite::{Connection, params};

use super::events::Event;
use super::records::LogFilter;

pub(super) fn validate_json_payload(connection: &Connection, payload_json: &str) -> Result<()> {
    let is_valid: i64 =
        connection.query_row("SELECT json_valid(?1)", params![payload_json], |row| {
            row.get(0)
        })?;
    if is_valid == 1 {
        return Ok(());
    }

    Err(StackError::InvalidEventPayload)
}

pub(super) fn collect_events(
    rows: impl Iterator<Item = rusqlite::Result<Event>>,
) -> rusqlite::Result<Vec<Event>> {
    rows.collect()
}

/// Push the optional dimensions of a `LogFilter` onto a SELECT against the
/// `events` table. Callers seed `sql` with `SELECT ... FROM events WHERE 1=1`
/// (plus any baseline kind scope) and an empty `bindings` Vec, then append
/// `ORDER BY ... LIMIT ?` themselves. `limit` is *not* pushed here so callers
/// can choose ASC / DESC ordering and add their own trailing predicates.
pub(super) fn push_event_predicates(
    sql: &mut String,
    bindings: &mut Vec<rusqlite::types::Value>,
    filter: &LogFilter<'_>,
) {
    if let Some(level) = filter.level {
        sql.push_str(" AND level = ?");
        bindings.push(rusqlite::types::Value::Text(level.to_owned()));
    }
    if let Some(kind) = filter.kind {
        sql.push_str(" AND kind = ?");
        bindings.push(rusqlite::types::Value::Text(kind.to_owned()));
    }
    if let Some(prefix) = filter.kind_prefix {
        // SQLite `LIKE` treats `%` as wildcard; the caller passes a literal
        // dotted prefix (e.g. `permission.`) and we add the wildcard here.
        sql.push_str(" AND kind LIKE ?");
        bindings.push(rusqlite::types::Value::Text(format!("{prefix}%")));
    }
    if let Some(source) = filter.source {
        sql.push_str(" AND source = ?");
        bindings.push(rusqlite::types::Value::Text(source.to_owned()));
    }
    if let Some(session_id) = filter.session_id {
        sql.push_str(" AND session_id = ?");
        bindings.push(rusqlite::types::Value::Text(session_id.to_owned()));
    }
    if let Some(command_id) = filter.command_id {
        // Command-lifecycle events embed the command id in their payload JSON.
        // `json_extract` is null-safe and short-circuits when the path is
        // absent, so non-command events drop out without raising.
        sql.push_str(" AND json_extract(payload_json, '$.command_id') = ?");
        bindings.push(rusqlite::types::Value::Text(command_id.to_owned()));
    }
    if let Some(permission_id) = filter.permission_id {
        // New permission events write `permission_id`; older/legacy rows and
        // previously emitted timeout events may only carry `id`. Keep that
        // fallback scoped to permission-shaped rows so unrelated payload ids
        // don't satisfy a permission lookup.
        sql.push_str(
            " AND (json_extract(payload_json, '$.permission_id') = ? \
             OR (json_extract(payload_json, '$.id') = ? \
                 AND (kind LIKE 'permission.%' OR kind LIKE 'permissions.%' OR source = 'permission')))",
        );
        bindings.push(rusqlite::types::Value::Text(permission_id.to_owned()));
        bindings.push(rusqlite::types::Value::Text(permission_id.to_owned()));
    }
    if let Some(since) = filter.since {
        sql.push_str(" AND created_at >= ?");
        bindings.push(rusqlite::types::Value::Text(since.to_owned()));
    }
    if let Some(until) = filter.until {
        sql.push_str(" AND created_at < ?");
        bindings.push(rusqlite::types::Value::Text(until.to_owned()));
    }
    if let Some(after) = filter.after_id {
        sql.push_str(" AND (created_at, id) < (SELECT created_at, id FROM events WHERE id = ?)");
        bindings.push(rusqlite::types::Value::Text(after.to_owned()));
    }
}
