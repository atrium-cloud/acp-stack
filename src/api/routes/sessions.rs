use axum::Json;
use axum::extract::{Path, Query, State};
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use super::super::core::AgentTargetRuntime;
use super::super::core::AppState;
use super::agent::{ensure_agent_started, open_mcp_servers};
use super::logs::{LogEventJson, MAX_LOGS_LIMIT, default_logs_limit};
use crate::envelope::ApiSuccess;
use crate::error::{Result, StackError};
use crate::runtime::agent::model_catalog::selected_agent_model;
use crate::runtime::agent::session_changes::SessionChangesSnapshot;
use crate::runtime::agent::supervisor::parse_prompt_blocks;
use crate::runtime::agent::supervisor::{SessionListSyncResult, resolve_session_cwd};
use crate::state::{
    DEFAULT_SESSION_ACTIVITY_THRESHOLD, DEFAULT_SESSION_STATUS_WINDOW,
    MAX_SESSION_STATUS_WINDOW_SECS, MIN_SESSION_STATUS_WINDOW_SECS, PromptRecord,
    SESSION_METADATA_AVAILABLE_COMMANDS, SESSION_METADATA_AVAILABLE_COMMANDS_UPDATED_AT,
    SESSION_STATUS_ACTIVE, SESSION_STATUS_AVAILABLE, SESSION_STATUS_CLOSED,
    SessionAvailableCommand, SessionRecord, SessionStatusRecord, SessionUpdateBounds,
};

pub(crate) mod commands;
pub(crate) mod events;
pub(crate) mod lifecycle;
pub(crate) mod list;
pub(crate) mod prompts;
pub(crate) mod status;
pub(crate) mod teardown;

// Router wiring (`api::core`, `local_listener::router`) imports handlers as
// `sessions::<handler>`; re-export them so the split is invisible to callers.
pub(crate) use commands::{sessions_commands_handler, sessions_commands_run_handler};
pub(crate) use events::{
    sessions_changes_handler, sessions_events_handler, sessions_snapshot_handler,
};
pub(crate) use lifecycle::{
    sessions_create_handler, sessions_fork_handler, sessions_get_handler, sessions_load_handler,
    sessions_resume_handler,
};
pub(crate) use list::sessions_list_handler;
pub(crate) use prompts::{sessions_prompt_handler, sessions_prompt_status_handler};
pub(crate) use status::sessions_status_handler;
pub(crate) use teardown::{
    sessions_cancel_handler, sessions_close_handler, sessions_delete_handler,
};

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct SessionResponse {
    id: String,
    target_id: String,
    agent_session_id: String,
    created_at: String,
    updated_at: String,
    #[schemars(extend("enum" = ["active", "available", "closed"]))]
    status: String,
    agent_id: String,
    cwd: String,
    title: Option<String>,
    metadata_json: String,
    /// Configured features (mode, model) the agent's advertised capabilities
    /// could not honor; the session proceeded on agent defaults. Omitted when
    /// nothing was ignored, so list/read responses are unchanged.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    ignored: Vec<crate::runtime::agent::acp_bridge::IgnoredFeature>,
}

impl From<SessionRecord> for SessionResponse {
    fn from(record: SessionRecord) -> Self {
        Self {
            id: record.id,
            target_id: record.target_id,
            agent_session_id: record.agent_session_id,
            created_at: record.created_at,
            updated_at: record.updated_at,
            status: record.status,
            agent_id: record.agent_id,
            cwd: record.cwd,
            title: record.title,
            metadata_json: record.metadata_json,
            ignored: Vec::new(),
        }
    }
}

impl From<crate::runtime::agent::supervisor::SessionAttachOutcome> for SessionResponse {
    fn from(outcome: crate::runtime::agent::supervisor::SessionAttachOutcome) -> Self {
        let mut response = Self::from(outcome.record);
        response.ignored = outcome.ignored;
        response
    }
}

/// Wire shape of one agent-advertised slash command (compact projection of
/// the ACP `AvailableCommand`; names carry no leading slash).
#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct AvailableCommandResponse {
    name: String,
    description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    input_hint: Option<String>,
}

impl From<SessionAvailableCommand> for AvailableCommandResponse {
    fn from(command: SessionAvailableCommand) -> Self {
        Self {
            name: command.name,
            description: command.description,
            input_hint: command.input_hint,
        }
    }
}

