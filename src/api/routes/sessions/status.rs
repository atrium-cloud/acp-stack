use super::*;

#[derive(Deserialize, schemars::JsonSchema)]
pub(crate) struct SessionsStatusParams {
    #[serde(default = "default_session_status_threshold")]
    threshold: String,
    #[serde(default = "default_session_status_window")]
    window: String,
    #[serde(default = "default_session_status_limit")]
    limit: u32,
    #[serde(default, alias = "target")]
    target_id: Option<String>,
    /// Read the direct subagents of this session instead of the sessions that are
    /// no subagent's.
    parent_session_id: Option<String>,
}

fn default_session_status_threshold() -> String {
    DEFAULT_SESSION_ACTIVITY_THRESHOLD.to_owned()
}

fn default_session_status_window() -> String {
    DEFAULT_SESSION_STATUS_WINDOW.to_owned()
}

fn default_session_status_limit() -> u32 {
    MAX_LOGS_LIMIT
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct SessionsStatusResponse {
    generated_at: String,
    threshold: String,
    window: String,
    window_start: String,
    window_end: String,
    session_count: usize,
    active_count: usize,
    truncated: bool,
    sessions: Vec<SessionStatusSessionResponse>,
    /// The change-feed epoch the rows' `change_seq` values belong to, as on
    /// `GET /v1/sessions/-/changes`.
    feed_epoch: String,
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct SessionStatusSessionResponse {
    id: String,
    /// Derived per request from the durable row plus in-flight prompt and
    /// permission state; a wider set than the durable `status`. A subagent's
    /// derives from the work state its agent last reported.
    #[schemars(extend("enum" = ["closed", "permission_required", "idle", "working", "prompt_sent", "done", "stopped", "error", "cancelled", "unknown"]))]
    state: &'static str,
    /// Durable session status as stored.
    #[schemars(extend("enum" = ["active", "available", "closed"]))]
    status: String,
    agent_id: String,
    cwd: String,
    title: Option<String>,
    last_activity_at: String,
    last_activity_from: String,
    recent: bool,
    prompt: Option<SessionStatusPromptResponse>,
    permission: Option<SessionStatusPermissionResponse>,
    prompt_stream_started_at: Option<String>,
    /// Position of the session's latest change in
    /// `GET /v1/sessions/-/changes`.
    change_seq: u64,
    /// `seq` of the session's newest event, `0` when it has none.
    event_seq: u64,
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct SessionStatusPromptResponse {
    id: String,
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

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct SessionStatusPermissionResponse {
    id: String,
    created_at: String,
    updated_at: String,
    /// The session that raised the request: this row's, or on a root one of its
    /// subagents'.
    session_id: String,
}

impl SessionStatusSessionResponse {
    fn from_record(
        record: SessionStatusRecord,
        cutoff: chrono::DateTime<Utc>,
    ) -> std::result::Result<Self, StackError> {
        let last_activity = chrono::DateTime::parse_from_rfc3339(&record.last_activity_at)
            .map_err(|err| StackError::InvalidParam {
                field: "last_activity_at",
                reason: format!("stored session activity timestamp is invalid: {err}"),
            })?
            .with_timezone(&Utc);
        let state = derived_session_state(&record);
        let prompt = record
            .latest_prompt
            .map(|prompt| SessionStatusPromptResponse {
                id: prompt.id,
                created_at: prompt.created_at,
                updated_at: prompt.updated_at,
                status: prompt.status,
                stop_reason: prompt.stop_reason,
                error_code: prompt.error_code,
                error_message: prompt.error_message,
                message_id: prompt.message_id,
                message_id_acknowledged: prompt.message_id_acknowledged,
            });
        let permission =
            record
                .pending_permission
                .map(|permission| SessionStatusPermissionResponse {
                    id: permission.id,
                    created_at: permission.created_at,
                    updated_at: permission.updated_at,
                    session_id: permission.session_id,
                });
        Ok(Self {
            id: record.id,
            state,
            status: record.status,
            agent_id: record.agent_id,
            cwd: record.cwd,
            title: record.title,
            last_activity_at: record.last_activity_at,
            last_activity_from: record.last_activity_from,
            recent: last_activity >= cutoff,
            prompt,
            permission,
            prompt_stream_started_at: record.prompt_stream_started_at,
            change_seq: record.change_seq,
            event_seq: record.event_seq,
        })
    }
}

fn derived_session_state(record: &SessionStatusRecord) -> &'static str {
    // `available` falls through to the activity-derived state: idle demotion
    // parks recently worked sessions there, and collapsing them to a bare
    // "available" would hide `done`/`error` detail the durable `status`
    // field already conveys.
    if record.status.as_str() == SESSION_STATUS_CLOSED {
        return "closed";
    }
    if record.pending_permission.is_some() {
        return "permission_required";
    }
    if let Some(subagent) = record.subagent.as_ref() {
        return subagent_state(subagent);
    }
    let Some(prompt) = record.latest_prompt.as_ref() else {
        return "idle";
    };
    match prompt.status.as_str() {
        "pending" | "running" => {
            if record.prompt_stream_started_at.is_some() {
                "working"
            } else {
                "prompt_sent"
            }
        }
        "completed" if prompt.stop_reason.as_deref() == Some("end_turn") => "done",
        "completed" => "stopped",
        "errored" | "stalled" => "error",
        "cancelled" => "cancelled",
        _ => "idle",
    }
}

/// A subagent has no prompts of its own: its state is the ACP work state its agent
/// last reported, and `unknown` once that report no longer holds for the live agent.
fn subagent_state(subagent: &SubagentStatusRecord) -> &'static str {
    let reported = subagent
        .state_json
        .as_deref()
        .filter(|_| subagent.state_current)
        .and_then(|state_json| serde_json::from_str::<serde_json::Value>(state_json).ok());
    let Some(reported) = reported else {
        return "unknown";
    };
    match (reported["state"].as_str(), reported["stopReason"].as_str()) {
        (Some("running"), _) => "working",
        (Some("requires_action"), _) => "permission_required",
        (Some("idle"), Some("end_turn")) => "done",
        (Some("idle"), Some("cancelled")) => "cancelled",
        (Some("idle"), Some("error")) => "error",
        (Some("idle"), Some(_)) => "stopped",
        (Some("idle"), None) => "idle",
        _ => "unknown",
    }
}

pub(crate) async fn sessions_status_handler(
    Query(params): Query<SessionsStatusParams>,
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<SessionsStatusResponse>, StackError> {
    let threshold =
        crate::time_util::parse_duration_suffix(&params.threshold).ok_or_else(|| {
            StackError::InvalidParam {
                field: "threshold",
                reason: format!(
                    "not a valid duration; expected values like `{}` or `30m`",
                    DEFAULT_SESSION_ACTIVITY_THRESHOLD
                ),
            }
        })?;
    let window = parse_session_status_window(&params.window)?;
    let generated_at = Utc::now();
    let cutoff = generated_at - threshold;
    let window_start = generated_at
        .checked_sub_signed(window)
        .ok_or(StackError::InvalidParam {
            field: "window",
            reason: "duration window underflowed the timestamp range".to_owned(),
        })?;
    let window_start_text = window_start.to_rfc3339_opts(SecondsFormat::Nanos, true);
    let limit = params.limit.min(MAX_LOGS_LIMIT);
    let query_limit = limit.saturating_add(1);
    let target = state
        .session_agent_target(params.target_id.as_deref())
        .await?;
    let status_window_start = window_start_text.clone();
    let status_target_id = target.target_id.clone();
    let parent_session_id = params.parent_session_id.clone();
    let mut rows = state
        .state
        .read(move |store| {
            store.query_session_status_window(
                &status_window_start,
                Some(&status_target_id),
                parent_session_id.as_deref(),
                query_limit,
            )
        })
        .await?;
    let feed_epoch = state.state.feed_epoch().to_owned();
    let truncated = rows.len() > limit as usize;
    if truncated {
        rows.truncate(limit as usize);
    }
    let active_count = rows
        .iter()
        .filter(|row| row.status == SESSION_STATUS_ACTIVE)
        .count();
    let sessions = rows
        .into_iter()
        .map(|row| SessionStatusSessionResponse::from_record(row, cutoff))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(ApiSuccess::new(SessionsStatusResponse {
        generated_at: generated_at.to_rfc3339_opts(SecondsFormat::Nanos, true),
        threshold: params.threshold,
        window: params.window,
        window_start: window_start_text,
        window_end: generated_at.to_rfc3339_opts(SecondsFormat::Nanos, true),
        session_count: sessions.len(),
        active_count,
        truncated,
        sessions,
        feed_epoch,
    }))
}

fn parse_session_status_window(raw: &str) -> Result<chrono::Duration> {
    let duration =
        crate::time_util::parse_duration_suffix(raw).ok_or_else(|| StackError::InvalidParam {
            field: "window",
            reason: format!("not a valid duration; expected values between 1m and 999h, got {raw}"),
        })?;
    let seconds = duration.num_seconds();
    if !(MIN_SESSION_STATUS_WINDOW_SECS..=MAX_SESSION_STATUS_WINDOW_SECS).contains(&seconds) {
        return Err(StackError::InvalidParam {
            field: "window",
            reason: format!("duration must be between 1m and 999h inclusive, got {raw}"),
        });
    }
    Ok(duration)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_subagent_state_follows_the_work_state_its_agent_reported() {
        for (state_json, state_current, expected) in [
            (Some(r#"{"state":"running"}"#), true, "working"),
            (
                Some(r#"{"state":"requires_action"}"#),
                true,
                "permission_required",
            ),
            (
                Some(r#"{"state":"idle","stopReason":"end_turn"}"#),
                true,
                "done",
            ),
            (
                Some(r#"{"state":"idle","stopReason":"cancelled"}"#),
                true,
                "cancelled",
            ),
            (
                Some(r#"{"state":"idle","stopReason":"error"}"#),
                true,
                "error",
            ),
            (
                Some(r#"{"state":"idle","stopReason":"refusal"}"#),
                true,
                "stopped",
            ),
            (Some(r#"{"state":"idle"}"#), true, "idle"),
            (Some(r#"{"state":"unknown"}"#), true, "unknown"),
            (Some(r#"{"state":"running"}"#), false, "unknown"),
            (None, false, "unknown"),
        ] {
            let subagent = SubagentStatusRecord {
                state_json: state_json.map(str::to_owned),
                state_current,
            };
            assert_eq!(subagent_state(&subagent), expected, "{state_json:?}");
        }
    }
}
