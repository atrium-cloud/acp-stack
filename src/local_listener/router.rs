use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::get;
use tower_http::limit::RequestBodyLimitLayer;

use crate::api::{
    self, AppState, health_ready_handler, metrics_summary_handler, security_check_handler,
    sessions_list_handler, sessions_status_handler, status_agent_handler, status_handler,
    ws_connections_handler, ws_sessions_handler,
};
use crate::auth::KeyKind;

/// Build the Axum router that the internal local socket serves. Mounts only
/// low-risk observability operations used by keyless `acps` commands. Anything
/// else over the socket returns 404 from the framework fallback, rewrapped by
/// `ensure_envelope`.
pub fn build_local_router(state: AppState) -> Router {
    let limit = state.max_request_bytes;

    let routes = Router::new()
        .route("/v1/status", get(status_handler))
        .route("/v1/status/agent", get(status_agent_handler))
        .route("/v1/agent/status", get(status_agent_handler))
        .route("/v1/health/ready", get(health_ready_handler))
        .route("/v1/security/check", get(security_check_handler))
        .route("/v1/metrics/summary", get(metrics_summary_handler))
        .route("/v1/sessions", get(sessions_list_handler))
        .route("/v1/sessions/-/status", get(sessions_status_handler))
        .route("/v1/ws/connections", get(ws_connections_handler))
        .route("/v1/ws/sessions", get(ws_sessions_handler))
        .layer(RequestBodyLimitLayer::new(limit))
        .layer(axum::extract::DefaultBodyLimit::disable());

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

#[cfg(test)]
mod tests {
    use super::*;
    use http::{Method, StatusCode};
    use tower::ServiceExt;

    use crate::config::Config;
    use crate::state::StateStore;

    fn test_state() -> AppState {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let state_path = tempdir.path().join("state.sqlite");
        let store = StateStore::open(&state_path).expect("state open");
        store.migrate().expect("migrate");
        std::mem::forget(tempdir);
        AppState::new(
            test_config(),
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
            (Method::GET, "/v1/files?path=."),
            (Method::PUT, "/v1/files/content"),
            (Method::GET, "/v1/commands"),
            (Method::POST, "/v1/commands"),
            (Method::POST, "/v1/commands/cmd_1/cancel"),
            (Method::GET, "/v1/config/export"),
            (Method::GET, "/v1/permissions/pending"),
            (Method::POST, "/v1/deps/check"),
            (Method::GET, "/v1/logs/events"),
            (Method::POST, "/v1/sessions"),
            (Method::POST, "/v1/sessions/session_1/prompt"),
            (Method::POST, "/v1/auth/session-key/regenerate"),
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
