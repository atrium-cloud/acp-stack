use super::*;

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct SessionsListResponse {
    sessions: Vec<SessionResponse>,
    agent_sync: SessionsAgentSyncResponse,
    /// The change-feed epoch the sessions' `change_seq` values belong to, as
    /// on `GET /v1/sessions/-/changes`.
    feed_epoch: String,
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct SessionsAgentSyncResponse {
    attempted: bool,
    #[schemars(extend("enum" = ["synced", "unsupported", "not_running"]))]
    status: String,
    upserted: u32,
    updated: u32,
}

impl From<SessionListSyncResult> for SessionsAgentSyncResponse {
    fn from(result: SessionListSyncResult) -> Self {
        Self {
            attempted: result.attempted,
            status: result.status.as_str().to_owned(),
            upserted: result.upserted,
            updated: result.updated,
        }
    }
}

#[derive(Deserialize, Default, schemars::JsonSchema)]
pub(crate) struct SessionsListParams {
    /// Values above 1000 are silently clamped to 1000, not rejected.
    #[serde(default = "default_logs_limit")]
    limit: u32,
    since: Option<String>,
    until: Option<String>,
    range: Option<String>,
    #[serde(default)]
    resolve_bounds: bool,
    #[serde(default, alias = "target")]
    target_id: Option<String>,
}

pub(crate) async fn sessions_list_handler(
    Query(params): Query<SessionsListParams>,
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<SessionsListResponse>, StackError> {
    let limit = params.limit.min(MAX_LOGS_LIMIT);
    let now = Utc::now();
    let target = state
        .session_agent_target(params.target_id.as_deref())
        .await?;
    let agent_for_session = target.live_agent_config.lock().await.clone();
    let agent_sync = target
        .supervisor
        .sync_listed_sessions(
            &target.target_id,
            &agent_for_session,
            &state.config.workspace.root,
            &state.state,
        )
        .await?;
    let store = state.state.lock().await;
    let bounds = store.session_update_bounds()?;
    let (since, until) = resolve_session_list_bounds(&params, bounds.as_ref(), now)?;
    let sessions = store.query_sessions(crate::state::SessionFilter {
        limit,
        since: since.as_deref(),
        until: until.as_deref(),
        target_id: Some(&target.target_id),
        ..Default::default()
    })?;
    let feed_epoch = store.feed_epoch().to_owned();
    drop(store);
    Ok(ApiSuccess::new(SessionsListResponse {
        sessions: sessions.into_iter().map(SessionResponse::from).collect(),
        agent_sync: agent_sync.into(),
        feed_epoch,
    }))
}

/// Smallest change-feed page. An empty page reads as "holds every change
/// through `head`", so a zero limit would hand a reader a cursor past changes
/// it never saw.
const MIN_CHANGE_FEED_LIMIT: u32 = 1;

#[derive(Deserialize, Default, schemars::JsonSchema)]
pub(crate) struct SessionsChangeFeedParams {
    /// Change position to read after; `0` reads from the start.
    #[serde(default)]
    after: u64,
    /// Clamped to 1 through 1000, not rejected.
    #[serde(default = "default_logs_limit")]
    limit: u32,
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct SessionsChangeFeedResponse {
    /// Ascending by `change_seq`, one entry per session: its latest change.
    changes: Vec<SessionChangeResponse>,
    /// The last `change_seq` issued. A page shorter than `limit` holds every
    /// change through `head`.
    head: u64,
    /// The highest `change_seq` of a pruned deletion tombstone. A cursor
    /// below it may have missed a deletion, so the reader re-reads from `0`.
    pruned_through: u64,
    /// Minted each time the runtime process starts. A reader holding a
    /// cursor from another epoch re-reads from `0`.
    feed_epoch: String,
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct SessionChangeResponse {
    session_id: String,
    change_seq: u64,
    deleted: bool,
}

pub(crate) async fn sessions_change_feed_handler(
    Query(params): Query<SessionsChangeFeedParams>,
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<SessionsChangeFeedResponse>, StackError> {
    let limit = params.limit.clamp(MIN_CHANGE_FEED_LIMIT, MAX_LOGS_LIMIT);
    let store = state.state.lock().await;
    let page = store.query_session_changes(params.after, limit)?;
    drop(store);
    Ok(ApiSuccess::new(SessionsChangeFeedResponse {
        changes: page
            .changes
            .into_iter()
            .map(|change| SessionChangeResponse {
                session_id: change.session_id,
                change_seq: change.change_seq,
                deleted: change.deleted,
            })
            .collect(),
        head: page.head,
        pruned_through: page.pruned_through,
        feed_epoch: page.feed_epoch,
    }))
}

fn resolve_session_list_bounds(
    params: &SessionsListParams,
    bounds: Option<&SessionUpdateBounds>,
    now: chrono::DateTime<Utc>,
) -> Result<(Option<String>, Option<String>)> {
    let until = match params.until.as_deref() {
        Some(raw) => resolve_time_bound(Some(raw), "until", now)?,
        None if params.resolve_bounds => default_until_bound(bounds, now)?,
        None => None,
    };
    let since = match params.since.as_deref() {
        Some(raw) => resolve_time_bound(Some(raw), "since", now)?,
        None if params.resolve_bounds => bounds.map(|b| b.first_updated_at.clone()),
        None => params
            .range
            .as_deref()
            .map(|range| resolve_range_start(range, now))
            .transpose()?
            .flatten(),
    };
    Ok((since, until))
}

fn default_until_bound(
    bounds: Option<&SessionUpdateBounds>,
    now: chrono::DateTime<Utc>,
) -> Result<Option<String>> {
    let Some(bounds) = bounds else {
        return Ok(None);
    };
    if bounds.latest_status == SESSION_STATUS_ACTIVE {
        return Ok(Some(now.to_rfc3339_opts(SecondsFormat::Nanos, true)));
    }
    let latest = parse_normalized_time_bound(&bounds.latest_updated_at, "latest_updated_at")?;
    Ok(Some(
        (latest + chrono::Duration::nanoseconds(1)).to_rfc3339_opts(SecondsFormat::Nanos, true),
    ))
}

fn resolve_range_start(raw: &str, now: chrono::DateTime<Utc>) -> Result<Option<String>> {
    if raw == "all" {
        return Ok(None);
    }
    let duration = session_range_duration(raw).ok_or_else(|| StackError::InvalidParam {
        field: "range",
        reason: format!(
            "expected day, week, month, year, all, or a duration like 30m, 60d, 6mo, or 1y; got {raw}"
        ),
    })?;
    Ok(Some(resolve_duration_start(duration, "range", now)?))
}

fn session_range_duration(raw: &str) -> Option<chrono::Duration> {
    match raw {
        "day" => Some(chrono::Duration::days(1)),
        "week" => Some(chrono::Duration::weeks(1)),
        "month" => Some(chrono::Duration::days(30)),
        "year" => Some(chrono::Duration::days(365)),
        other => crate::time_util::parse_coarse_duration_suffix(other),
    }
}

fn parse_normalized_time_bound(raw: &str, field: &'static str) -> Result<chrono::DateTime<Utc>> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|err| StackError::InvalidParam {
            field,
            reason: format!("not a valid RFC3339 timestamp: {err}"),
        })
}

fn resolve_time_bound(
    raw: Option<&str>,
    field: &'static str,
    now: chrono::DateTime<Utc>,
) -> Result<Option<String>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Ok(Some(
            dt.with_timezone(&Utc)
                .to_rfc3339_opts(SecondsFormat::Nanos, true),
        ));
    }
    let duration = crate::time_util::parse_coarse_duration_suffix(raw).ok_or_else(|| {
        StackError::InvalidParam {
            field,
            reason: format!("not a valid RFC3339 timestamp or duration (m, h, d, w, mo, y): {raw}"),
        }
    })?;
    Ok(Some(resolve_duration_start(duration, field, now)?))
}

/// Anchor a relative duration to `now` and render the resulting lower bound as
/// a normalized RFC 3339 timestamp. Shared by the `range` and `since`/`until`
/// paths, which reject pre-epoch windows identically.
fn resolve_duration_start(
    duration: chrono::Duration,
    field: &'static str,
    now: chrono::DateTime<Utc>,
) -> Result<String> {
    let resolved =
        crate::time_util::resolve_since_after_unix_epoch(duration, now).ok_or_else(|| {
            StackError::InvalidParam {
                field,
                reason: "duration range must not begin before 1970-01-01T00:00:00Z".to_owned(),
            }
        })?;
    Ok(resolved.to_rfc3339_opts(SecondsFormat::Nanos, true))
}
