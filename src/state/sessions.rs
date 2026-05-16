//! Sessions, prompts, and session-scoped event persistence.

use crate::error::{Result, StackError};
use rusqlite::{OptionalExtension, params};

use super::core::StateStore;
use super::events::{EVENT_SOURCE_SYSTEM, Event, row_to_event};
use super::ids::{current_timestamp, next_event_id};
use super::records::SessionFilter;
use super::rows::validate_json_payload;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    pub id: String,
    pub created_at: String,
    pub updated_at: String,
    pub status: String,
    pub agent_id: String,
    pub cwd: String,
    pub title: Option<String>,
    pub metadata_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSessionRecord {
    pub id: String,
    pub agent_id: String,
    pub cwd: String,
    pub title: Option<String>,
    pub metadata_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptRecord {
    pub id: String,
    pub session_id: String,
    pub created_at: String,
    pub updated_at: String,
    pub status: String,
    pub stop_reason: Option<String>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub prompt_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPromptRecord {
    pub id: String,
    pub session_id: String,
    pub prompt_json: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptStatus {
    Pending,
    Running,
    Completed,
    Errored,
    Cancelled,
}

impl PromptStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            PromptStatus::Pending => "pending",
            PromptStatus::Running => "running",
            PromptStatus::Completed => "completed",
            PromptStatus::Errored => "errored",
            PromptStatus::Cancelled => "cancelled",
        }
    }
}

pub(super) fn row_to_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRecord> {
    Ok(SessionRecord {
        id: row.get(0)?,
        created_at: row.get(1)?,
        updated_at: row.get(2)?,
        status: row.get(3)?,
        agent_id: row.get(4)?,
        cwd: row.get(5)?,
        title: row.get(6)?,
        metadata_json: row.get(7)?,
    })
}

pub(super) fn row_to_prompt(row: &rusqlite::Row<'_>) -> rusqlite::Result<PromptRecord> {
    Ok(PromptRecord {
        id: row.get(0)?,
        session_id: row.get(1)?,
        created_at: row.get(2)?,
        updated_at: row.get(3)?,
        status: row.get(4)?,
        stop_reason: row.get(5)?,
        error_code: row.get(6)?,
        error_message: row.get(7)?,
        prompt_json: row.get(8)?,
    })
}

