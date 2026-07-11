use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use tower_http::limit::RequestBodyLimitLayer;

use crate::api::routes::agent::{
    agent_capabilities_handler, array_agent_capabilities_handler, array_status_handler,
};
use crate::api::routes::commands::{
    commands_cancel_handler, commands_get_handler, commands_list_handler, commands_output_handler,
    commands_submit_handler,
};
use crate::api::routes::config::{config_export_handler, config_validate_handler};
use crate::api::routes::deps::{deps_check_handler, deps_get_handler};
use crate::api::routes::logs::{
    logs_commands_handler, logs_events_handler, logs_permissions_handler, logs_security_handler,
    logs_sessions_handler,
};
use crate::api::routes::metrics::metrics_summary_handler;
use crate::api::routes::permissions::{
    permissions_approve_handler, permissions_deny_handler, permissions_get_handler,
    permissions_pending_handler,
};
use crate::api::routes::providers::{models_handler, providers_handler};
use crate::api::routes::security::security_check_handler;
use crate::api::routes::sessions::{
    sessions_cancel_handler, sessions_changes_handler, sessions_close_handler,
    sessions_create_handler, sessions_events_handler, sessions_fork_handler, sessions_get_handler,
    sessions_list_handler, sessions_load_handler, sessions_prompt_handler,
    sessions_prompt_status_handler, sessions_resume_handler, sessions_snapshot_handler,
    sessions_status_handler,
};
use crate::api::routes::status::{
    health_live_handler, health_ready_handler, status_agent_handler, status_connections_handler,
    status_handler,
};
use crate::api::routes::workspace::{
    files_content_get_handler, files_content_put_handler, files_delete_handler,
    files_download_handler, files_list_handler, files_upload_handler, workspace_metadata_handler,
};
use crate::api::routes::ws::{ws_connections_handler, ws_sessions_handler};
use crate::api::{self, AppState};
use crate::auth::KeyKind;
use crate::config::LocalSessionAuth;

