use std::sync::atomic::Ordering;

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use http::StatusCode;
use serde::Serialize;

use super::super::core::AppState;
use super::logs::default_logs_limit;
use crate::config::AgentAdapterConfig;
use crate::envelope::ApiSuccess;
use crate::error::StackError;
use crate::runtime::agent::provider_keys::ResolvedProviderSnapshot;
use crate::runtime::agent::supervisor::AgentStateLabel;
use crate::runtime::health::HealthReport;

use super::agent::open_agent_environment;

#[derive(Serialize)]
pub(crate) struct StatusResponse {
    schema_version: i64,
    latest_event: Option<String>,
    server: ServerInfo,
}

#[derive(Serialize)]
struct ServerInfo {
    version: &'static str,
}

pub(crate) async fn status_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<StatusResponse>, StackError> {
    let store = state.state.lock().await;
    let schema_version = store.schema_version()?;
    let latest_event = store.latest_event_timestamp()?;
    drop(store);
    Ok(ApiSuccess::new(StatusResponse {
        schema_version,
        latest_event,
        server: ServerInfo {
            version: env!("CARGO_PKG_VERSION"),
        },
    }))
}

#[derive(Serialize)]
pub(crate) struct StatusAgentResponse {
    configured: bool,
    agent: AgentStatusJson,
    process_state: String,
    pid: Option<u32>,
    latest_failure: Option<AgentFailureJson>,
    lifecycle_events: Vec<AgentLifecycleJson>,
    configured_providers: Vec<ProviderStatusJson>,
    loaded_providers: Option<Vec<ProviderStatusJson>>,
    provider_restart_required: bool,
    /// Populated when configured-provider resolution fails (missing/unselected
    /// credential, corrupt secret). Carries a remote-safe message so the
    /// monitoring endpoint stays reachable in exactly the broken state an
    /// operator queries it to diagnose. `None` on the happy path.
    provider_error: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct ProviderStatusJson {
    provider_id: String,
    alias: Option<String>,
    env_names: Vec<String>,
}

impl From<&ResolvedProviderSnapshot> for ProviderStatusJson {
    fn from(provider: &ResolvedProviderSnapshot) -> Self {
        Self {
            provider_id: provider.provider_id.clone(),
            alias: provider.alias.clone(),
            env_names: provider.env_names.clone(),
        }
    }
}

pub(crate) fn provider_snapshot_requires_restart(
    state: AgentStateLabel,
    loaded: Option<&[ResolvedProviderSnapshot]>,
    configured: &[ResolvedProviderSnapshot],
) -> bool {
    state == AgentStateLabel::Running && loaded != Some(configured)
}

/// Map a provider-resolution result into the status view. On failure the
/// monitoring endpoint degrades instead of erroring: `configured_providers`
/// is emptied and a remote-safe `provider_error` is surfaced so operators can
/// still read live state (and, for the array route, other targets) while a
/// credential is broken.
pub(crate) fn configured_providers_or_error(
    environment: std::result::Result<
        crate::runtime::agent::provider_keys::ResolvedAgentEnvironment,
        StackError,
    >,
) -> (Vec<ResolvedProviderSnapshot>, Option<String>) {
    match environment {
        Ok(environment) => (environment.providers, None),
        Err(error) => (Vec::new(), Some(error.public_message())),
    }
}

/// Restart-required signal for the status view. When resolution failed
/// (`resolution_failed`) the configured set is unknown, so a running agent
/// would otherwise read as "restart required" against its still-loaded
/// snapshot — report false instead, since the signal is unknowable until the
/// credential is fixed.
pub(crate) fn provider_restart_required_for_status(
    resolution_failed: bool,
    state: AgentStateLabel,
    loaded: Option<&[ResolvedProviderSnapshot]>,
    configured: &[ResolvedProviderSnapshot],
) -> bool {
    !resolution_failed && provider_snapshot_requires_restart(state, loaded, configured)
}

#[derive(Serialize)]
struct AgentStatusJson {
    id: String,
    name: String,
    command: String,
    args: Vec<String>,
    cwd: Option<String>,
    restart: String,
    adapter: Option<AgentAdapterConfig>,
}

#[derive(Serialize)]
struct AgentLifecycleJson {
    id: String,
    created_at: String,
    event_kind: String,
    message: String,
    payload_json: String,
}

#[derive(Serialize)]
struct AgentFailureJson {
    id: String,
    created_at: String,
    event_kind: String,
    message: String,
    reason: String,
}

pub(crate) async fn status_agent_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<StatusAgentResponse>, StackError> {
    let config = state.refresh_array_runtime_from_disk().await?;
    let target_id = config.array.primary_target.clone();
    let target = state.agent_target(&target_id)?;
    let snapshot = target.supervisor.snapshot().await;
    let mut target_config = config.clone();
    target_config.agent = config
        .array
        .target(&target_id)
        .ok_or_else(|| StackError::InvalidParam {
            field: "target",
            reason: format!("unknown Array target `{target_id}`"),
        })?
        .agent
        .clone();
    let (configured_provider_snapshot, provider_error) =
        configured_providers_or_error(open_agent_environment(&target_config));
    let provider_restart_required = provider_restart_required_for_status(
        provider_error.is_some(),
        snapshot.state,
        snapshot.loaded_providers.as_deref(),
        &configured_provider_snapshot,
    );
    let agent = target.live_agent_config.lock().await.clone();
    let store = state.state.lock().await;
    let lifecycle_events = store.query_agent_lifecycle(default_logs_limit())?;
    let latest_failure = store.latest_agent_failure(&agent.id)?;
    drop(store);
    Ok(ApiSuccess::new(StatusAgentResponse {
        configured: true,
        agent: AgentStatusJson {
            id: agent.id.clone(),
            name: agent.name.clone(),
            command: agent.command.clone(),
            args: agent.args.clone(),
            cwd: agent.cwd.clone(),
            restart: agent.restart.clone(),
            adapter: agent.adapter.clone(),
        },
        process_state: snapshot.state.as_wire_str().to_owned(),
        pid: snapshot.pid,
        latest_failure: latest_failure.map(|failure| AgentFailureJson {
            id: failure.id,
            created_at: failure.created_at,
            event_kind: failure.event_kind,
            message: failure.message,
            reason: failure.reason,
        }),
        lifecycle_events: lifecycle_events
            .into_iter()
            .map(|event| AgentLifecycleJson {
                id: event.id,
                created_at: event.created_at,
                event_kind: event.event_kind,
                message: event.message,
                payload_json: event.payload_json,
            })
            .collect(),
        configured_providers: configured_provider_snapshot
            .iter()
            .map(ProviderStatusJson::from)
            .collect(),
        loaded_providers: snapshot
            .loaded_providers
            .as_ref()
            .map(|providers| providers.iter().map(ProviderStatusJson::from).collect()),
        provider_restart_required,
        provider_error,
    }))
}

#[derive(Serialize)]
pub(crate) struct StatusConnectionsResponse {
    active_requests: u64,
}

pub(crate) async fn status_connections_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<StatusConnectionsResponse>, StackError> {
    Ok(ApiSuccess::new(StatusConnectionsResponse {
        active_requests: state.active_requests.load(Ordering::Relaxed),
    }))
}

#[derive(Serialize)]
pub(crate) struct HealthLiveResponse {
    ok: bool,
    server: ServerInfo,
}

/// `GET /v1/health/live` — session-authenticated liveness. Once the daemon is
/// accepting authenticated requests, this answers "is the process alive and the
/// router up?" without touching SQLite, the supervisor, or the workspace.
/// Readers that want subsystem detail should call `/v1/health/ready`.
pub(crate) async fn health_live_handler() -> ApiSuccess<HealthLiveResponse> {
    ApiSuccess::new(HealthLiveResponse {
        ok: true,
        server: ServerInfo {
            version: env!("CARGO_PKG_VERSION"),
        },
    })
}

/// `GET /v1/health/ready` — collects a fresh `HealthReport` and returns 200
/// when every subsystem is ok, otherwise 503 with the same body shape so
/// callers can pull the `failing` list and per-subsystem detail from a single
/// schema regardless of status code.
///
/// The envelope's top-level `ok` mirrors `report.ok` (not always `true`) so
/// the 503 case follows the envelope convention from `docs/specs/api/api.md`
/// where successful responses use `ok: true` and failure responses use
/// `ok: false`. Clients that key off envelope.ok see the same yes/no signal
/// as the HTTP status code.
pub(crate) async fn health_ready_handler(State(state): State<AppState>) -> Response {
    let report = HealthReport::collect(&state).await;
    let status = if report.ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    let body = serde_json::json!({
        "ok": report.ok,
        "data": report,
    });
    (status, Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(alias: Option<&str>, revision: &str) -> ResolvedProviderSnapshot {
        ResolvedProviderSnapshot {
            provider_id: "opencode-go".to_owned(),
            alias: alias.map(str::to_owned),
            revision: Some(revision.to_owned()),
            env_names: vec!["OPENCODE_API_KEY".to_owned()],
        }
    }

    #[test]
    fn provider_restart_only_applies_to_changed_running_snapshots() {
        let loaded = vec![provider(Some("go_1"), "revision-1")];
        assert!(!provider_snapshot_requires_restart(
            AgentStateLabel::Stopped,
            None,
            &loaded,
        ));
        assert!(!provider_snapshot_requires_restart(
            AgentStateLabel::Running,
            Some(&loaded),
            &loaded,
        ));
        assert!(provider_snapshot_requires_restart(
            AgentStateLabel::Running,
            Some(&loaded),
            &[provider(Some("go_2"), "revision-2")],
        ));
        assert!(provider_snapshot_requires_restart(
            AgentStateLabel::Running,
            Some(&loaded),
            &[provider(Some("go_1"), "revision-2")],
        ));
        assert!(provider_snapshot_requires_restart(
            AgentStateLabel::Running,
            None,
            &loaded,
        ));
    }

    #[test]
    fn provider_status_json_excludes_internal_revision() {
        let serialized = serde_json::to_value(ProviderStatusJson::from(&provider(
            Some("go_2"),
            "private-revision",
        )))
        .expect("serialize");

        assert_eq!(serialized["provider_id"], "opencode-go");
        assert_eq!(serialized["alias"], "go_2");
        assert!(serialized.get("revision").is_none());
        assert!(!serialized.to_string().contains("private-revision"));
    }

    #[test]
    fn configured_providers_pass_through_on_success() {
        let environment = crate::runtime::agent::provider_keys::ResolvedAgentEnvironment {
            env: std::collections::HashMap::new(),
            providers: vec![provider(Some("go_1"), "revision-1")],
        };
        let (providers, error) = configured_providers_or_error(Ok(environment));
        assert_eq!(providers.len(), 1);
        assert!(error.is_none());
    }

    #[test]
    fn broken_credential_degrades_to_error_marker_without_erroring() {
        let (providers, error) = configured_providers_or_error(Err(StackError::InvalidParam {
            field: "agent.providers.selected_aliases",
            reason: "provider `openrouter` has backup aliases".to_owned(),
        }));
        assert!(providers.is_empty());
        assert!(error.is_some());
    }

    #[test]
    fn failed_resolution_never_reports_restart_required() {
        // A running agent with a loaded snapshot but empty (failed) configured
        // set must not read as needing a restart — the raw predicate says true,
        // the guarded status helper must say false.
        let loaded = vec![provider(Some("go_1"), "revision-1")];
        assert!(provider_snapshot_requires_restart(
            AgentStateLabel::Running,
            Some(&loaded),
            &[],
        ));
        assert!(!provider_restart_required_for_status(
            true,
            AgentStateLabel::Running,
            Some(&loaded),
            &[],
        ));
        // When resolution succeeds the guard defers to the raw predicate.
        assert!(provider_restart_required_for_status(
            false,
            AgentStateLabel::Running,
            Some(&loaded),
            &[],
        ));
    }
}
