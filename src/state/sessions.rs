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
/// Default rolling window for the multi-session turn status view.
pub const DEFAULT_SESSION_STATUS_WINDOW: &str = "8h";
/// Shorter windows are too noisy for human session monitoring.
pub const MIN_SESSION_STATUS_WINDOW_SECS: i64 = 60;
/// Keep status queries bounded while still allowing long workday views.
pub const MAX_SESSION_STATUS_WINDOW_SECS: i64 = 999 * 60 * 60;
/// Operator-view actor labels; these are not ACP protocol values.
pub const SESSION_ACTIVITY_ACTOR_AGENT: &str = "agent";
pub const SESSION_ACTIVITY_ACTOR_USER: &str = "user";

/// Session-scoped event kind: the prompt's underlying inference endpoint
/// returned an HTTP error (5xx, 429, etc.). Payload carries `prompt_id`,
/// `status_code`, and `reason_category`.
pub const EVENT_KIND_PROMPT_INFERENCE_FAILED: &str = "prompt.inference_failed";
/// Session-scoped event kind: the prompt was forcibly transitioned to
/// `stalled` because no progress was observed within the inactivity
/// threshold. Payload carries `prompt_id` and the last-update timestamp.
pub const EVENT_KIND_PROMPT_STALLED: &str = "prompt.stalled";
/// Session-scoped event kind: the prompt reached a terminal `errored`
/// status for a non-inference reason. Payload carries `prompt_id` and the
/// `error_code` string.
pub const EVENT_KIND_PROMPT_ERRORED: &str = "prompt.errored";

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
pub struct SessionStatusRecord {
    pub id: String,
    pub created_at: String,
    pub updated_at: String,
    pub status: String,
    pub agent_id: String,
    pub cwd: String,
    pub title: Option<String>,
    pub last_activity_at: String,
    pub last_activity_from: String,
    pub latest_prompt: Option<SessionStatusPromptRecord>,
    pub pending_permission: Option<SessionStatusPermissionRecord>,
    pub prompt_stream_started_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionStatusPromptRecord {
    pub id: String,
    pub created_at: String,
    pub updated_at: String,
    pub status: String,
    pub stop_reason: Option<String>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub message_id: Option<String>,
    pub message_id_acknowledged: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionStatusPermissionRecord {
    pub id: String,
    pub created_at: String,
    pub updated_at: String,
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
    pub message_id: Option<String>,
    pub message_id_acknowledged: bool,
    /// Internal failure taxonomy (see `FailureClass`). Populated only for
    /// terminal `errored`/`stalled` rows; otherwise NULL in the DB and `None`
    /// here. Phase 2 wires up the supervisor call sites.
    pub failure_class: Option<String>,
    /// JSON envelope with class-specific details (e.g. underlying error
    /// code, last heartbeat timestamp, agent stderr tail). Free-form on
    /// purpose so each taxonomy class can attach whatever is useful.
    pub failure_detail_json: Option<String>,
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
    /// Terminal status for prompts the runtime gave up on (e.g. no agent
    /// progress past the inactivity threshold). Distinct from `Errored` so
    /// dashboards and clients can surface stalled prompts separately.
    Stalled,
}

impl PromptStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            PromptStatus::Pending => "pending",
            PromptStatus::Running => "running",
            PromptStatus::Completed => "completed",
            PromptStatus::Errored => "errored",
            PromptStatus::Cancelled => "cancelled",
            PromptStatus::Stalled => "stalled",
        }
    }

    /// True for statuses that will not transition further. Lets supervisor
    /// reconciliation skip rows that are already done instead of forcing
    /// them through another taxonomy pass.
    pub fn terminal(self) -> bool {
        matches!(
            self,
            PromptStatus::Completed
                | PromptStatus::Errored
                | PromptStatus::Cancelled
                | PromptStatus::Stalled
        )
    }
}

