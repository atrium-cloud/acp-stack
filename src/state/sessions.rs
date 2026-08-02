//! Sessions, prompts, and session-scoped event persistence.

use crate::error::{Result, StackError};
use chrono::{SecondsFormat, Utc};
use rusqlite::{OptionalExtension, params};

use super::core::StateStore;
use super::events::{EVENT_SOURCE_ACP, EVENT_SOURCE_SYSTEM, Event, row_to_event};
use super::ids::{current_timestamp, next_event_id};
use super::records::{LogOrder, SessionFilter};
use super::rows::validate_json_payload;
use super::sink_outbox;

mod events;
mod prompts;
mod queries;

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
/// Session-scoped event kind: configured MCP servers were dropped from the
/// session because the running agent does not advertise their transport.
/// Payload carries `session_id` and the skipped `name`/`capability` pairs.
pub const EVENT_KIND_MCP_SESSION_SKIPPED: &str = "mcp.session_skipped";
/// Session-scoped event kind: a configured capability-backed feature (mode,
/// model) was ignored because the agent does not advertise the capability.
/// The session proceeds with the agent's default. Payload carries
/// `session_id` and the ignored `feature`/`target`/`capability` entries.
pub const EVENT_KIND_SESSION_CAPABILITY_IGNORED: &str = "session.capability_ignored";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    pub id: String,
    pub target_id: String,
    pub agent_session_id: String,
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
    pub target_id: String,
    pub agent_session_id: String,
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
    pub target_id: String,
    pub agent_session_id: String,
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
pub struct RestartBlockerRecord {
    pub session_id: String,
    pub target_id: String,
    pub state: String,
    pub prompt_id: Option<String>,
    pub prompt_status: Option<String>,
    pub prompt_stop_reason: Option<String>,
    pub permission_id: Option<String>,
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
    pub agent_session_id: String,
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
        target_id: row.get(1)?,
        agent_session_id: row.get(2)?,
        created_at: row.get(3)?,
        updated_at: row.get(4)?,
        status: row.get(5)?,
        agent_id: row.get(6)?,
        cwd: row.get(7)?,
        title: row.get(8)?,
        metadata_json: row.get(9)?,
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
