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
mod security;
mod security_category;
mod sessions;
pub(crate) mod sink_outbox;
mod stack_update;

pub use agent::{
    AgentCapabilitiesRecord, AgentFailureRecord, AgentLifecycleEvent, AgentStartedProcess,
    INSTALLER_METHOD_APT, INSTALLER_METHOD_GITHUB, INSTALLER_METHOD_NATIVE, INSTALLER_METHOD_NPM,
    INSTALLER_METHOD_SHELL, INSTALLER_OPERATION_INSTALL, INSTALLER_OPERATION_UPDATE,
    INSTALLER_OUTPUT_CAP_BYTES, InstallerRun, InstallerRunInput, default_installer_log_base,
};
pub use auth::{AuthFailure, AuthFailureFilter, AuthKeyRecord};
pub use commands::{CommandRecord, CommandStatus, NewCommandRecord};
pub use core::{StateStore, default_state_path};
pub use events::{
    EVENT_SOURCE_ACP, EVENT_SOURCE_API, EVENT_SOURCE_CLI, EVENT_SOURCE_COMMAND, EVENT_SOURCE_LOCAL,
    EVENT_SOURCE_PERMISSION, EVENT_SOURCE_SYSTEM, Event,
};
pub use ids::{
    next_command_id, next_deps_apply_run_id, next_permission_decision_id,
    next_permission_request_id, next_prompt_id, next_prompt_message_id, next_session_id,
};
pub use init::{
    INIT_RUN_FAILED, INIT_RUN_PENDING, INIT_RUN_RUNNING, INIT_RUN_SUCCEEDED, INIT_STEP_FAILED,
    INIT_STEP_PENDING, INIT_STEP_RUNNING, INIT_STEP_SKIPPED, INIT_STEP_SUCCEEDED, InitRunRecord,
    InitStepRecord, NewInitRun, NewInitStep,
};
pub use metrics::{
    ApiConnectionMetrics, CommandMetrics, MetricsSummary, MetricsWindow, PermissionMetrics,
    PromptFailureMetrics, SecurityMetrics, SessionMetrics, StateCounts, TurnMetrics, UsageMetrics,
    WsConnectionMetrics,
};
pub use permissions::{
    NewPermissionRequest, PermissionDecisionRecord, PermissionRequestRecord, PermissionStatus,
};
pub use records::{CommandFilter, EventFilter, LogFilter, LogOrder, SessionFilter};
pub use security::{
    NewSecurityFinding, NewSecurityRun, SECURITY_FINDING_SEVERITY_CRITICAL,
    SECURITY_FINDING_SEVERITY_WARNING, SECURITY_RUN_FAILED, SECURITY_RUN_SUCCEEDED,
    SecurityFindingRow, SecurityRunFilter, SecurityRunRecord,
};
pub use security_category::SecurityCategory;
pub use sessions::{
    DEFAULT_SESSION_ACTIVITY_THRESHOLD, DEFAULT_SESSION_STATUS_WINDOW, EVENT_KIND_PROMPT_ERRORED,
    EVENT_KIND_PROMPT_INFERENCE_FAILED, EVENT_KIND_PROMPT_STALLED, FailureClass,
    ListedSessionRecord, ListedSessionUpsertCounts, MAX_SESSION_STATUS_WINDOW_SECS,
    MIN_SESSION_STATUS_WINDOW_SECS, NewPromptRecord, NewSessionRecord, PromptRecord, PromptStatus,
    SESSION_ACTIVITY_ACTOR_AGENT, SESSION_ACTIVITY_ACTOR_USER, SESSION_STATUS_ACTIVE,
    SESSION_STATUS_AVAILABLE, SESSION_STATUS_CLOSED, SessionActivityRecord, SessionRecord,
    SessionStatusPermissionRecord, SessionStatusPromptRecord, SessionStatusRecord,
    SessionUpdateBounds,
};
pub use stack_update::{
    NewStackUpdateRun, STACK_UPDATE_OPERATION_CHECK, STACK_UPDATE_OPERATION_INSTALL,
    STACK_UPDATE_STATUS_FAILED, STACK_UPDATE_STATUS_SKIPPED, STACK_UPDATE_STATUS_SUCCEEDED,
    StackUpdateRun,
};