impl std::str::FromStr for PromptStatus {
    type Err = StackError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "pending" => Ok(PromptStatus::Pending),
            "running" => Ok(PromptStatus::Running),
            "completed" => Ok(PromptStatus::Completed),
            "errored" => Ok(PromptStatus::Errored),
            "cancelled" => Ok(PromptStatus::Cancelled),
            "stalled" => Ok(PromptStatus::Stalled),
            other => Err(StackError::InvalidParam {
                field: "prompt_status",
                reason: format!("unknown prompt status `{other}`"),
            }),
        }
    }
}

/// Internal taxonomy attached to terminal `errored` and `stalled` prompt
/// rows so operators can group failures by root cause without scraping
/// `error_message`. Persisted as snake_case strings in `prompts.failure_class`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    /// Agent-side request failure (ACP protocol error, bad request shape).
    AgentRequest,
    /// Upstream inference service returned a 5xx-style failure.
    Inference5xx,
    /// Upstream inference service returned a 4xx-style failure.
    Inference4xx,
    /// VM / sandbox layer failure (workspace mount, syscall guard, etc.).
    Vm,
    /// SQLite-level failure (constraint violation, IO error).
    Sqlite,
    /// Daemon-level failure (supervisor crash, runtime panic).
    Daemon,
    /// Agent subprocess failure (binary crash, missing stream).
    AgentProcess,
    /// Inactivity threshold exceeded; paired with `PromptStatus::Stalled`.
    Stalled,
}

impl FailureClass {
    pub fn as_str(self) -> &'static str {
        match self {
            FailureClass::AgentRequest => "agent_request",
            FailureClass::Inference5xx => "inference_5xx",
            FailureClass::Inference4xx => "inference_4xx",
            FailureClass::Vm => "vm",
            FailureClass::Sqlite => "sqlite",
            FailureClass::Daemon => "daemon",
            FailureClass::AgentProcess => "agent_process",
            FailureClass::Stalled => "stalled",
        }
    }
}