/// Build the Axum router that the internal local socket serves. Low-risk
/// observability routes are always keyless. Session-tier HTTP routes are
/// mounted but return 404 unless `[local].session_auth = "keyless"` is active.
/// Admin routes and WebSocket upgrades are never mounted here.
pub fn build_local_router(state: AppState) -> Router {
    let limit = state.max_request_bytes;

    let routes = Router::new()
        .route("/v1/status", get(status_handler))
        .route("/v1/status/agent", get(status_agent_handler))
        .route("/v1/agent/status", get(status_agent_handler))
        .route("/v1/status/connections", get(status_connections_handler))
        .route("/v1/health/live", get(health_live_handler))
        .route("/v1/health/ready", get(health_ready_handler))
        .route("/v1/config/export", get(config_export_handler))
        .route("/v1/config/validate", post(config_validate_handler))
        .route("/v1/agent/capabilities", get(agent_capabilities_handler))
        .route("/v1/array/status", get(array_status_handler))
        .route(
            "/v1/array/targets/{target_id}/capabilities",
            get(array_agent_capabilities_handler),
        )
        .route("/v1/security/check", get(security_check_handler))
        .route("/v1/logs/events", get(logs_events_handler))
        .route("/v1/logs/commands", get(logs_commands_handler))
        .route("/v1/logs/permissions", get(logs_permissions_handler))
        .route("/v1/logs/security", get(logs_security_handler))
        .route("/v1/logs/sessions", get(logs_sessions_handler))
        .route("/v1/metrics/summary", get(metrics_summary_handler))
        .route(
            "/v1/sessions",
            get(sessions_list_handler).post(sessions_create_handler),
        )
        .route("/v1/sessions/-/status", get(sessions_status_handler))
        .route(
            "/v1/sessions/{id}",
            get(sessions_get_handler).delete(sessions_close_handler),
        )
        .route("/v1/sessions/{id}/load", post(sessions_load_handler))
        .route("/v1/sessions/{id}/resume", post(sessions_resume_handler))
        .route("/v1/sessions/{id}/fork", post(sessions_fork_handler))
        .route("/v1/sessions/{id}/prompt", post(sessions_prompt_handler))
        .route("/v1/sessions/{id}/cancel", post(sessions_cancel_handler))
        .route(
            "/v1/sessions/{id}/prompts/{prompt_id}",
            get(sessions_prompt_status_handler),
        )
        .route("/v1/sessions/{id}/events", get(sessions_events_handler))
        .route("/v1/sessions/{id}/changes", get(sessions_changes_handler))
        .route("/v1/sessions/{id}/snapshot", get(sessions_snapshot_handler))
        .route("/v1/workspace", get(workspace_metadata_handler))
        .route(
            "/v1/files",
            get(files_list_handler).delete(files_delete_handler),
        )
        .route(
            "/v1/files/content",
            get(files_content_get_handler).put(files_content_put_handler),
        )
        .route("/v1/files/upload", post(files_upload_handler))
        .route("/v1/files/download", get(files_download_handler))
        .route(
            "/v1/commands",
            get(commands_list_handler).post(commands_submit_handler),
        )
        .route("/v1/commands/{id}", get(commands_get_handler))
        .route("/v1/commands/{id}/output", get(commands_output_handler))
        .route("/v1/commands/{id}/cancel", post(commands_cancel_handler))
        .route("/v1/deps", get(deps_get_handler))
        .route("/v1/deps/check", post(deps_check_handler))
        .route("/v1/providers", get(providers_handler))
        .route("/v1/models", get(models_handler))
        .route("/v1/permissions/pending", get(permissions_pending_handler))
        .route("/v1/permissions/{id}", get(permissions_get_handler))
        .route(
            "/v1/permissions/{id}/approve",
            post(permissions_approve_handler),
        )
        .route("/v1/permissions/{id}/deny", post(permissions_deny_handler))
        .route("/v1/ws/connections", get(ws_connections_handler))
        .route("/v1/ws/sessions", get(ws_sessions_handler))
        .layer(RequestBodyLimitLayer::new(limit))
        .layer(axum::extract::DefaultBodyLimit::disable())
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            enforce_local_session_access,
        ));

    // Layer ordering matters here. In axum, each `.layer` call wraps further
    // out: the LAST layer added sees requests first and responses last. We want
    // `tag_local` outermost so the `KeyKind::Local` extension is on the request
    // before `ensure_envelope` or `log_api_request` inspect it.
    Router::new()
        .merge(routes)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            api::log_api_request,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            api::track_active_requests,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            api::ensure_envelope,
        ))
        .layer(middleware::from_fn(tag_local))
        .with_state(state)
}

/// Stamp every UDS request with `KeyKind::Local` before any handler or
/// downstream middleware (incl. `log_api_request`) inspects extensions.
async fn tag_local(mut req: Request<Body>, next: Next) -> Response {
    req.extensions_mut().insert(KeyKind::Local);
    next.run(req).await
}

async fn enforce_local_session_access(
    axum::extract::State(state): axum::extract::State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    if is_always_keyless_local_route(req.method(), req.uri().path())
        || state.local_session_auth().await == LocalSessionAuth::Keyless
    {
        return next.run(req).await;
    }
    StatusCode::NOT_FOUND.into_response()
}

