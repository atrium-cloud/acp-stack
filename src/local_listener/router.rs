use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, post};
use tower_http::limit::RequestBodyLimitLayer;

use crate::api::{
    self, AppState, commands_cancel_handler, commands_get_handler, commands_list_handler,
    commands_output_handler, commands_submit_handler, config_export_handler, deps_check_handler,
    files_content_get_handler, files_content_put_handler, files_list_handler, logs_events_handler,
    permissions_pending_handler, security_check_handler, status_handler, ws_connections_handler,
    ws_sessions_handler,
};
use crate::auth::KeyKind;

/// Build the Axum router that the UDS listener serves. Mounts only the
/// allowlisted operations exposed to `acpctl`. Anything else over the UDS
/// returns 404 from the framework fallback, rewrapped by `ensure_envelope`.
pub fn build_local_router(state: AppState) -> Router {
    let limit = state.max_request_bytes;

    let routes = Router::new()
        .route("/v1/status", get(status_handler))
        .route("/v1/security/check", get(security_check_handler))
        .route("/v1/deps/check", post(deps_check_handler))
        .route("/v1/logs/events", get(logs_events_handler))
        .route("/v1/files", get(files_list_handler))
        .route(
            "/v1/files/content",
            get(files_content_get_handler).put(files_content_put_handler),
        )
        .route(
            "/v1/commands",
            get(commands_list_handler).post(commands_submit_handler),
        )
        .route("/v1/commands/{id}", get(commands_get_handler))
        .route("/v1/commands/{id}/output", get(commands_output_handler))
        .route("/v1/commands/{id}/cancel", post(commands_cancel_handler))
        .route("/v1/config/export", get(config_export_handler))
        .route("/v1/permissions/pending", get(permissions_pending_handler))
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