impl std::str::FromStr for FailureClass {
    type Err = StackError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "agent_request" => Ok(FailureClass::AgentRequest),
            "inference_5xx" => Ok(FailureClass::Inference5xx),
            "inference_4xx" => Ok(FailureClass::Inference4xx),
            "vm" => Ok(FailureClass::Vm),
            "sqlite" => Ok(FailureClass::Sqlite),
            "daemon" => Ok(FailureClass::Daemon),
            "agent_process" => Ok(FailureClass::AgentProcess),
            "stalled" => Ok(FailureClass::Stalled),
            other => Err(StackError::InvalidParam {
                field: "failure_class",
                reason: format!("unknown failure class `{other}`"),
            }),
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
        message_id: row.get(9)?,
        message_id_acknowledged: row.get::<_, i64>(10)? != 0,
        failure_class: row.get(11)?,
        failure_detail_json: row.get(12)?,
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
            match filter.order {
                super::records::LogOrder::Desc => sql.push_str(
                    " AND (updated_at, id) < (SELECT updated_at, id FROM sessions WHERE id = ?)",
                ),
                super::records::LogOrder::Asc => sql.push_str(
                    " AND (updated_at, id) > (SELECT updated_at, id FROM sessions WHERE id = ?)",
                ),
            }
            bindings.push(rusqlite::types::Value::Text(after.to_owned()));
        }
        let direction = filter.order.sql_keyword();
        sql.push_str(&format!(
            " ORDER BY updated_at {direction}, id {direction} LIMIT ?"
        ));
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

    pub fn query_session_status_window(
        &self,
        since: &str,
        limit: u32,
    ) -> Result<Vec<SessionStatusRecord>> {
        let mut statement = self.connection().prepare(
            r#"
            WITH activity AS (
                SELECT s.id AS session_id,
                       s.updated_at AS activity_at,
                       ?2 AS actor,
                       0 AS priority
                FROM sessions s
                WHERE s.updated_at >= ?1
                UNION ALL
                SELECT p.session_id,
                       p.created_at AS activity_at,
                       ?2 AS actor,
                       1 AS priority
                FROM prompts p
                WHERE p.created_at >= ?1
                UNION ALL
                SELECT p.session_id,
                       p.updated_at AS activity_at,
                       ?3 AS actor,
                       2 AS priority
                FROM prompts p
                WHERE p.status <> 'pending'
                  AND p.updated_at >= ?1
                UNION ALL
                SELECT e.session_id,
                       e.created_at AS activity_at,
                       CASE WHEN e.source = ?4 THEN ?3 ELSE ?2 END AS actor,
                       3 AS priority
                FROM events e
                WHERE e.session_id IS NOT NULL
                  AND e.created_at >= ?1
                UNION ALL
                SELECT pr.subject_id AS session_id,
                       pr.created_at AS activity_at,
                       ?3 AS actor,
                       4 AS priority
                FROM permission_requests pr
                WHERE pr.status = 'pending'
                  AND pr.source = 'acp'
                  AND pr.subject_id IS NOT NULL
                  AND pr.created_at >= ?1
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
            ),
            window_sessions AS (
                SELECT session_id,
                       activity_at,
                       actor
                FROM ranked_activity
                WHERE row_number = 1
            ),
            latest_prompts AS (
                SELECT id, session_id, created_at, updated_at, status,
                       stop_reason, error_code, error_message, message_id,
                       message_id_acknowledged
                FROM (
                    SELECT p.id, p.session_id, p.created_at, p.updated_at, p.status,
                           p.stop_reason, p.error_code, p.error_message, p.message_id,
                           p.message_id_acknowledged,
                           ROW_NUMBER() OVER (
                               PARTITION BY p.session_id
                               ORDER BY
                                   CASE WHEN p.status IN ('pending', 'running') THEN 0 ELSE 1 END ASC,
                                   CASE WHEN p.status IN ('pending', 'running') THEN p.created_at END ASC,
                                   CASE WHEN p.status IN ('pending', 'running') THEN p.id END ASC,
                                   CASE WHEN p.status NOT IN ('pending', 'running') THEN p.created_at END DESC,
                                   CASE WHEN p.status NOT IN ('pending', 'running') THEN p.id END DESC
                           ) AS row_number
                    FROM prompts p
                    JOIN window_sessions ws ON ws.session_id = p.session_id
                )
                WHERE row_number = 1
            ),
            pending_acp_permissions AS (
                SELECT id, session_id, created_at, updated_at
                FROM (
                    SELECT pr.id, pr.subject_id AS session_id, pr.created_at, pr.updated_at,
                           ROW_NUMBER() OVER (
                               PARTITION BY pr.subject_id
                               ORDER BY pr.created_at ASC, pr.id ASC
                           ) AS row_number
                    FROM permission_requests pr
                    JOIN window_sessions ws ON ws.session_id = pr.subject_id
                    WHERE pr.status = 'pending'
                      AND pr.source = 'acp'
                      AND pr.subject_id IS NOT NULL
                )
                WHERE row_number = 1
            )
            SELECT s.id,
                   s.created_at,
                   s.updated_at,
                   s.status,
                   s.agent_id,
                   s.cwd,
                   s.title,
                   r.activity_at,
                   r.actor,
                   lp.id,
                   lp.created_at,
                   lp.updated_at,
                   lp.status,
                   lp.stop_reason,
                   lp.error_code,
                   lp.error_message,
                   lp.message_id,
                   lp.message_id_acknowledged,
                   pp.id,
                   pp.created_at,
                   pp.updated_at,
                   (
                       SELECT MIN(e.created_at)
                       FROM events e
                       WHERE e.session_id = s.id
                         AND e.kind = 'session.update'
                         AND e.source = ?4
                         AND lp.id IS NOT NULL
                         AND e.created_at >= lp.created_at
                   ) AS prompt_stream_started_at
            FROM sessions s
            JOIN window_sessions r ON r.session_id = s.id
            LEFT JOIN latest_prompts lp ON lp.session_id = s.id
            LEFT JOIN pending_acp_permissions pp ON pp.session_id = s.id
            ORDER BY r.activity_at DESC, s.id DESC
            LIMIT ?5
            "#,
        )?;
        let rows = statement.query_map(
            params![
                since,
                SESSION_ACTIVITY_ACTOR_USER,
                SESSION_ACTIVITY_ACTOR_AGENT,
                EVENT_SOURCE_ACP,
                i64::from(limit),
            ],
            |row| {
                let prompt_id: Option<String> = row.get(9)?;
                let latest_prompt = match prompt_id {
                    Some(id) => Some(SessionStatusPromptRecord {
                        id,
                        created_at: row.get(10)?,
                        updated_at: row.get(11)?,
                        status: row.get(12)?,
                        stop_reason: row.get(13)?,
                        error_code: row.get(14)?,
                        error_message: row.get(15)?,
                        message_id: row.get(16)?,
                        message_id_acknowledged: row.get::<_, Option<i64>>(17)?.unwrap_or(0) != 0,
                    }),
                    None => None,
                };
                let permission_id: Option<String> = row.get(18)?;
                let pending_permission = match permission_id {
                    Some(id) => Some(SessionStatusPermissionRecord {
                        id,
                        created_at: row.get(19)?,
                        updated_at: row.get(20)?,
                    }),
                    None => None,
                };
                Ok(SessionStatusRecord {
                    id: row.get(0)?,
                    created_at: row.get(1)?,
                    updated_at: row.get(2)?,
                    status: row.get(3)?,
                    agent_id: row.get(4)?,
                    cwd: row.get(5)?,
                    title: row.get(6)?,
                    last_activity_at: row.get(7)?,
                    last_activity_from: row.get(8)?,
                    latest_prompt,
                    pending_permission,
                    prompt_stream_started_at: row.get(21)?,
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

    pub fn update_session_status_and_cwd(&self, id: &str, status: &str, cwd: &str) -> Result<()> {
        let now = current_timestamp();
        self.persist_with_outbox("sessions", id, &now, |conn| {
            let affected = conn.execute(
                r#"
                UPDATE sessions
                SET status = ?1, cwd = ?2, updated_at = ?3
                WHERE id = ?4
                "#,
                params![status, cwd, now, id],
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
            session_id: Some(session_id.to_owned()),
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
            // `logs` subscribers expect every persisted event, and
            // `sessions.{id}` subscribers also need it so a per-session
            // WebSocket sees the same row that landed in SQLite. Missing the
            // second call stranded session-scoped events on the logs topic
            // only, breaking reconnect/live-tail flows.
            hub.publish_log_event(&event);
            hub.publish_session_update(session_id, &event, &event.payload_json);
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
                    SELECT e.id, e.created_at, e.level, e.kind, e.message, e.payload_json, e.source, e.session_id
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
                    SELECT id, created_at, level, kind, message, payload_json, source, session_id
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

    /// Newest-first window of session-scoped events. Used by the snapshot
    /// endpoint so a reconnecting client gets the most-recent slice without
    /// having to page from the beginning of the table. Ordering mirrors
    /// `query_session_events` (the `(created_at, id)` pair is stable across
    /// inserts sharing a clock tick), just reversed.
    pub fn latest_session_events(&self, session_id: &str, limit: u32) -> Result<Vec<Event>> {
        let limit = i64::from(limit);
        let mut statement = self.connection().prepare(
            r#"
            SELECT id, created_at, level, kind, message, payload_json, source, session_id
            FROM events
            WHERE session_id = ?1
            ORDER BY created_at DESC, id DESC
            LIMIT ?2
            "#,
        )?;
        let rows = statement.query_map(params![session_id, limit], row_to_event)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn insert_prompt(&self, record: NewPromptRecord) -> Result<PromptRecord> {
        self.insert_prompt_with_message_id(record, None)
    }

    pub fn insert_prompt_with_message_id(
        &self,
        record: NewPromptRecord,
        message_id: Option<String>,
    ) -> Result<PromptRecord> {
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
            message_id,
            message_id_acknowledged: false,
            failure_class: None,
            failure_detail_json: None,
        };
        self.persist_with_outbox("prompts", &row.id, &row.created_at, |conn| {
            conn.execute(
                r#"
                INSERT INTO prompts
                    (id, session_id, created_at, updated_at, status, prompt_json, message_id)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                "#,
                params![
                    row.id,
                    row.session_id,
                    row.created_at,
                    row.updated_at,
                    row.status,
                    row.prompt_json,
                    row.message_id,
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
                       stop_reason, error_code, error_message, prompt_json,
                       message_id, message_id_acknowledged,
                       failure_class, failure_detail_json
                FROM prompts
                WHERE id = ?1
                "#,
                params![id],
                row_to_prompt,
            )
            .optional()?)
    }

    pub fn get_prompt_by_message_id(
        &self,
        session_id: &str,
        message_id: &str,
    ) -> Result<Option<PromptRecord>> {
        Ok(self
            .connection()
            .query_row(
                r#"
                SELECT id, session_id, created_at, updated_at, status,
                       stop_reason, error_code, error_message, prompt_json,
                       message_id, message_id_acknowledged,
                       failure_class, failure_detail_json
                FROM prompts
                WHERE session_id = ?1 AND message_id = ?2
                "#,
                params![session_id, message_id],
                row_to_prompt,
            )
            .optional()?)
    }

    pub fn acknowledge_prompt_message_id(&self, prompt_id: &str, message_id: &str) -> Result<()> {
        let now = current_timestamp();
        self.persist_with_outbox("prompts", prompt_id, &now, |conn| {
            let affected = conn.execute(
                r#"
                UPDATE prompts
                SET message_id_acknowledged = 1,
                    updated_at = ?1
                WHERE id = ?2 AND message_id = ?3
                "#,
                params![now, prompt_id, message_id],
            )?;
            if affected == 0 {
                return Err(StackError::PromptNotFound {
                    id: prompt_id.to_owned(),
                });
            }
            Ok(())
        })
    }

    /// Update a prompt's lifecycle row. `failure_class` and
    /// `failure_detail_json` follow a three-valued convention to keep callers
    /// from clobbering prior taxonomy on a status flip:
    ///
    ///   * `None` preserves the existing column value.
    ///   * `Some("")` writes SQL NULL — used to explicitly clear a value.
    ///   * `Some(value)` overwrites with the new value.
    ///
    /// Phase 1 callers all pass `None, None`; Phase 2 will populate real
    /// failure taxonomies at the supervisor settle path.
    #[allow(clippy::too_many_arguments)]
    pub fn update_prompt_status(
        &self,
        id: &str,
        status: PromptStatus,
        stop_reason: Option<&str>,
        error_code: Option<&str>,
        error_message: Option<&str>,
        failure_class: Option<&str>,
        failure_detail_json: Option<&str>,
    ) -> Result<bool> {
        let now = current_timestamp();
        let failure_class_param = failure_class.map(|value| {
            if value.is_empty() {
                None
            } else {
                Some(value.to_owned())
            }
        });
        let failure_detail_param = failure_detail_json.map(|value| {
            if value.is_empty() {
                None
            } else {
                Some(value.to_owned())
            }
        });

        let update = |conn: &rusqlite::Connection| -> Result<bool> {
            // The WHERE excludes terminal statuses so a late settle from the
            // supervisor cannot overwrite a prompt that the stale-prompt
            // sweeper (or any earlier path) already moved to a terminal state.
            // `stalled` is documented as terminal; without this guard the
            // supervisor's eventual `completed`/`errored`/`cancelled` write
            // would race the sweeper.
            let affected = conn.execute(
                r#"
                UPDATE prompts
                SET status = ?1,
                    updated_at = ?2,
                    stop_reason = ?3,
                    error_code = ?4,
                    error_message = ?5,
                    failure_class = CASE WHEN ?6 = 1 THEN ?7 ELSE failure_class END,
                    failure_detail_json = CASE WHEN ?8 = 1 THEN ?9 ELSE failure_detail_json END
                WHERE id = ?10
                  AND status NOT IN ('completed', 'errored', 'cancelled', 'stalled')
                "#,
                params![
                    status.as_str(),
                    now,
                    stop_reason,
                    error_code,
                    error_message,
                    i64::from(failure_class_param.is_some()),
                    failure_class_param
                        .as_ref()
                        .and_then(|inner| inner.as_deref()),
                    i64::from(failure_detail_param.is_some()),
                    failure_detail_param
                        .as_ref()
                        .and_then(|inner| inner.as_deref()),
                    id
                ],
            )?;
            if affected == 0 {
                // Disambiguate: row missing entirely vs row already terminal.
                let exists: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM prompts WHERE id = ?1",
                    params![id],
                    |row| row.get(0),
                )?;
                if exists == 0 {
                    return Err(StackError::PromptNotFound { id: id.to_owned() });
                }
                tracing::warn!(
                    prompt_id = %id,
                    new_status = %status.as_str(),
                    "skipping update_prompt_status on already-terminal prompt"
                );
                return Ok(false);
            }
            Ok(true)
        };

        if !self.external_logging_enabled() {
            return update(self.connection());
        }
        let tx = rusqlite::Transaction::new_unchecked(
            self.connection(),
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let updated = update(&tx)?;
        if updated {
            super::sink_outbox::enqueue(&tx, "prompts", id, &now)?;
        }
        tx.commit()?;
        Ok(updated)
    }

    /// Mark every `pending`/`running` prompt row as `errored` with the given
    /// reason. Called on daemon startup so prompts orphaned by a crash get a
    /// terminal status — otherwise clients polling those prompts would never
    /// see them settle. Returns the number of rows transitioned. The rows are
    /// classified `agent_process` because the daemon restart implies the
    /// underlying agent subprocess died with the daemon.
    pub fn reconcile_orphaned_prompts(&self, reason: &str) -> Result<usize> {
        let now = current_timestamp();
        if !self.external_logging_enabled() {
            let affected = self.connection().execute(
                r#"
                UPDATE prompts
                SET status = 'errored',
                    updated_at = ?1,
                    error_code = 'agent.daemon_restart',
                    error_message = ?2,
                    failure_class = 'agent_process'
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
                error_message = ?2,
                failure_class = 'agent_process'
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

    /// Mark every `pending`/`running` prompt row whose `updated_at` is
    /// older than `now - threshold` as `Stalled`. Used by the background
    /// sweeper so prompts whose agent stopped streaming ACP `session/update`
    /// notifications still settle to a terminal status — otherwise clients
    /// polling those rows would never see them resolve.
    ///
    /// Returns `(prompt_id, session_id)` pairs for every flipped row so the
    /// caller can emit a per-session `prompt.stalled` event. Idempotent:
    /// rows already in a terminal status (`completed`, `errored`,
    /// `cancelled`, `stalled`) are filtered out by the `WHERE` clause.
    pub fn mark_stalled_prompts(
        &self,
        threshold: std::time::Duration,
        reason: &str,
    ) -> Result<Vec<(String, String)>> {
        let now = Utc::now();
        let now_string = now.to_rfc3339_opts(SecondsFormat::Nanos, true);
        // The threshold cutoff timestamp is formatted the same way as
        // `prompts.updated_at` so the `<` comparison is exact at the
        // string level — every row writer goes through `current_timestamp`
        // which uses identical SecondsFormat::Nanos formatting.
        let threshold_chrono =
            chrono::Duration::from_std(threshold).map_err(|err| StackError::InvalidParam {
                field: "prompts.stale_threshold",
                reason: format!("threshold out of range: {err}"),
            })?;
        let cutoff = now
            .checked_sub_signed(threshold_chrono)
            .ok_or(StackError::InvalidParam {
                field: "prompts.stale_threshold",
                reason: "threshold subtraction underflowed the chrono range".to_owned(),
            })?;
        let cutoff_string = cutoff.to_rfc3339_opts(SecondsFormat::Nanos, true);

        if !self.external_logging_enabled() {
            let mut statement = self.connection().prepare(
                r#"
                UPDATE prompts
                SET status = 'stalled',
                    updated_at = ?1,
                    error_code = 'prompt.stalled',
                    error_message = ?2,
                    failure_class = 'stalled'
                WHERE status IN ('pending', 'running')
                  AND updated_at < ?3
                RETURNING id, session_id
                "#,
            )?;
            let rows = statement.query_map(params![now_string, reason, cutoff_string], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            return Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?);
        }
        // External logging path: run the UPDATE ... RETURNING inside an
        // IMMEDIATE transaction and enqueue an outbox row per flipped prompt
        // so the terminal status reaches Supabase atomically.
        let tx = rusqlite::Transaction::new_unchecked(
            self.connection(),
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let pairs: Vec<(String, String)> = {
            let mut statement = tx.prepare(
                r#"
                UPDATE prompts
                SET status = 'stalled',
                    updated_at = ?1,
                    error_code = 'prompt.stalled',
                    error_message = ?2,
                    failure_class = 'stalled'
                WHERE status IN ('pending', 'running')
                  AND updated_at < ?3
                RETURNING id, session_id
                "#,
            )?;
            let rows = statement.query_map(params![now_string, reason, cutoff_string], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        for (id, _session_id) in &pairs {
            super::sink_outbox::enqueue(&tx, "prompts", id, &now_string)?;
        }
        tx.commit()?;
        Ok(pairs)
    }

    /// Count of `pending`/`running` prompt rows older than `now - threshold`,
    /// plus the oldest such row's `updated_at`. Drives the `PromptsHealth`
    /// subsystem so `/v1/health/ready` and `acps status` can warn an
    /// operator that a row is stuck before the sweeper has a chance to
    /// flip it. The threshold matches the sweeper threshold so a single
    /// idle tick is normal and only persistent overrun shows up here.
    pub fn count_stuck_prompts(
        &self,
        threshold: std::time::Duration,
    ) -> Result<(i64, Option<String>)> {
        let now = Utc::now();
        let threshold_chrono =
            chrono::Duration::from_std(threshold).map_err(|err| StackError::InvalidParam {
                field: "prompts.stale_threshold",
                reason: format!("threshold out of range: {err}"),
            })?;
        let cutoff = now
            .checked_sub_signed(threshold_chrono)
            .ok_or(StackError::InvalidParam {
                field: "prompts.stale_threshold",
                reason: "threshold subtraction underflowed the chrono range".to_owned(),
            })?;
        let cutoff_string = cutoff.to_rfc3339_opts(SecondsFormat::Nanos, true);
        let row = self.connection().query_row(
            r#"
            SELECT COUNT(*), MIN(updated_at)
            FROM prompts
            WHERE status IN ('pending', 'running')
              AND updated_at < ?1
            "#,
            params![cutoff_string],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?)),
        )?;
        Ok(row)
    }

    pub fn in_flight_prompts_for_session(&self, session_id: &str) -> Result<Vec<PromptRecord>> {
        let mut statement = self.connection().prepare(
            r#"
            SELECT id, session_id, created_at, updated_at, status,
                   stop_reason, error_code, error_message, prompt_json,
                   message_id, message_id_acknowledged,
                   failure_class, failure_detail_json
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
