//! `events` table persistence and the source-label constants. Every durable
//! runtime event lands here, tagged with a `source` so queries can scope by origin.

use crate::error::Result;
use rusqlite::{OptionalExtension, params};

use super::core::StateStore;
use super::ids::{current_timestamp, next_event_id};
use super::records::LogFilter;
use super::rows::{collect_events, push_event_predicates, validate_json_payload};

/// Stable event-source labels for the `events.source` column.
pub const EVENT_SOURCE_SYSTEM: &str = "system";
pub const EVENT_SOURCE_API: &str = "api";
pub const EVENT_SOURCE_ACP: &str = "acp";
pub const EVENT_SOURCE_COMMAND: &str = "command";
pub const EVENT_SOURCE_PERMISSION: &str = "permission";
pub const EVENT_SOURCE_CLI: &str = "cli";
/// Internal local Unix-socket calls use this source.
pub const EVENT_SOURCE_LOCAL: &str = "local";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub id: String,
    pub created_at: String,
    pub level: String,
    pub kind: String,
    pub message: String,
    pub payload_json: String,
    /// Origin label; pre-migration-007 rows default to `system`.
    pub source: String,
    /// Session scope; `None` for rows written through the unscoped append paths.
    pub session_id: Option<String>,
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
        session_id: row.get(7)?,
    })
}

impl StateStore {
    /// Append an unscoped runtime event with the default source `"system"`.
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
            session_id: None,
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

    /// Unified `events`-table query; `after_id` compares against `(created_at, id)`
    /// so events sharing a `created_at` still advance past the cursor.
    pub fn query_events(&self, filter: LogFilter<'_>) -> Result<Vec<Event>> {
        let mut sql = String::from(
            "SELECT id, created_at, level, kind, message, payload_json, source, session_id FROM events WHERE 1=1",
        );
        let mut bindings: Vec<rusqlite::types::Value> = Vec::new();
        push_event_predicates(&mut sql, &mut bindings, &filter);
        let direction = filter.order.sql_keyword();
        sql.push_str(&format!(
            " ORDER BY created_at {direction}, id {direction} LIMIT ?"
        ));
        bindings.push(rusqlite::types::Value::Integer(i64::from(filter.limit)));
        let mut statement = self.connection().prepare(&sql)?;
        let rows =
            statement.query_map(rusqlite::params_from_iter(bindings.iter()), row_to_event)?;
        Ok(collect_events(rows)?)
    }

    /// Scope a `LogFilter` to permission events.
    pub fn query_permission_events(&self, mut filter: LogFilter<'_>) -> Result<Vec<Event>> {
        let mut sql = String::from(
            "SELECT id, created_at, level, kind, message, payload_json, source, session_id FROM events \
             WHERE (kind LIKE 'permission.%' OR kind LIKE 'permissions.%')",
        );
        let mut bindings: Vec<rusqlite::types::Value> = Vec::new();
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
        let direction = filter.order.sql_keyword();
        sql.push_str(&format!(
            " ORDER BY created_at {direction}, id {direction} LIMIT ?"
        ));
        bindings.push(rusqlite::types::Value::Integer(i64::from(filter.limit)));
        let mut statement = self.connection().prepare(&sql)?;
        let rows =
            statement.query_map(rusqlite::params_from_iter(bindings.iter()), row_to_event)?;
        Ok(collect_events(rows)?)
    }

    /// Scope a `LogFilter` to security events.
    pub fn query_security_events(&self, filter: LogFilter<'_>) -> Result<Vec<Event>> {
        let mut sql = String::from(
            "SELECT id, created_at, level, kind, message, payload_json, source, session_id FROM events \
             WHERE kind LIKE 'security.%'",
        );
        let mut bindings: Vec<rusqlite::types::Value> = Vec::new();
        push_event_predicates(&mut sql, &mut bindings, &filter);
        let direction = filter.order.sql_keyword();
        sql.push_str(&format!(
            " ORDER BY created_at {direction}, id {direction} LIMIT ?"
        ));
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
