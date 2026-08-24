//! Cross-domain SQLite helpers: JSON payload validation and the shared
//! `events`-table query predicates.

use crate::error::{Result, StackError};
use rusqlite::{Connection, params};

use super::events::Event;
use super::records::{LogFilter, LogOrder};

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

/// Push the optional dimensions of a `LogFilter` onto a SELECT against the `events`
/// table. Callers seed `sql` with `... WHERE 1=1` and append `ORDER BY ... LIMIT ?`
/// themselves, since `limit` is deliberately not pushed here.
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
        // The caller passes a literal dotted prefix; the `%` wildcard is added here.
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
        sql.push_str(" AND json_extract(payload_json, '$.command_id') = ?");
        bindings.push(rusqlite::types::Value::Text(command_id.to_owned()));
    }
    if let Some(permission_id) = filter.permission_id {
        // Older rows carry only `id`, so the fallback stays scoped to permission-shaped
        // rows and unrelated payload ids cannot satisfy a permission lookup.
        sql.push_str(
            " AND (json_extract(payload_json, '$.permission_id') = ? \
             OR (json_extract(payload_json, '$.id') = ? \
                 AND (kind LIKE 'permission.%' OR kind LIKE 'permissions.%' OR source = 'permission')))",
        );
        bindings.push(rusqlite::types::Value::Text(permission_id.to_owned()));
        bindings.push(rusqlite::types::Value::Text(permission_id.to_owned()));
    }
    if let Some(category) = filter.security_category {
        let kinds = category.kinds();
        // Sized exactly to the closed kinds list so SQLite evaluates against the kind
        // index without a temp table.
        sql.push_str(" AND kind IN (");
        for (index, kind) in kinds.iter().enumerate() {
            if index > 0 {
                sql.push_str(", ");
            }
            sql.push('?');
            bindings.push(rusqlite::types::Value::Text((*kind).to_owned()));
        }
        sql.push(')');
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
        // The keyset comparison flips with sort direction; strict inequality excludes
        // the cursor row itself.
        match filter.order {
            LogOrder::Desc => sql.push_str(
                " AND (created_at, id) < (SELECT created_at, id FROM events WHERE id = ?)",
            ),
            LogOrder::Asc => sql.push_str(
                " AND (created_at, id) > (SELECT created_at, id FROM events WHERE id = ?)",
            ),
        }
        bindings.push(rusqlite::types::Value::Text(after.to_owned()));
    }
}
