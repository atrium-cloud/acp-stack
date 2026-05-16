//! `events` table persistence and the source-label constants.
//!
//! The `events` table is the unified runtime event log: every durable runtime
//! event (api requests, ACP notifications, command output chunks, permission
//! lifecycle transitions, ...) lands here. Each row carries a `source` label
//! so log queries can scope by origin.

use crate::error::Result;
use rusqlite::{OptionalExtension, params};

use super::core::StateStore;
use super::ids::{current_timestamp, next_event_id};
use super::records::LogFilter;
use super::rows::{collect_events, push_event_predicates, validate_json_payload};

/// Stable event-source labels. The `events.source` column (migration 007) lets
/// log queries filter writes by their origin. Default for `append_event` is
/// `system`; explicit callers should choose the closest label.
pub const EVENT_SOURCE_SYSTEM: &str = "system";
pub const EVENT_SOURCE_API: &str = "api";
pub const EVENT_SOURCE_ACP: &str = "acp";
pub const EVENT_SOURCE_COMMAND: &str = "command";
pub const EVENT_SOURCE_PERMISSION: &str = "permission";
pub const EVENT_SOURCE_CLI: &str = "cli";
/// Reserved for the `acpctl` local-agent CLI (Phase 3 batch D); no in-tree
/// writers should emit with this source today.
pub const EVENT_SOURCE_LOCAL: &str = "local";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub id: String,
    pub created_at: String,
    pub level: String,
    pub kind: String,
    pub message: String,
    pub payload_json: String,
    /// Origin label (`system`, `api`, `acp`, `command`, `permission`, `cli`,
    /// `local`). Added in migration 007; pre-007 rows default to `system`.
    pub source: String,
}

pub(super) fn row_to_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<Event> {
    Ok(Event {
        id: row.get(0)?,
        created_at: row.get(1)?,
        level: row.get(2)?,
        kind: row.get(3)?,
        message: row.get(4)?,
        payload_json: row.get(5)?,
        source: row.get(6)?,
    })
}

impl StateStore {
    /// Append an unscoped runtime event with the default source `"system"`.
    /// Use `append_event_with_source` when the caller has a more specific
    /// origin (`api`, `acp`, `command`, etc.) — log queries can filter by it.
    pub fn append_event(
        &self,
        level: &str,
        kind: &str,
        message: &str,
        payload_json: &str,
    ) -> Result<Event> {
        self.append_event_with_source(level, kind, EVENT_SOURCE_SYSTEM, message, payload_json)
    }

    pub fn append_event_with_source(
        &self,
        level: &str,
        kind: &str,
        source: &str,
        message: &str,
        payload_json: &str,
    ) -> Result<Event> {
        validate_json_payload(self.connection(), payload_json)?;
        let event = Event {
            id: next_event_id(),
            created_at: current_timestamp(),
            level: level.to_owned(),
            kind: kind.to_owned(),
            message: message.to_owned(),
            payload_json: payload_json.to_owned(),
            source: source.to_owned(),
        };

        self.persist_with_outbox("events", &event.id, &event.created_at, |conn| {
            conn.execute(
                r#"
                INSERT INTO events (id, created_at, level, kind, message, payload_json, source)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                "#,
                params![
                    event.id,
                    event.created_at,
                    event.level,
                    event.kind,
                    event.message,
                    event.payload_json,
                    event.source,
                ],
            )?;
            Ok(())
        })?;

        if let Some(hub) = self.event_hub() {
            hub.publish_log_event(&event);
        }

        Ok(event)
    }

    /// Unified `events`-table query. The filter is built dynamically so each
    /// optional field translates to at most one WHERE clause. `after_id` uses
    /// a row-value comparison against `(created_at, id)` so two events that
    /// share a `created_at` still progress past the cursor in a single
    /// direction.
    pub fn query_events(&self, filter: LogFilter<'_>) -> Result<Vec<Event>> {
        let mut sql = String::from(
            "SELECT id, created_at, level, kind, message, payload_json, source FROM events WHERE 1=1",
        );
        let mut bindings: Vec<rusqlite::types::Value> = Vec::new();
        push_event_predicates(&mut sql, &mut bindings, &filter);
        sql.push_str(" ORDER BY created_at DESC, id DESC LIMIT ?");
        bindings.push(rusqlite::types::Value::Integer(i64::from(filter.limit)));
        let mut statement = self.connection().prepare(&sql)?;
        let rows =
            statement.query_map(rusqlite::params_from_iter(bindings.iter()), row_to_event)?;
        Ok(collect_events(rows)?)
    }

    /// Convenience wrapper that scopes a `LogFilter` to permission events
    /// (`kind LIKE 'permission.%'` / `'permissions.%'`). Used by
    /// `GET /v1/logs/permissions`.
    pub fn query_permission_events(&self, mut filter: LogFilter<'_>) -> Result<Vec<Event>> {
        let mut sql = String::from(
            "SELECT id, created_at, level, kind, message, payload_json, source FROM events \
             WHERE (kind LIKE 'permission.%' OR kind LIKE 'permissions.%')",
        );
        let mut bindings: Vec<rusqlite::types::Value> = Vec::new();
        // The caller may pass an explicit kind filter; honor it on top of the
        // built-in permission prefix.
        filter.kind_prefix = filter.kind_prefix.or(Some("permission."));
        // Don't double-apply the prefix below.
        let kind_prefix_was_added = filter.kind_prefix == Some("permission.");
        let filter_for_pushers = if kind_prefix_was_added {
            LogFilter {
                kind_prefix: None,
                ..filter
            }
        } else {
            filter
        };
        push_event_predicates(&mut sql, &mut bindings, &filter_for_pushers);
        sql.push_str(" ORDER BY created_at DESC, id DESC LIMIT ?");
        bindings.push(rusqlite::types::Value::Integer(i64::from(filter.limit)));
        let mut statement = self.connection().prepare(&sql)?;
        let rows =
            statement.query_map(rusqlite::params_from_iter(bindings.iter()), row_to_event)?;
        Ok(collect_events(rows)?)
    }

    /// Convenience wrapper that scopes a `LogFilter` to security events
    /// (`kind LIKE 'security.%'`). Used by `GET /v1/logs/security` alongside
    /// the dedicated `auth_failures` table.
    pub fn query_security_events(&self, filter: LogFilter<'_>) -> Result<Vec<Event>> {
        let mut sql = String::from(
            "SELECT id, created_at, level, kind, message, payload_json, source FROM events \
             WHERE kind LIKE 'security.%'",
        );
        let mut bindings: Vec<rusqlite::types::Value> = Vec::new();
        push_event_predicates(&mut sql, &mut bindings, &filter);
        sql.push_str(" ORDER BY created_at DESC, id DESC LIMIT ?");
        bindings.push(rusqlite::types::Value::Integer(i64::from(filter.limit)));
        let mut statement = self.connection().prepare(&sql)?;
        let rows =
            statement.query_map(rusqlite::params_from_iter(bindings.iter()), row_to_event)?;
        Ok(collect_events(rows)?)
    }

    pub fn latest_event_timestamp(&self) -> Result<Option<String>> {
        Ok(self
            .connection()
            .query_row(
                "SELECT created_at FROM events ORDER BY created_at DESC, id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?)
    }
}