impl StateStore {
    pub fn query_sessions(&self, filter: SessionFilter<'_>) -> Result<Vec<SessionRecord>> {
        let mut sql = String::from(
            "SELECT id, created_at, updated_at, status, agent_id, cwd, title, metadata_json \
             FROM sessions WHERE 1=1",
        );
        let mut bindings: Vec<rusqlite::types::Value> = Vec::new();
        if let Some(since) = filter.since {
            sql.push_str(" AND updated_at >= ?");
            bindings.push(rusqlite::types::Value::Text(since.to_owned()));
        }
        if let Some(until) = filter.until {
            sql.push_str(" AND updated_at < ?");
            bindings.push(rusqlite::types::Value::Text(until.to_owned()));
        }
        if let Some(status) = filter.status {
            sql.push_str(" AND status = ?");
            bindings.push(rusqlite::types::Value::Text(status.to_owned()));
        }
        if let Some(after) = filter.after_id {
            sql.push_str(
                " AND (updated_at, id) < (SELECT updated_at, id FROM sessions WHERE id = ?)",
            );
            bindings.push(rusqlite::types::Value::Text(after.to_owned()));
        }
        sql.push_str(" ORDER BY updated_at DESC, id DESC LIMIT ?");
        bindings.push(rusqlite::types::Value::Integer(i64::from(filter.limit)));
        let mut statement = self.connection().prepare(&sql)?;
        let rows =
            statement.query_map(rusqlite::params_from_iter(bindings.iter()), row_to_session)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn get_session(&self, id: &str) -> Result<Option<SessionRecord>> {
        Ok(self
            .connection()
            .query_row(
                r#"
                SELECT id, created_at, updated_at, status, agent_id, cwd, title, metadata_json
                FROM sessions
                WHERE id = ?1
                "#,
                params![id],
                row_to_session,
            )
            .optional()?)
    }

    pub fn insert_session(&self, record: NewSessionRecord) -> Result<SessionRecord> {
        validate_json_payload(self.connection(), &record.metadata_json)?;
        let now = current_timestamp();
        let row = SessionRecord {
            id: record.id,
            created_at: now.clone(),
            updated_at: now,
            status: "active".to_owned(),
            agent_id: record.agent_id,
            cwd: record.cwd,
            title: record.title,
            metadata_json: record.metadata_json,
        };
        self.connection().execute(
            r#"
            INSERT INTO sessions
                (id, created_at, updated_at, status, agent_id, cwd, title, metadata_json)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            "#,
            params![
                row.id,
                row.created_at,
                row.updated_at,
                row.status,
                row.agent_id,
                row.cwd,
                row.title,
                row.metadata_json,
            ],
        )?;
        Ok(row)
    }

    pub fn update_session_status(&self, id: &str, status: &str) -> Result<()> {
        let now = current_timestamp();
        let affected = self.connection().execute(
            r#"
            UPDATE sessions
            SET status = ?1, updated_at = ?2
            WHERE id = ?3
            "#,
            params![status, now, id],
        )?;
        if affected == 0 {
            return Err(StackError::SessionNotFound { id: id.to_owned() });
        }
        Ok(())
    }

    /// Append an event scoped to a session. Used by the ACP bridge to persist
    /// `session/update` notifications. `kind` is the dotted event kind (e.g.
    /// `session.update`); `payload_json` is the verbatim notification body.
    /// Wrapper around `append_session_event_with_source` that records the
    /// default `system` source for callers that have no better label.
    pub fn append_session_event(
        &self,
        session_id: &str,
        level: &str,
        kind: &str,
        message: &str,
        payload_json: &str,
    ) -> Result<Event> {
        self.append_session_event_with_source(
            session_id,
            level,
            kind,
            EVENT_SOURCE_SYSTEM,
            message,
            payload_json,
        )
    }

    pub fn append_session_event_with_source(
        &self,
        session_id: &str,
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

        self.connection().execute(
            r#"
            INSERT INTO events (id, created_at, level, kind, message, payload_json, source, session_id)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            "#,
            params![
                event.id,
                event.created_at,
                event.level,
                event.kind,
                event.message,
                event.payload_json,
                event.source,
                session_id,
            ],
        )?;

        if let Some(hub) = self.event_hub() {
            hub.publish_log_event(&event);
        }

        Ok(event)
    }

    pub fn query_session_events(
        &self,
        session_id: &str,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<Event>> {
        let limit = i64::from(limit);
        match after {
            Some(after_id) => {
                // Stable ordering pairs `(created_at, id)` so two events sharing
                // a created_at still progress past the cursor. Compare on the
                // tuple instead of just id so a slow inserter cannot reorder
                // pagination across a clock tick.
                let mut statement = self.connection().prepare(
                    r#"
                    SELECT e.id, e.created_at, e.level, e.kind, e.message, e.payload_json, e.source
                    FROM events e
                    JOIN events cursor ON cursor.id = ?2
                    WHERE e.session_id = ?1
                      AND (e.created_at, e.id) > (cursor.created_at, cursor.id)
                    ORDER BY e.created_at ASC, e.id ASC
                    LIMIT ?3
                    "#,
                )?;
                let rows =
                    statement.query_map(params![session_id, after_id, limit], row_to_event)?;
                Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
            }
            None => {
                let mut statement = self.connection().prepare(
                    r#"
                    SELECT id, created_at, level, kind, message, payload_json, source
                    FROM events
                    WHERE session_id = ?1
                    ORDER BY created_at ASC, id ASC
                    LIMIT ?2
                    "#,
                )?;
                let rows = statement.query_map(params![session_id, limit], row_to_event)?;
                Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
            }
        }
    }

    pub fn insert_prompt(&self, record: NewPromptRecord) -> Result<PromptRecord> {
        validate_json_payload(self.connection(), &record.prompt_json)?;
        let now = current_timestamp();
        let row = PromptRecord {
            id: record.id,
            session_id: record.session_id,
            created_at: now.clone(),
            updated_at: now,
            status: PromptStatus::Pending.as_str().to_owned(),
            stop_reason: None,
            error_code: None,
            error_message: None,
            prompt_json: record.prompt_json,
        };
        self.connection().execute(
            r#"
            INSERT INTO prompts
                (id, session_id, created_at, updated_at, status, prompt_json)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            "#,
            params![
                row.id,
                row.session_id,
                row.created_at,
                row.updated_at,
                row.status,
                row.prompt_json,
            ],
        )?;
        Ok(row)
    }

    pub fn get_prompt(&self, id: &str) -> Result<Option<PromptRecord>> {
        Ok(self
            .connection()
            .query_row(
                r#"
                SELECT id, session_id, created_at, updated_at, status,
                       stop_reason, error_code, error_message, prompt_json
                FROM prompts
                WHERE id = ?1
                "#,
                params![id],
                row_to_prompt,
            )
            .optional()?)
    }

    pub fn update_prompt_status(
        &self,
        id: &str,
        status: PromptStatus,
        stop_reason: Option<&str>,
        error_code: Option<&str>,
        error_message: Option<&str>,
    ) -> Result<()> {
        let now = current_timestamp();
        let affected = self.connection().execute(
            r#"
            UPDATE prompts
            SET status = ?1,
                updated_at = ?2,
                stop_reason = ?3,
                error_code = ?4,
                error_message = ?5
            WHERE id = ?6
            "#,
            params![
                status.as_str(),
                now,
                stop_reason,
                error_code,
                error_message,
                id
            ],
        )?;
        if affected == 0 {
            return Err(StackError::PromptNotFound { id: id.to_owned() });
        }
        Ok(())
    }

    /// Mark every `pending`/`running` prompt row as `errored` with the given
    /// reason. Called on daemon startup so prompts orphaned by a crash get a
    /// terminal status — otherwise clients polling those prompts would never
    /// see them settle. Returns the number of rows transitioned.
    pub fn reconcile_orphaned_prompts(&self, reason: &str) -> Result<usize> {
        let now = current_timestamp();
        let affected = self.connection().execute(
            r#"
            UPDATE prompts
            SET status = 'errored',
                updated_at = ?1,
                error_code = 'agent.daemon_restart',
                error_message = ?2
            WHERE status IN ('pending', 'running')
            "#,
            params![now, reason],
        )?;
        Ok(affected)
    }

    pub fn in_flight_prompts_for_session(&self, session_id: &str) -> Result<Vec<PromptRecord>> {
        let mut statement = self.connection().prepare(
            r#"
            SELECT id, session_id, created_at, updated_at, status,
                   stop_reason, error_code, error_message, prompt_json
            FROM prompts
            WHERE session_id = ?1 AND status IN ('pending', 'running')
            ORDER BY created_at ASC, id ASC
            "#,
        )?;
        let rows = statement.query_map(params![session_id], row_to_prompt)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}