pub(crate) struct StoredAvailableCommands {
    pub(crate) commands: Vec<SessionAvailableCommand>,
    pub(crate) updated_at: Option<String>,
}

/// Read the last agent-advertised command list off a session's metadata.
/// `None` means no list was ever stored; a malformed value degrades to `None`
/// with a warning instead of failing the caller — the reading routes must not
/// break because one agent wrote an unexpected payload.
pub(crate) fn stored_available_commands(metadata_json: &str) -> Option<StoredAvailableCommands> {
    let metadata = match serde_json::from_str::<serde_json::Value>(metadata_json) {
        Ok(value) => value,
        Err(err) => {
            tracing::warn!(error = %err, "session metadata_json is not valid JSON");
            return None;
        }
    };
    let commands = metadata.get(SESSION_METADATA_AVAILABLE_COMMANDS)?;
    let commands = match serde_json::from_value::<Vec<SessionAvailableCommand>>(commands.clone()) {
        Ok(commands) => commands,
        Err(err) => {
            tracing::warn!(
                error = %err,
                "stored session available_commands has an unexpected shape"
            );
            return None;
        }
    };
    let updated_at = metadata
        .get(SESSION_METADATA_AVAILABLE_COMMANDS_UPDATED_AT)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    Some(StoredAvailableCommands {
        commands,
        updated_at,
    })
}

#[derive(Deserialize, Default, schemars::JsonSchema)]
pub(crate) struct SessionsTargetParams {
    #[serde(default, alias = "target")]
    target_id: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct PromptStatusResponse {
    id: String,
    session_id: String,
    created_at: String,
    updated_at: String,
    #[schemars(extend("enum" = ["pending", "running", "completed", "errored", "cancelled", "stalled"]))]
    status: String,
    stop_reason: Option<String>,
    error_code: Option<String>,
    error_message: Option<String>,
    message_id: Option<String>,
    message_id_acknowledged: bool,
}

impl From<PromptRecord> for PromptStatusResponse {
    fn from(r: PromptRecord) -> Self {
        Self {
            id: r.id,
            session_id: r.session_id,
            created_at: r.created_at,
            updated_at: r.updated_at,
            status: r.status,
            stop_reason: r.stop_reason,
            error_code: r.error_code,
            error_message: r.error_message,
            message_id: r.message_id,
            message_id_acknowledged: r.message_id_acknowledged,
        }
    }
}

/// Look up a session's stored `target_id`, rejecting a mismatched
/// caller-asserted target. Shared by both the gated (driving) and wind-down
/// (terminal) resolvers below.
async fn resolved_stored_target_id(
    state: &AppState,
    session_id: &str,
    asserted_target_id: Option<&str>,
) -> Result<String> {
    let stored_target_id = {
        let store = state.state.lock().await;
        let record = store
            .get_session(session_id)?
            .ok_or_else(|| StackError::SessionNotFound {
                id: session_id.to_owned(),
            })?;
        record.target_id
    };
    if let Some(asserted) = asserted_target_id
        && asserted != stored_target_id
    {
        return Err(StackError::InvalidParam {
            field: "target",
            reason: format!(
                "session `{session_id}` belongs to target `{stored_target_id}`, not `{asserted}`"
            ),
        });
    }
    Ok(stored_target_id)
}

/// Resolve the supervisor for a driving op (prompt/load/resume/fork) against
/// an existing session. Honors Array-mode gating: a non-primary target is only
/// reachable while Array mode is enabled.
async fn target_for_existing_session(
    state: &AppState,
    session_id: &str,
    asserted_target_id: Option<&str>,
) -> Result<AgentTargetRuntime> {
    let stored_target_id = resolved_stored_target_id(state, session_id, asserted_target_id).await?;
    state.session_agent_target(Some(&stored_target_id)).await
}

/// Resolve the supervisor for a terminal wind-down op (cancel/close) against an
/// existing session. Reaches the stored target even when Array mode is off, so
/// an operator can always close or cancel a session that was opened against a
/// non-primary target before `acps array off`.
async fn target_for_session_wind_down(
    state: &AppState,
    session_id: &str,
    asserted_target_id: Option<&str>,
) -> Result<AgentTargetRuntime> {
    let stored_target_id = resolved_stored_target_id(state, session_id, asserted_target_id).await?;
    state.existing_session_target(&stored_target_id).await
}
