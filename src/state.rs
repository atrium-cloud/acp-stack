//! Durable runtime state backed by SQLite.
//!
//! Each domain table lives in its own leaf module (`sessions`, `events`,
//! `commands`, `permissions`, `auth`, `agent`, `metrics`). Cross-cutting
//! primitives live in `ids` (id generators + timestamp), `records` (shared
//! filter DTOs), `rows` (json validation + the unified `events` query
//! predicate builder), and `schema` (migration runner). The `StateStore`
//! struct and its connection lifecycle live in `core`.

mod agent;
mod auth;
mod commands;
mod core;
mod events;
mod ids;
mod init;
mod metrics;
mod permissions;
mod records;
mod rows;
mod schema;
mod sessions;
pub(crate) mod sink_outbox;

pub use agent::{
    AgentCapabilitiesRecord, AgentLifecycleEvent, INSTALLER_OUTPUT_CAP_BYTES, InstallerRun,
    InstallerRunInput, default_installer_log_base,
};
pub use auth::{AuthFailure, AuthFailureFilter};
pub use commands::{CommandRecord, CommandStatus, NewCommandRecord};
pub use core::{StateStore, default_state_path};
pub use events::{
    EVENT_SOURCE_ACP, EVENT_SOURCE_API, EVENT_SOURCE_CLI, EVENT_SOURCE_COMMAND, EVENT_SOURCE_LOCAL,
    EVENT_SOURCE_PERMISSION, EVENT_SOURCE_SYSTEM, Event,
};
pub use ids::{
    next_command_id, next_deps_apply_run_id, next_permission_decision_id,
    next_permission_request_id, next_prompt_id, next_session_id,
};
pub use init::{
    INIT_RUN_FAILED, INIT_RUN_PENDING, INIT_RUN_RUNNING, INIT_RUN_SUCCEEDED, INIT_STEP_FAILED,
    INIT_STEP_PENDING, INIT_STEP_RUNNING, INIT_STEP_SKIPPED, INIT_STEP_SUCCEEDED, InitRunRecord,
    InitStepRecord, NewInitRun, NewInitStep,
};
pub use metrics::{
    ApiConnectionMetrics, CommandMetrics, MetricsSummary, MetricsWindow, PermissionMetrics,
    SecurityMetrics, SessionMetrics, StateCounts, TurnMetrics, UsageMetrics, WsConnectionMetrics,
};
pub use permissions::{
    NewPermissionRequest, PermissionDecisionRecord, PermissionRequestRecord, PermissionStatus,
};
pub use records::{CommandFilter, EventFilter, LogFilter, SessionFilter};
pub use sessions::{
    DEFAULT_SESSION_ACTIVITY_THRESHOLD, ListedSessionRecord, ListedSessionUpsertCounts,
    NewPromptRecord, NewSessionRecord, PromptRecord, PromptStatus, SESSION_ACTIVITY_ACTOR_AGENT,
    SESSION_ACTIVITY_ACTOR_USER, SESSION_STATUS_ACTIVE, SESSION_STATUS_AVAILABLE,
    SESSION_STATUS_CLOSED, SessionActivityRecord, SessionRecord, SessionUpdateBounds,
};
