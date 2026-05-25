//! Sessions, prompts, and session-scoped event persistence.

use crate::error::{Result, StackError};
use chrono::{SecondsFormat, Utc};
use rusqlite::{OptionalExtension, params};

use super::core::StateStore;
use super::events::{EVENT_SOURCE_ACP, EVENT_SOURCE_SYSTEM, Event, row_to_event};
use super::ids::{current_timestamp, next_event_id};
use super::records::SessionFilter;
use super::rows::validate_json_payload;

pub const SESSION_STATUS_ACTIVE: &str = "active";
pub const SESSION_STATUS_AVAILABLE: &str = "available";
pub const SESSION_STATUS_CLOSED: &str = "closed";
/// Operator-facing activity threshold used by the compact session status view.
pub const DEFAULT_SESSION_ACTIVITY_THRESHOLD: &str = "15m";
/// Operator-view actor labels; these are not ACP protocol values.
pub const SESSION_ACTIVITY_ACTOR_AGENT: &str = "agent";
pub const SESSION_ACTIVITY_ACTOR_USER: &str = "user";

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
pub struct SessionActivityRecord {
    pub id: String,
    pub created_at: String,
    pub updated_at: String,
    pub status: String,
    pub agent_id: String,
    pub cwd: String,
    pub title: Option<String>,
    pub last_activity_at: String,
    pub last_activity_from: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionUpdateBounds {
    pub first_updated_at: String,
    pub latest_updated_at: String,
    pub latest_status: String,
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
pub struct ListedSessionRecord {
    pub id: String,
    pub agent_id: String,
    pub cwd: String,
    pub title: Option<String>,
    pub updated_at: Option<String>,
    pub metadata_json: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ListedSessionUpsertCounts {
    pub upserted: u32,
    pub updated: u32,
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

    pub fn session_update_bounds(&self) -> Result<Option<SessionUpdateBounds>> {
        let first = self
            .connection()
            .query_row(
                r#"
                SELECT updated_at
                FROM sessions
                ORDER BY updated_at ASC, id ASC
                LIMIT 1
                "#,
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let Some(first_updated_at) = first else {
            return Ok(None);
        };
        let (latest_updated_at, latest_status) = self.connection().query_row(
            r#"
            SELECT updated_at, status
            FROM sessions
            ORDER BY updated_at DESC, id DESC
            LIMIT 1
            "#,
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?;
        Ok(Some(SessionUpdateBounds {
            first_updated_at,
            latest_updated_at,
            latest_status,
        }))
    }

    pub fn query_active_session_activity(&self, limit: u32) -> Result<Vec<SessionActivityRecord>> {
        let mut statement = self.connection().prepare(
            r#"
            WITH active_sessions AS (
                SELECT id, created_at, updated_at, status, agent_id, cwd, title
                FROM sessions
                WHERE status = ?1
            ),
            activity AS (
                SELECT e.session_id,
                       e.created_at AS activity_at,
                       CASE WHEN e.source = ?2 THEN ?3 ELSE ?4 END AS actor,
                       3 AS priority
                FROM events e
                JOIN active_sessions s ON s.id = e.session_id
                UNION ALL
                SELECT p.session_id,
                       p.created_at AS activity_at,
                       ?4 AS actor,
                       1 AS priority
                FROM prompts p
                JOIN active_sessions s ON s.id = p.session_id
                UNION ALL
                SELECT p.session_id,
                       p.updated_at AS activity_at,
                       ?3 AS actor,
                       2 AS priority
                FROM prompts p
                JOIN active_sessions s ON s.id = p.session_id
                WHERE p.status <> 'pending'
                UNION ALL
                SELECT s.id,
                       s.updated_at AS activity_at,
                       ?4 AS actor,
                       0 AS priority
                FROM active_sessions s
            ),
            ranked_activity AS (
                SELECT session_id,
                       activity_at,
                       actor,
                       ROW_NUMBER() OVER (
                           PARTITION BY session_id
                           ORDER BY activity_at DESC, priority DESC
                       ) AS row_number
                FROM activity
            )
            SELECT s.id,
                   s.created_at,
                   s.updated_at,
                   s.status,
                   s.agent_id,
                   s.cwd,
                   s.title,
                   r.activity_at,
                   r.actor
            FROM active_sessions s
            JOIN ranked_activity r ON r.session_id = s.id AND r.row_number = 1
            ORDER BY r.activity_at DESC, s.id DESC
            LIMIT ?5
            "#,
        )?;
        let rows = statement.query_map(
            params![
                SESSION_STATUS_ACTIVE,
                EVENT_SOURCE_ACP,
                SESSION_ACTIVITY_ACTOR_AGENT,
                SESSION_ACTIVITY_ACTOR_USER,
                i64::from(limit),
            ],
            |row| {
                Ok(SessionActivityRecord {
                    id: row.get(0)?,
                    created_at: row.get(1)?,
                    updated_at: row.get(2)?,
                    status: row.get(3)?,
                    agent_id: row.get(4)?,
                    cwd: row.get(5)?,
                    title: row.get(6)?,
                    last_activity_at: row.get(7)?,
                    last_activity_from: row.get(8)?,
                })
            },
        )?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn insert_session(&self, record: NewSessionRecord) -> Result<SessionRecord> {
        validate_json_payload(self.connection(), &record.metadata_json)?;
        let now = current_timestamp();
        let row = SessionRecord {
            id: record.id,
            created_at: now.clone(),
            updated_at: now,
            status: SESSION_STATUS_ACTIVE.to_owned(),
            agent_id: record.agent_id,
            cwd: record.cwd,
            title: record.title,
            metadata_json: record.metadata_json,
        };
        self.persist_with_outbox("sessions", &row.id, &row.created_at, |conn| {
            conn.execute(
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
            Ok(())
        })?;
        Ok(row)
    }

    pub fn upsert_listed_sessions(
        &self,
        records: Vec<ListedSessionRecord>,
    ) -> Result<ListedSessionUpsertCounts> {
        let mut counts = ListedSessionUpsertCounts::default();
        for record in records {
            let existing = self.get_session(&record.id)?;
            validate_json_payload(self.connection(), &record.metadata_json)?;
            let updated_at = record
                .updated_at
                .as_deref()
                .map(normalize_listed_session_timestamp)
                .transpose()?
                .unwrap_or_else(current_timestamp);
            match existing {
                Some(_) => {
                    self.persist_with_outbox("sessions", &record.id, &updated_at, |conn| {
                        conn.execute(
                            r#"
                            UPDATE sessions
                            SET updated_at = ?1,
                                status = CASE
                                    WHEN status IN (?2, ?3) THEN status
                                    ELSE ?4
                                END,
                                agent_id = ?5,
                                cwd = ?6,
                                title = ?7,
                                metadata_json = ?8
                            WHERE id = ?9
                            "#,
                            params![
                                updated_at,
                                SESSION_STATUS_ACTIVE,
                                SESSION_STATUS_CLOSED,
                                SESSION_STATUS_AVAILABLE,
                                record.agent_id,
                                record.cwd,
                                record.title,
                                record.metadata_json,
                                record.id,
                            ],
                        )?;
                        Ok(())
                    })?;
                    counts.updated += 1;
                }
                None => {
                    let created_at = current_timestamp();
                    self.persist_with_outbox("sessions", &record.id, &updated_at, |conn| {
                        conn.execute(
                            r#"
                            INSERT INTO sessions
                                (id, created_at, updated_at, status, agent_id, cwd, title, metadata_json)
                            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                            "#,
                            params![
                                record.id,
                                created_at,
                                updated_at,
                                SESSION_STATUS_AVAILABLE,
                                record.agent_id,
                                record.cwd,
                                record.title,
                                record.metadata_json,
                            ],
                        )?;
                        Ok(())
                    })?;
                    counts.upserted += 1;
                }
            }
        }
        Ok(counts)
    }

    pub fn update_session_status(&self, id: &str, status: &str) -> Result<()> {
        let now = current_timestamp();
        self.persist_with_outbox("sessions", id, &now, |conn| {
            let affected = conn.execute(
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
        })
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

        self.persist_with_outbox("events", &event.id, &event.created_at, |conn| {
            conn.execute(
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
            Ok(())
        })?;

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
        self.persist_with_outbox("prompts", &row.id, &row.created_at, |conn| {
            conn.execute(
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
            Ok(())
        })?;
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
        self.persist_with_outbox("prompts", id, &now, |conn| {
            let affected = conn.execute(
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
        })
    }

    /// Mark every `pending`/`running` prompt row as `errored` with the given
    /// reason. Called on daemon startup so prompts orphaned by a crash get a
    /// terminal status — otherwise clients polling those prompts would never
    /// see them settle. Returns the number of rows transitioned.
    pub fn reconcile_orphaned_prompts(&self, reason: &str) -> Result<usize> {
        let now = current_timestamp();
        if !self.external_logging_enabled() {
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
            return Ok(affected);
        }
        // External logging path: collect affected ids first so we can enqueue
        // them transactionally with the UPDATE.
        let tx = rusqlite::Transaction::new_unchecked(
            self.connection(),
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let ids: Vec<String> = {
            let mut statement =
                tx.prepare("SELECT id FROM prompts WHERE status IN ('pending', 'running')")?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let affected = tx.execute(
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
        for id in &ids {
            super::sink_outbox::enqueue(&tx, "prompts", id, &now)?;
        }
        tx.commit()?;
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

fn normalize_listed_session_timestamp(raw: &str) -> Result<String> {
    let parsed =
        chrono::DateTime::parse_from_rfc3339(raw).map_err(|err| StackError::InvalidParam {
            field: "updated_at",
            reason: format!("listed session timestamp is not valid RFC3339: {err}"),
        })?;
    Ok(parsed
        .with_timezone(&Utc)
        .to_rfc3339_opts(SecondsFormat::Nanos, true))
}