fn is_always_keyless_local_route(method: &Method, path: &str) -> bool {
    if *method != Method::GET {
        return false;
    }
    matches!(
        path,
        "/v1/status"
            | "/v1/status/agent"
            | "/v1/agent/status"
            | "/v1/status/connections"
            | "/v1/health/live"
            | "/v1/health/ready"
            | "/v1/security/check"
            | "/v1/metrics/summary"
            | "/v1/sessions"
            | "/v1/sessions/-/status"
            | "/v1/ws/connections"
            | "/v1/ws/sessions"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt;

    use crate::config::Config;
    use crate::state::StateStore;

    fn test_state() -> AppState {
        test_state_with_session_auth(LocalSessionAuth::SessionKey)
    }

    fn test_state_with_session_auth(session_auth: LocalSessionAuth) -> AppState {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let state_path = tempdir.path().join("state.sqlite");
        let store = StateStore::open(&state_path).expect("state open");
        store.migrate().expect("migrate");
        std::mem::forget(tempdir);
        let mut config = test_config();
        config.local.session_auth = session_auth;
        AppState::new(
            config,
            store,
            "acps_session_local_router".to_owned(),
            "acps_admin_local_router".to_owned(),
        )
    }

    fn test_config() -> Config {
        let toml_text = include_str!("../../tests/fixtures/valid-opencode-stack.toml");
        crate::config::load_config_from_str(toml_text).expect("sample config parses")
    }

    async fn status_for(app: Router, method: Method, uri: &str) -> StatusCode {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
            .expect("request");
        app.oneshot(request).await.expect("response").status()
    }

    #[tokio::test]
    async fn local_router_exposes_keyless_low_risk_views() {
        let app = build_local_router(test_state());
        for (method, uri) in [
            (Method::GET, "/v1/status"),
            (Method::GET, "/v1/status/agent"),
            (Method::GET, "/v1/status/connections"),
            (Method::GET, "/v1/health/live"),
            (Method::GET, "/v1/health/ready"),
            (Method::GET, "/v1/security/check"),
            (Method::GET, "/v1/metrics/summary"),
            (Method::GET, "/v1/sessions"),
            (Method::GET, "/v1/sessions/-/status"),
            (Method::GET, "/v1/ws/connections"),
            (Method::GET, "/v1/ws/sessions"),
        ] {
            let status = status_for(app.clone(), method.clone(), uri).await;
            assert!(
                !matches!(
                    status,
                    StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
                ),
                "{method} {uri} returned {status}"
            );
        }
    }

    #[tokio::test]
    async fn local_router_rejects_mutating_and_secret_bearing_routes() {
        let app = build_local_router(test_state());
        for (method, uri) in [
            (Method::GET, "/v1/config/export"),
            (Method::POST, "/v1/config/validate"),
            (Method::GET, "/v1/logs/events"),
            (Method::GET, "/v1/files?path=."),
            (Method::PUT, "/v1/files/content"),
            (Method::GET, "/v1/commands"),
            (Method::POST, "/v1/commands"),
            (Method::POST, "/v1/commands/cmd_1/cancel"),
            (Method::GET, "/v1/permissions/pending"),
            (Method::POST, "/v1/deps/check"),
            (Method::POST, "/v1/sessions"),
            (Method::POST, "/v1/sessions/session_1/prompt"),
            (Method::POST, "/v1/auth/session-key/regenerate"),
            (Method::GET, "/v1/secrets"),
            (Method::POST, "/v1/config/import"),
            (Method::POST, "/v1/deps/apply"),
            (Method::POST, "/v1/ws/connections/disconnect"),
            (Method::POST, "/v1/agent/start"),
            (Method::POST, "/v1/permissions/perm_1/approve"),
        ] {
            let status = status_for(app.clone(), method.clone(), uri).await;
            assert!(
                matches!(
                    status,
                    StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
                ),
                "{method} {uri} returned {status}"
            );
        }
    }

    #[tokio::test]
    async fn local_router_exposes_session_tier_http_routes_when_keyless_enabled() {
        let app = build_local_router(test_state_with_session_auth(LocalSessionAuth::Keyless));
        for (method, uri) in [
            (Method::GET, "/v1/config/export"),
            (Method::GET, "/v1/logs/events"),
            (Method::POST, "/v1/deps/check"),
            (Method::GET, "/v1/workspace"),
            (Method::GET, "/v1/permissions/pending"),
            (Method::POST, "/v1/sessions"),
        ] {
            let status = status_for(app.clone(), method.clone(), uri).await;
            assert!(
                !matches!(
                    status,
                    StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
                ),
                "{method} {uri} returned {status}"
            );
        }
    }

    #[tokio::test]
    async fn local_router_keeps_admin_routes_unavailable_when_keyless_enabled() {
        let app = build_local_router(test_state_with_session_auth(LocalSessionAuth::Keyless));
        for (method, uri) in [
            (Method::POST, "/v1/auth/session-key/regenerate"),
            (Method::GET, "/v1/secrets"),
            (Method::POST, "/v1/config/import"),
            (Method::POST, "/v1/deps/apply"),
            (Method::POST, "/v1/ws/connections/disconnect"),
            (Method::POST, "/v1/agent/start"),
        ] {
            let status = status_for(app.clone(), method.clone(), uri).await;
            assert!(
                matches!(
                    status,
                    StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
                ),
                "{method} {uri} returned {status}"
            );
        }
    }
}
