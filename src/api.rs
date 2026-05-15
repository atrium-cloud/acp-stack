//! HTTP API surface.
//!
//! The router exposes `/v1/*` routes behind a two-stage auth chain:
//!
//! 1. `authenticate` reads the Bearer token, constant-time compares against
//!    the cached session and admin keys, tags the request with the resolved
//!    `KeyKind`, and 401s on any failure (with a structured `auth_failures`
//!    row written through `record_auth_failure`).
//! 2. A per-route `require_tier` middleware reads the tag and rejects any
//!    request whose key tier does not match the route's required tier. Admin
//!    keys are NOT accepted on session-tier routes — strict tiering (see
//!    `docs/specs/security.md`).
//!
//! Body-size limits come from `min(api.max_request_bytes,
//! security.http.max_request_bytes)` and are enforced by tower-http before
//! any handler runs.
//!
//! Handlers return `Result<ApiSuccess<T>, StackError>`. `IntoResponse` on
//! `StackError` (see `envelope.rs`) maps each variant to its HTTP status and
//! a sanitized `ApiError` payload via `public_message()`, so local paths and
//! secret-store internals never leak to remote callers.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, Query, Request, State};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use chrono::{Duration, SecondsFormat, Utc};
use http::StatusCode;
use http::header::AUTHORIZATION;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::Mutex as TokioMutex;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;
use zeroize::Zeroizing;

use crate::acp_bridge::AgentCapabilitiesDto;
use crate::agent_installer::run_installer_capture;
use crate::auth::{AuthFailureReason, KeyKind, constant_time_eq, record_auth_failure};
use crate::config::Config;
use crate::envelope::{ApiError, ApiSuccess};
use crate::error::{Result, StackError};
use crate::events::EventHub;
use crate::fs_util::home_dir;
use crate::secrets::SecretStore;
use crate::state::InstallerRunInput;
use crate::state::{AuthFailureFilter, EventFilter, PromptRecord, SessionRecord, StateStore};
use crate::supervisor::{AgentSnapshot, AgentSupervisor, parse_mcp_servers, parse_prompt_blocks};

/// Shared handler/middleware state. Cheap to clone (Arc-only inside).
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub effective_bind: Arc<String>,
    pub state: Arc<TokioMutex<StateStore>>,
    pub session_key: Arc<Zeroizing<String>>,
    pub admin_key: Arc<Zeroizing<String>>,
    pub max_request_bytes: usize,
    pub active_requests: Arc<AtomicU64>,
    pub agent_supervisor: Arc<AgentSupervisor>,
    pub event_hub: EventHub,
}

impl AppState {
    /// Build app state from already-resolved config + state + auth secrets.
    /// `max_request_bytes` is the tighter of the two config caps; see the
    /// module-level doc.
    pub fn new(config: Config, state: StateStore, session_key: String, admin_key: String) -> Self {
        let effective_bind = config.api.bind.clone();
        Self::with_effective_bind(config, state, session_key, admin_key, effective_bind)
    }

    /// Build app state with the actual listener address. `acps serve --bind`
    /// can override config for one run; security checks must inspect the
    /// effective listener, while config export still returns the stored config.
    pub fn with_effective_bind(
        config: Config,
        state: StateStore,
        session_key: String,
        admin_key: String,
        effective_bind: String,
    ) -> Self {
        let api_cap = config.api.max_request_bytes;
        let security_cap = config.security.http.max_request_bytes;
        let cap = api_cap.min(security_cap);
        let event_hub = EventHub::new();
        // SQLite is local and `usize::MAX` covers any byte count we'd allow on
        // a HTTP request. Saturating cast keeps 32-bit targets safe.
        let max_request_bytes = usize::try_from(cap).unwrap_or(usize::MAX);
        Self {
            config: Arc::new(config),
            effective_bind: Arc::new(effective_bind),
            state: Arc::new(TokioMutex::new(state)),
            session_key: Arc::new(Zeroizing::new(session_key)),
            admin_key: Arc::new(Zeroizing::new(admin_key)),
            max_request_bytes,
            active_requests: Arc::new(AtomicU64::new(0)),
            agent_supervisor: Arc::new(AgentSupervisor::new()),
            event_hub,
        }
    }
}

/// Build the application router. Layers, outermost first as requests arrive:
/// `RequestBodyLimitLayer` (rejects oversize bodies before handlers see them)
/// → `TraceLayer` (tracing span per request) → `authenticate` (sets the
/// resolved `KeyKind` on the request) → per-route `require_tier`.
pub fn build_router(state: AppState) -> Router {
    let limit = state.max_request_bytes;

    // Session-tier sub-router. The layer stack inside this router runs as:
    //   require_session  (outermost — checks KeyKind == Session)
    //     body_limit     (next — checks Content-Length once tier is OK)
    //       handler
    // So a wrong-tier (admin) key on a session route is rejected — and
    // `auth.wrong_kind` is logged — BEFORE the body limit sees the request,
    // even when the body is oversized. The tier gate is the gatekeeper for
    // both the durable auth-failure trail and the body limit.
    let session_routes = Router::new()
        .route("/v1/status", get(status_handler))
        .route("/v1/status/agent", get(status_agent_handler))
        // Alias for `docs/specs/api/api.md` which documents the same handler
        // under both the Status API (`/v1/status/agent`) and the Agent API
        // (`/v1/agent/status`).
        .route("/v1/agent/status", get(status_agent_handler))
        .route("/v1/status/connections", get(status_connections_handler))
        .route("/v1/config/export", get(config_export_handler))
        .route("/v1/config/validate", post(config_validate_handler))
        .route("/v1/agent/capabilities", get(agent_capabilities_handler))
        .route("/v1/logs/events", get(logs_events_handler))
        .route("/v1/logs/commands", get(logs_commands_handler))
        .route("/v1/logs/permissions", get(logs_permissions_handler))
        .route("/v1/logs/security", get(logs_security_handler))
        .route("/v1/logs/sessions", get(logs_sessions_handler))
        .route("/v1/metrics/summary", get(metrics_summary_handler))
        .route("/v1/ws", get(ws_handler))
        .route(
            "/v1/sessions",
            get(sessions_list_handler).post(sessions_create_handler),
        )
        .route(
            "/v1/sessions/{id}",
            get(sessions_get_handler).delete(sessions_close_handler),
        )
        .route("/v1/sessions/{id}/load", post(sessions_load_handler))
        .route("/v1/sessions/{id}/resume", post(sessions_resume_handler))
        .route("/v1/sessions/{id}/prompt", post(sessions_prompt_handler))
        .route("/v1/sessions/{id}/cancel", post(sessions_cancel_handler))
        .route(
            "/v1/sessions/{id}/prompts/{prompt_id}",
            get(sessions_prompt_status_handler),
        )
        .route("/v1/sessions/{id}/events", get(sessions_events_handler))
        .layer(RequestBodyLimitLayer::new(limit))
        // Axum's per-extractor default body limit (2 MiB) would silently cap
        // String/Json/etc handlers below the configured runtime limit. Disable
        // it so `RequestBodyLimitLayer` is the sole gatekeeper for body size.
        .layer(DefaultBodyLimit::disable())
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_session,
        ));

    let admin_routes = Router::new()
        .route("/v1/security/check", get(security_check_handler))
        .route("/v1/agent/install", post(agent_install_handler))
        .route("/v1/agent/start", post(agent_start_handler))
        .route("/v1/agent/stop", post(agent_stop_handler))
        .layer(RequestBodyLimitLayer::new(limit))
        .layer(DefaultBodyLimit::disable())
        .route_layer(middleware::from_fn_with_state(state.clone(), require_admin));

    Router::new()
        .merge(session_routes)
        .merge(admin_routes)
        // Authenticate runs OUTSIDE the tier gate. It sets the resolved
        // KeyKind on the request extensions; require_session then matches
        // against the tier required by this router.
        .layer(middleware::from_fn_with_state(state.clone(), authenticate))
        .layer(TraceLayer::new_for_http())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            track_active_requests,
        ))
        // Outermost. Catches framework-generated rejections (oversize body
        // 413, axum's malformed-query 400, malformed-body 400, fallback 404)
        // and rewraps them in the standard envelope so every error response
        // the client sees has the documented `{ok:false, error:{...}}` shape.
        .layer(middleware::from_fn(ensure_envelope))
        .with_state(state)
}

/// Drive the HTTP server on an already-bound listener. Returns when the
/// graceful shutdown signal arrives (SIGTERM or SIGINT) or when axum's
/// internal IO loop errors.
///
/// Factored out from the CLI entry so integration tests can drive it on a
/// `127.0.0.1:0` listener.
pub async fn serve(state: AppState, listener: TcpListener) -> Result<()> {
    let app = build_router(state);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .map_err(|source| StackError::ServeIo { source })
}

#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let ctrl_c = async {
        // Installing the handler can fail on unusual hosts (e.g. PID 1
        // without a controlling terminal). Treat that as "no Ctrl-C signal
        // available" rather than crashing the server.
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::warn!(error = %err, "ctrl-c handler install failed; relying on SIGTERM only");
            std::future::pending::<()>().await;
        }
    };
    let term = async {
        match signal(SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(err) => {
                tracing::warn!(error = %err, "SIGTERM handler install failed; relying on Ctrl-C only");
                std::future::pending::<()>().await;
            }
        }
    };
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    // Non-unix hosts (tests, dev on Windows): only Ctrl-C is wired.
    if let Err(err) = tokio::signal::ctrl_c().await {
        tracing::warn!(error = %err, "ctrl-c handler install failed");
        std::future::pending::<()>().await;
    }
}

#[derive(Deserialize)]
struct WsClientMessage {
    #[serde(rename = "type")]
    message_type: String,
    #[serde(default)]
    topics: Vec<String>,
}

async fn ws_handler(State(state): State<AppState>, ws: WebSocketUpgrade) -> Response {
    let event_hub = state.event_hub.clone();
    ws.on_upgrade(move |socket| ws_connection(socket, event_hub))
        .into_response()
}

async fn ws_connection(mut socket: WebSocket, event_hub: EventHub) {
    let mut receiver = event_hub.subscribe();
    let mut subscribed_topics = HashSet::<String>::new();

    loop {
        tokio::select! {
            inbound = socket.recv() => {
                let Some(inbound) = inbound else {
                    break;
                };
                let Ok(message) = inbound else {
                    break;
                };
                match message {
                    Message::Text(text) => {
                        handle_ws_client_message(&mut subscribed_topics, text.as_str()).await;
                    }
                    Message::Binary(_) | Message::Ping(_) | Message::Pong(_) => {}
                    Message::Close(_) => break,
                }
            }
            event = receiver.recv() => {
                let Ok(event) = event else {
                    continue;
                };
                if !subscribed_topics.contains(&event.topic) {
                    continue;
                }
                let payload = match serde_json::to_string(&event) {
                    Ok(payload) => payload,
                    Err(err) => {
                        tracing::warn!(error = %err, event_id = %event.id, "failed to serialize websocket event");
                        continue;
                    }
                };
                if socket.send(Message::Text(payload.into())).await.is_err() {
                    break;
                }
            }
        }
    }
}

async fn handle_ws_client_message(subscribed_topics: &mut HashSet<String>, text: &str) {
    let message: WsClientMessage = match serde_json::from_str(text) {
        Ok(message) => message,
        Err(err) => {
            tracing::debug!(error = %err, "dropping malformed websocket client message");
            return;
        }
    };
    if message.message_type != "subscribe" {
        return;
    }
    for topic in message.topics {
        if topic.starts_with("sessions.") {
            subscribed_topics.insert(topic);
        } else {
            tracing::debug!(topic = %topic, "dropping unsupported websocket subscription topic");
        }
    }
}

// ----- Middleware -----------------------------------------------------------

/// Extract `Authorization: Bearer <key>`, classify it against the cached
/// session / admin keys, tag the request with the resolved `KeyKind`. On any
/// failure, write an `auth_failures` row and return 401.
async fn authenticate(
    State(state): State<AppState>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    let route = req.uri().path().to_owned();
    let client_ip = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0.ip().to_string());

    // Spec hardening (`docs/specs/security.md`): reject duplicate or malformed
    // Authorization headers. `headers().get_all` exposes every value, so we
    // can count them and refuse anything other than exactly one. Without this
    // check, a request with two Authorization headers would silently take the
    // first value, which is an auth ambiguity attackers can exploit.
    let mut auth_values = req.headers().get_all(AUTHORIZATION).iter();
    let header = match (auth_values.next(), auth_values.next()) {
        (None, _) => {
            return match log_failure(
                &state,
                KeyKind::Unknown,
                AuthFailureReason::Missing,
                client_ip.as_deref(),
                &route,
            )
            .await
            {
                Ok(()) => reject(
                    StatusCode::UNAUTHORIZED,
                    "auth.missing",
                    "missing Authorization header",
                ),
                Err(state_err) => state_err,
            };
        }
        (Some(_), Some(_)) => {
            return match log_failure(
                &state,
                KeyKind::Unknown,
                AuthFailureReason::MalformedHeader,
                client_ip.as_deref(),
                &route,
            )
            .await
            {
                Ok(()) => reject(
                    StatusCode::UNAUTHORIZED,
                    "auth.malformed_header",
                    "duplicate Authorization headers are not allowed",
                ),
                Err(state_err) => state_err,
            };
        }
        (Some(only), None) => only,
    };

    let bearer = match parse_bearer(header) {
        Some(token) => token,
        None => {
            return match log_failure(
                &state,
                KeyKind::Unknown,
                AuthFailureReason::MalformedHeader,
                client_ip.as_deref(),
                &route,
            )
            .await
            {
                Ok(()) => reject(
                    StatusCode::UNAUTHORIZED,
                    "auth.malformed_header",
                    "Authorization header must be `Bearer <token>` with a single ASCII token",
                ),
                Err(state_err) => state_err,
            };
        }
    };

    let bearer_bytes = bearer.as_bytes();
    let matched = if constant_time_eq(bearer_bytes, state.session_key.as_bytes()) {
        Some(KeyKind::Session)
    } else if constant_time_eq(bearer_bytes, state.admin_key.as_bytes()) {
        Some(KeyKind::Admin)
    } else {
        None
    };

    match matched {
        Some(kind) => {
            req.extensions_mut().insert(kind);
            next.run(req).await
        }
        None => match log_failure(
            &state,
            KeyKind::Unknown,
            AuthFailureReason::Invalid,
            client_ip.as_deref(),
            &route,
        )
        .await
        {
            Ok(()) => reject(
                StatusCode::UNAUTHORIZED,
                "auth.invalid",
                "invalid credential",
            ),
            Err(state_err) => state_err,
        },
    }
}

/// Per-route tier gate: rejects valid keys of the wrong tier with 401 and
/// records a `wrong_kind` auth-failure row. Session-tier in this batch.
async fn require_session(
    State(state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    enforce_tier(KeyKind::Session, state, req, next).await
}

async fn require_admin(State(state): State<AppState>, req: Request<Body>, next: Next) -> Response {
    enforce_tier(KeyKind::Admin, state, req, next).await
}

async fn track_active_requests(
    State(state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let _guard = ActiveRequestGuard::new(state.active_requests.clone());
    next.run(req).await
}

struct ActiveRequestGuard {
    active_requests: Arc<AtomicU64>,
}

impl ActiveRequestGuard {
    fn new(active_requests: Arc<AtomicU64>) -> Self {
        active_requests.fetch_add(1, Ordering::Relaxed);
        Self { active_requests }
    }
}

impl Drop for ActiveRequestGuard {
    fn drop(&mut self) {
        self.active_requests.fetch_sub(1, Ordering::Relaxed);
    }
}

async fn enforce_tier(
    required: KeyKind,
    state: AppState,
    req: Request<Body>,
    next: Next,
) -> Response {
    let presented = req.extensions().get::<KeyKind>().copied();
    match presented {
        Some(kind) if kind == required => next.run(req).await,
        Some(kind) => {
            let route = req.uri().path().to_owned();
            let client_ip = req
                .extensions()
                .get::<ConnectInfo<SocketAddr>>()
                .map(|info| info.0.ip().to_string());
            match log_failure(
                &state,
                kind,
                AuthFailureReason::WrongKind,
                client_ip.as_deref(),
                &route,
            )
            .await
            {
                Ok(()) => reject(
                    StatusCode::UNAUTHORIZED,
                    "auth.wrong_kind",
                    "presented key is not valid for this route",
                ),
                Err(state_err) => state_err,
            }
        }
        None => {
            // `authenticate` runs ahead of this and must populate the tag.
            // Treat a missing tag as a server-side wiring bug.
            tracing::error!(
                route = %req.uri().path(),
                "require_tier saw no KeyKind extension; authenticate middleware not wired ahead",
            );
            reject(
                StatusCode::INTERNAL_SERVER_ERROR,
                "auth.internal",
                "auth middleware misconfigured",
            )
        }
    }
}

/// Wrap framework-generated error responses in the standard `{ok:false, ...}`
/// envelope. Responses already carrying `application/json` (handler returns,
/// auth middleware) pass through untouched. Original HTTP semantic headers
/// (e.g. `Allow` on a 405) are copied onto the rewrapped response so
/// downstream method-discovery and similar conventions keep working.
async fn ensure_envelope(req: Request<Body>, next: Next) -> Response {
    let response = next.run(req).await;
    let status = response.status();
    if !status.is_client_error() && !status.is_server_error() {
        return response;
    }
    let is_json = response
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.starts_with("application/json"))
        .unwrap_or(false);
    if is_json {
        return response;
    }
    let (parts, _body) = response.into_parts();
    let mut new_response = ApiError::new(error_code_for_status(status), message_for_status(status))
        .into_response_with(status);
    // Preserve any non-payload headers from the original framework response.
    // Skip content-type/content-length because we're replacing the body with
    // a JSON envelope; let axum's response builder set those fresh.
    for (name, value) in parts.headers.iter() {
        if name == http::header::CONTENT_TYPE || name == http::header::CONTENT_LENGTH {
            continue;
        }
        new_response
            .headers_mut()
            .append(name.clone(), value.clone());
    }
    new_response
}

fn error_code_for_status(status: StatusCode) -> &'static str {
    match status {
        StatusCode::BAD_REQUEST => "request.invalid",
        StatusCode::UNAUTHORIZED => "auth.invalid",
        StatusCode::FORBIDDEN => "auth.forbidden",
        StatusCode::NOT_FOUND => "not_found",
        StatusCode::METHOD_NOT_ALLOWED => "method_not_allowed",
        StatusCode::PAYLOAD_TOO_LARGE => "request.too_large",
        StatusCode::UNSUPPORTED_MEDIA_TYPE => "request.unsupported_media_type",
        _ if status.is_server_error() => "internal_error",
        _ => "request.rejected",
    }
}

fn message_for_status(status: StatusCode) -> &'static str {
    match status {
        StatusCode::BAD_REQUEST => "bad request",
        StatusCode::UNAUTHORIZED => "authentication required",
        StatusCode::FORBIDDEN => "forbidden",
        StatusCode::NOT_FOUND => "not found",
        StatusCode::METHOD_NOT_ALLOWED => "method not allowed",
        StatusCode::PAYLOAD_TOO_LARGE => "request body exceeds configured size limit",
        StatusCode::UNSUPPORTED_MEDIA_TYPE => "unsupported media type",
        _ if status.is_server_error() => "internal server error",
        _ => "request rejected",
    }
}

fn parse_bearer(header: &http::HeaderValue) -> Option<String> {
    let text = header.to_str().ok()?;
    let rest = text.strip_prefix("Bearer ")?;
    if rest.is_empty() || rest.chars().any(|c| c.is_whitespace()) {
        return None;
    }
    Some(rest.to_owned())
}

async fn log_failure(
    state: &AppState,
    kind: KeyKind,
    reason: AuthFailureReason,
    client_ip: Option<&str>,
    route: &str,
) -> std::result::Result<(), Response> {
    // Hold the lock only long enough to insert one row. record_auth_failure
    // is blocking sqlite work but lasts microseconds for a single INSERT.
    //
    // The hardening contract requires every rejected auth attempt to leave
    // a durable `auth_failures` row. If we can't write it (state DB locked,
    // corrupt, or full), failing closed with 500 is safer than silently
    // returning 401: the operator's monitoring sees the failure, and an
    // attacker cannot brute-force keys against an unrecorded server.
    let store = state.state.lock().await;
    if let Err(err) = record_auth_failure(&store, kind, reason, client_ip, Some(route)) {
        tracing::error!(error = %err, "failed to record auth failure");
        return Err(reject(
            StatusCode::INTERNAL_SERVER_ERROR,
            "state.error",
            "internal state error while recording auth failure",
        ));
    }
    Ok(())
}

fn reject(status: StatusCode, code: &str, message: &str) -> Response {
    ApiError::new(code, message).into_response_with(status)
}

// ----- Handlers -------------------------------------------------------------

#[derive(Serialize)]
struct StatusResponse {
    schema_version: i64,
    latest_event: Option<String>,
    server: ServerInfo,
}

#[derive(Serialize)]
struct ServerInfo {
    version: &'static str,
}

async fn status_handler(
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
struct StatusAgentResponse {
    configured: bool,
    agent: AgentStatusJson,
    process_state: String,
    pid: Option<u32>,
    lifecycle_events: Vec<AgentLifecycleJson>,
}

#[derive(Serialize)]
struct AgentStatusJson {
    id: String,
    name: String,
    command: String,
    args: Vec<String>,
    cwd: Option<String>,
    restart: String,
}

#[derive(Serialize)]
struct AgentLifecycleJson {
    id: String,
    created_at: String,
    event_kind: String,
    message: String,
    payload_json: String,
}

async fn status_agent_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<StatusAgentResponse>, StackError> {
    let store = state.state.lock().await;
    let lifecycle_events = store.query_agent_lifecycle(default_logs_limit())?;
    drop(store);
    let snapshot = state.agent_supervisor.snapshot().await;
    let agent = &state.config.agent;
    Ok(ApiSuccess::new(StatusAgentResponse {
        configured: true,
        agent: AgentStatusJson {
            id: agent.id.clone(),
            name: agent.name.clone(),
            command: agent.command.clone(),
            args: agent.args.clone(),
            cwd: agent.cwd.clone(),
            restart: agent.restart.clone(),
        },
        process_state: format!("{:?}", snapshot.state).to_lowercase(),
        pid: snapshot.pid,
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
    }))
}

#[derive(Serialize)]
struct StatusConnectionsResponse {
    active_requests: u64,
}

async fn status_connections_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<StatusConnectionsResponse>, StackError> {
    Ok(ApiSuccess::new(StatusConnectionsResponse {
        active_requests: state.active_requests.load(Ordering::Relaxed),
    }))
}

#[derive(Serialize)]
struct ConfigExportResponse {
    toml: String,
}

async fn config_export_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<ConfigExportResponse>, StackError> {
    let toml = state.config.to_canonical_toml()?;
    Ok(ApiSuccess::new(ConfigExportResponse { toml }))
}

#[derive(Serialize)]
struct ConfigValidateResponse {
    valid: bool,
}

/// POST /v1/config/validate accepts the canonical TOML in the raw request
/// body (any content type). Returning `{valid:true}` on parse + validate
/// success matches the read-only contract; on failure the standard envelope
/// surfaces the underlying `config.invalid` (or related) code.
async fn config_validate_handler(
    body: String,
) -> std::result::Result<ApiSuccess<ConfigValidateResponse>, StackError> {
    let _ = crate::config::load_config_from_str(&body)?;
    Ok(ApiSuccess::new(ConfigValidateResponse { valid: true }))
}

/// Per-request cap on `GET /v1/logs/events?limit=`. An authenticated session
/// could otherwise request billions of rows and turn a log query into a
/// memory-pressure attack. Operators with longer-tail queries should page.
const MAX_LOGS_LIMIT: u32 = 1000;

#[derive(Deserialize)]
struct LogsEventsParams {
    #[serde(default = "default_logs_limit")]
    limit: u32,
    #[serde(default)]
    level: Option<String>,
}

fn default_logs_limit() -> u32 {
    100
}

#[derive(Serialize)]
struct LogsEventsResponse {
    events: Vec<LogEventJson>,
}

#[derive(Serialize)]
struct LogEventJson {
    id: String,
    created_at: String,
    level: String,
    kind: String,
    message: String,
    payload_json: String,
}

async fn logs_events_handler(
    Query(params): Query<LogsEventsParams>,
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<LogsEventsResponse>, StackError> {
    let limit = params.limit.min(MAX_LOGS_LIMIT);
    let store = state.state.lock().await;
    let events = store.query_events(EventFilter {
        limit,
        level: params.level.as_deref(),
    })?;
    drop(store);
    let events = events
        .into_iter()
        .map(|e| LogEventJson {
            id: e.id,
            created_at: e.created_at,
            level: e.level,
            kind: e.kind,
            message: e.message,
            payload_json: e.payload_json,
        })
        .collect();
    Ok(ApiSuccess::new(LogsEventsResponse { events }))
}

#[derive(Serialize)]
struct LogsSessionsResponse {
    sessions: Vec<SessionLogJson>,
}

#[derive(Serialize)]
struct SessionLogJson {
    id: String,
    created_at: String,
    updated_at: String,
    status: String,
}

async fn logs_sessions_handler(
    Query(params): Query<LogsLimitParams>,
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<LogsSessionsResponse>, StackError> {
    let limit = params.limit.min(MAX_LOGS_LIMIT);
    let store = state.state.lock().await;
    let sessions = store.query_sessions(limit)?;
    drop(store);
    Ok(ApiSuccess::new(LogsSessionsResponse {
        sessions: sessions
            .into_iter()
            .map(|session| SessionLogJson {
                id: session.id,
                created_at: session.created_at,
                updated_at: session.updated_at,
                status: session.status,
            })
            .collect(),
    }))
}

#[derive(Deserialize)]
struct LogsLimitParams {
    #[serde(default = "default_logs_limit")]
    limit: u32,
}

#[derive(Serialize)]
struct LogsCommandsResponse {
    commands: Vec<CommandLogJson>,
}

#[derive(Serialize)]
struct CommandLogJson {
    id: String,
    created_at: String,
    updated_at: String,
    status: String,
    command: String,
    exit_status: Option<i64>,
}

async fn logs_commands_handler(
    Query(params): Query<LogsLimitParams>,
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<LogsCommandsResponse>, StackError> {
    let limit = params.limit.min(MAX_LOGS_LIMIT);
    let store = state.state.lock().await;
    let commands = store.query_commands(limit)?;
    drop(store);
    Ok(ApiSuccess::new(LogsCommandsResponse {
        commands: commands
            .into_iter()
            .map(|command| CommandLogJson {
                id: command.id,
                created_at: command.created_at,
                updated_at: command.updated_at,
                status: command.status,
                command: command.command,
                exit_status: command.exit_status,
            })
            .collect(),
    }))
}

async fn logs_permissions_handler(
    Query(params): Query<LogsLimitParams>,
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<LogsEventsResponse>, StackError> {
    let limit = params.limit.min(MAX_LOGS_LIMIT);
    let store = state.state.lock().await;
    let events = store.query_permission_events(limit)?;
    drop(store);
    let events = events
        .into_iter()
        .map(|e| LogEventJson {
            id: e.id,
            created_at: e.created_at,
            level: e.level,
            kind: e.kind,
            message: e.message,
            payload_json: e.payload_json,
        })
        .collect();
    Ok(ApiSuccess::new(LogsEventsResponse { events }))
}

#[derive(Serialize)]
struct LogsSecurityResponse {
    auth_failures: Vec<AuthFailureJson>,
}

#[derive(Serialize)]
struct AuthFailureJson {
    id: String,
    created_at: String,
    key_kind: String,
    reason: String,
    client_ip: Option<String>,
    route: Option<String>,
    payload_json: String,
}

async fn logs_security_handler(
    Query(params): Query<LogsLimitParams>,
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<LogsSecurityResponse>, StackError> {
    let limit = params.limit.min(MAX_LOGS_LIMIT);
    let store = state.state.lock().await;
    let auth_failures = store.query_auth_failures(AuthFailureFilter { limit })?;
    drop(store);
    Ok(ApiSuccess::new(LogsSecurityResponse {
        auth_failures: auth_failures
            .into_iter()
            .map(|failure| AuthFailureJson {
                id: failure.id,
                created_at: failure.created_at,
                key_kind: failure.key_kind,
                reason: failure.reason,
                client_ip: failure.client_ip,
                route: failure.route,
                payload_json: failure.payload_json,
            })
            .collect(),
    }))
}

#[derive(Serialize)]
struct MetricsSummaryResponse {
    events: i64,
    sessions: i64,
    commands: i64,
    auth_failures: i64,
    agent_lifecycle: i64,
    installer_runs: i64,
    agent_capabilities: i64,
    prompts: i64,
}

async fn metrics_summary_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<MetricsSummaryResponse>, StackError> {
    let store = state.state.lock().await;
    let counts = store.counts()?;
    drop(store);
    Ok(ApiSuccess::new(MetricsSummaryResponse {
        events: counts.events,
        sessions: counts.sessions,
        commands: counts.commands,
        auth_failures: counts.auth_failures,
        agent_lifecycle: counts.agent_lifecycle,
        installer_runs: counts.installer_runs,
        agent_capabilities: counts.agent_capabilities,
        prompts: counts.prompts,
    }))
}

#[derive(Serialize)]
struct SecurityCheckResponse {
    ok: bool,
    findings: Vec<SecurityFindingJson>,
    auth_failure_count: i64,
}

#[derive(Serialize)]
struct SecurityFindingJson {
    code: String,
    severity: String,
    message: String,
}

async fn security_check_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<SecurityCheckResponse>, StackError> {
    let store = state.state.lock().await;
    let counts = store.counts()?;
    let recent_cutoff =
        (Utc::now() - Duration::minutes(1)).to_rfc3339_opts(SecondsFormat::Nanos, true);
    let recent_auth_failures = store.count_auth_failures_since(&recent_cutoff)?;
    drop(store);
    let findings = security_findings(&state, recent_auth_failures);
    Ok(ApiSuccess::new(SecurityCheckResponse {
        ok: findings.is_empty(),
        findings,
        auth_failure_count: counts.auth_failures,
    }))
}

fn security_findings(state: &AppState, auth_failure_count: i64) -> Vec<SecurityFindingJson> {
    let mut findings = Vec::new();
    let bind_is_public = state
        .effective_bind
        .parse::<SocketAddr>()
        .map(|addr| addr.ip().is_unspecified())
        .unwrap_or(false);

    if bind_is_public {
        findings.push(SecurityFindingJson::warning(
            "api.public_bind",
            "API bind address listens on all interfaces",
        ));
    }

    if bind_is_public
        && state
            .config
            .security
            .http
            .allowed_origins
            .iter()
            .any(|origin| origin == "*")
    {
        findings.push(SecurityFindingJson::critical(
            "http.wildcard_origin_public_bind",
            "wildcard CORS origin is configured on a public bind address",
        ));
    }

    if state.config.security.http.trust_proxy_headers {
        findings.push(SecurityFindingJson::critical(
            "http.trust_proxy_without_trusted_proxies",
            "proxy headers are trusted but no trusted proxy allowlist is configured",
        ));
    }

    if state.session_key.is_empty() {
        findings.push(SecurityFindingJson::critical(
            "auth.session_key_empty",
            "session API key is empty",
        ));
    }

    if state.admin_key.is_empty() {
        findings.push(SecurityFindingJson::critical(
            "auth.admin_key_empty",
            "admin API key is empty",
        ));
    }

    let threshold = state.config.security.http.auth_failures_per_minute;
    if threshold > 0 && auth_failure_count >= i64::try_from(threshold).unwrap_or(i64::MAX) {
        findings.push(SecurityFindingJson::warning(
            "auth.failure_threshold",
            "auth failure count meets or exceeds the configured per-minute threshold",
        ));
    }

    findings
}

impl SecurityFindingJson {
    fn warning(code: &str, message: &str) -> Self {
        Self {
            code: code.to_owned(),
            severity: "warning".to_owned(),
            message: message.to_owned(),
        }
    }

    fn critical(code: &str, message: &str) -> Self {
        Self {
            code: code.to_owned(),
            severity: "critical".to_owned(),
            message: message.to_owned(),
        }
    }
}

// ----- Agent handlers -------------------------------------------------------

#[derive(Serialize)]
struct AgentInstallResponse {
    outcome: &'static str,
    path: String,
    sha256: String,
}

async fn agent_install_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<AgentInstallResponse>, StackError> {
    let install = state
        .config
        .agent
        .install
        .clone()
        .ok_or(StackError::AgentNotConfigured)?;
    let expected_sha256 = state.config.agent.expected_sha256.clone();
    // Resolve agent env from the secret store. The installer should only
    // see the same names the agent itself will see (security.md:91).
    let env = open_agent_env(&state.config)?;
    let workspace_root = std::path::PathBuf::from(state.config.workspace.root.clone());

    // Run the synchronous installer on a blocking thread so its
    // up-to-10-minute timeout window cannot pin a tokio runtime worker.
    // Critically, we do NOT hold the state lock while it runs — that would
    // make every other state-backed endpoint (incl. auth-failure logging)
    // wait behind the install.
    let result = tokio::task::spawn_blocking(move || {
        run_installer_capture(&install, expected_sha256.as_deref(), env, &workspace_root)
    })
    .await
    .map_err(|err| StackError::AgentInitializeFailed {
        reason: format!("installer thread join failed: {err}"),
    })?;

    // Persist the row briefly under the state lock. The lock is held only
    // for the single INSERT, not for the installer's runtime.
    {
        let store = state.state.lock().await;
        store.append_installer_run(InstallerRunInput {
            started_at: &result.row.started_at,
            finished_at: result.row.finished_at.as_deref(),
            status: &result.row.status,
            stdout: &result.row.stdout,
            stderr: &result.row.stderr,
            exit_status: result.row.exit_status,
        })?;
    }

    let outcome = result.outcome?;
    let outcome_label = outcome.label();
    let path = outcome.path().to_string_lossy().into_owned();
    let sha256 = outcome.sha256().to_owned();
    Ok(ApiSuccess::new(AgentInstallResponse {
        outcome: outcome_label,
        path,
        sha256,
    }))
}

fn open_agent_env(config: &Config) -> Result<std::collections::HashMap<String, String>> {
    if config.agent.env.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let home = home_dir()?;
    let store = SecretStore::open(&home)?;
    let mut env = std::collections::HashMap::with_capacity(config.agent.env.len());
    for name in &config.agent.env {
        let value = store.get(name)?;
        env.insert(name.clone(), value.to_owned());
    }
    Ok(env)
}

#[derive(Serialize)]
struct AgentStartResponse {
    started_at: String,
    capabilities: AgentCapabilitiesDto,
    pid: Option<u32>,
}

async fn agent_start_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<AgentStartResponse>, StackError> {
    // Resolve env BEFORE invoking the supervisor so the secret store is only
    // opened when [agent].env is non-empty. Production deployments always
    // have a populated store; tests with empty agent.env skip the open
    // entirely. open_agent_env enforces the same allowlist semantics
    // (security.md:91) regardless of caller.
    let env = open_agent_env(&state.config)?;
    let capabilities = state
        .agent_supervisor
        .start(
            &state.config.agent,
            &state.config.workspace.root,
            env,
            &state.state,
            state.event_hub.clone(),
        )
        .await?;
    let started_at = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
    let pid = state.agent_supervisor.snapshot().await.pid;
    Ok(ApiSuccess::new(AgentStartResponse {
        started_at,
        capabilities,
        pid,
    }))
}

#[derive(Serialize)]
struct AgentStopResponse {
    stopped_at: String,
    exit_status: Option<i32>,
}

async fn agent_stop_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<AgentStopResponse>, StackError> {
    let exit_status = state.agent_supervisor.stop(&state.state).await?;
    let stopped_at = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
    Ok(ApiSuccess::new(AgentStopResponse {
        stopped_at,
        exit_status,
    }))
}

#[derive(Serialize)]
struct AgentCapabilitiesResponseBody {
    agent_id: String,
    captured_at: String,
    capabilities: serde_json::Value,
    process_state: String,
}

async fn agent_capabilities_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<AgentCapabilitiesResponseBody>, StackError> {
    let agent_id = state.config.agent.id.clone();
    let snapshot: AgentSnapshot = state.agent_supervisor.snapshot().await;
    let store = state.state.lock().await;
    let record = store.latest_agent_capabilities(&agent_id)?;
    drop(store);
    let record = record.ok_or(StackError::AgentNotInitialized)?;
    let capabilities = serde_json::from_str(&record.capabilities_json).map_err(|err| {
        StackError::AgentInitializeFailed {
            reason: format!("stored capabilities are unparseable: {err}"),
        }
    })?;
    Ok(ApiSuccess::new(AgentCapabilitiesResponseBody {
        agent_id: record.agent_id,
        captured_at: record.captured_at,
        capabilities,
        process_state: format!("{:?}", snapshot.state).to_lowercase(),
    }))
}

// ----- Session handlers -----------------------------------------------------

#[derive(Serialize)]
struct SessionResponse {
    id: String,
    created_at: String,
    updated_at: String,
    status: String,
    agent_id: String,
    cwd: String,
    title: Option<String>,
    metadata_json: String,
}

impl From<SessionRecord> for SessionResponse {
    fn from(record: SessionRecord) -> Self {
        Self {
            id: record.id,
            created_at: record.created_at,
            updated_at: record.updated_at,
            status: record.status,
            agent_id: record.agent_id,
            cwd: record.cwd,
            title: record.title,
            metadata_json: record.metadata_json,
        }
    }
}

#[derive(Serialize)]
struct SessionsListResponse {
    sessions: Vec<SessionResponse>,
}

#[derive(Deserialize, Default)]
struct SessionsListParams {
    #[serde(default = "default_logs_limit")]
    limit: u32,
}

async fn sessions_list_handler(
    Query(params): Query<SessionsListParams>,
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<SessionsListResponse>, StackError> {
    let limit = params.limit.min(MAX_LOGS_LIMIT);
    let store = state.state.lock().await;
    let sessions = store.query_sessions(limit)?;
    drop(store);
    Ok(ApiSuccess::new(SessionsListResponse {
        sessions: sessions.into_iter().map(SessionResponse::from).collect(),
    }))
}

#[derive(Deserialize, Default)]
struct SessionsCreateBody {
    #[serde(default)]
    cwd: Option<String>,
    // `mcp_servers` is intentionally omitted from the public surface in this
    // batch. The spec (`docs/specs/acp/acp-bridge.md`) declares MCP servers
    // through admin-controlled config, not the session API. Accepting an
    // ad-hoc list from session-tier callers would let any session-key
    // holder request arbitrary agent-side process execution.
}

async fn sessions_create_handler(
    State(state): State<AppState>,
    body: Option<Json<SessionsCreateBody>>,
) -> std::result::Result<ApiSuccess<SessionResponse>, StackError> {
    let Json(payload) = body.unwrap_or_default();
    let cwd = resolve_session_cwd(payload.cwd, &state.config.workspace.root)?;
    let record = state
        .agent_supervisor
        .create_session(
            &state.config.agent,
            &state.config.workspace.root,
            Some(cwd),
            parse_mcp_servers(None)?,
            &state.state,
        )
        .await?;
    Ok(ApiSuccess::new(SessionResponse::from(record)))
}

async fn sessions_get_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> std::result::Result<ApiSuccess<SessionResponse>, StackError> {
    let store = state.state.lock().await;
    let record = store.get_session(&id)?;
    drop(store);
    let record = record.ok_or(StackError::SessionNotFound { id })?;
    Ok(ApiSuccess::new(SessionResponse::from(record)))
}

#[derive(Deserialize, Default)]
struct SessionsLoadBody {
    #[serde(default)]
    cwd: Option<String>,
    // See `SessionsCreateBody`: MCP servers come from admin config, not
    // session-tier request bodies, until a proper policy surface lands.
}

async fn sessions_load_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<SessionsLoadBody>>,
) -> std::result::Result<ApiSuccess<SessionResponse>, StackError> {
    let Json(payload) = body.unwrap_or_default();
    let cwd = payload
        .cwd
        .map(|raw| resolve_session_cwd(Some(raw), &state.config.workspace.root))
        .transpose()?;
    let record = state
        .agent_supervisor
        .load_session(
            &id,
            cwd,
            parse_mcp_servers(None)?,
            &state.config.workspace.root,
            &state.state,
        )
        .await?;
    Ok(ApiSuccess::new(SessionResponse::from(record)))
}

async fn sessions_resume_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<SessionsLoadBody>>,
) -> std::result::Result<ApiSuccess<SessionResponse>, StackError> {
    let Json(payload) = body.unwrap_or_default();
    let cwd = payload
        .cwd
        .map(|raw| resolve_session_cwd(Some(raw), &state.config.workspace.root))
        .transpose()?;
    let record = state
        .agent_supervisor
        .resume_session(
            &id,
            cwd,
            parse_mcp_servers(None)?,
            &state.config.workspace.root,
            &state.state,
        )
        .await?;
    Ok(ApiSuccess::new(SessionResponse::from(record)))
}

/// Resolve and validate a session `cwd` against `workspace.root`. Returns
/// the canonical (no `..`) string. Rejects anything outside the workspace
/// boundary; this is the same containment the Workspace API will share when
/// it lands.
fn resolve_session_cwd(raw: Option<String>, workspace_root: &str) -> Result<String> {
    let candidate = raw.unwrap_or_else(|| workspace_root.to_owned());
    let root_path = std::path::PathBuf::from(workspace_root);
    let candidate_path = std::path::PathBuf::from(&candidate);
    if !candidate_path.is_absolute() {
        return Err(StackError::PromptBodyInvalid(
            "session cwd must be an absolute path".to_owned(),
        ));
    }
    // Reject `..` segments before normalization rather than relying on
    // canonicalize, because the path may not exist yet and canonicalize would
    // fail. The spec already forbids traversal; we enforce it lexically.
    if candidate_path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(StackError::PromptBodyInvalid(
            "session cwd must not contain `..` segments".to_owned(),
        ));
    }
    if !candidate_path.starts_with(&root_path) {
        return Err(StackError::PromptBodyInvalid(format!(
            "session cwd must be under workspace.root ({workspace_root})"
        )));
    }
    Ok(candidate)
}

#[derive(Deserialize)]
struct SessionsPromptBody {
    prompt: serde_json::Value,
}

#[derive(Serialize)]
struct PromptSubmitResponse {
    prompt_id: String,
    session_id: String,
    status: String,
    created_at: String,
}

async fn sessions_prompt_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(payload): Json<SessionsPromptBody>,
) -> std::result::Result<ApiSuccess<PromptSubmitResponse>, StackError> {
    let blocks = parse_prompt_blocks(&payload.prompt)?;
    if blocks.is_empty() {
        return Err(StackError::PromptBodyEmpty);
    }
    // Canonical JSON of the parsed blocks is durable storage; the original
    // request body shape is what the agent sees, so we serialize the typed
    // ACP value (consistent with how we read it back).
    let prompt_json = serde_json::to_string(&blocks).map_err(|err| {
        StackError::PromptBodyInvalid(format!("failed to canonicalize prompt: {err}"))
    })?;
    let record = state
        .agent_supervisor
        .submit_prompt(&id, blocks, prompt_json, &state.state)
        .await?;
    Ok(ApiSuccess::new(PromptSubmitResponse {
        prompt_id: record.id,
        session_id: record.session_id,
        status: record.status,
        created_at: record.created_at,
    }))
}

#[derive(Serialize)]
struct PromptStatusResponse {
    id: String,
    session_id: String,
    created_at: String,
    updated_at: String,
    status: String,
    stop_reason: Option<String>,
    error_code: Option<String>,
    error_message: Option<String>,
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
        }
    }
}

async fn sessions_prompt_status_handler(
    State(state): State<AppState>,
    Path((session_id, prompt_id)): Path<(String, String)>,
) -> std::result::Result<ApiSuccess<PromptStatusResponse>, StackError> {
    let store = state.state.lock().await;
    let record = store.get_prompt(&prompt_id)?;
    drop(store);
    let record = record.ok_or_else(|| StackError::PromptNotFound {
        id: prompt_id.clone(),
    })?;
    if record.session_id != session_id {
        return Err(StackError::PromptSessionMismatch {
            session_id,
            prompt_id,
        });
    }
    Ok(ApiSuccess::new(PromptStatusResponse::from(record)))
}

#[derive(Deserialize, Default)]
struct SessionsEventsParams {
    #[serde(default = "default_logs_limit")]
    limit: u32,
    #[serde(default)]
    after: Option<String>,
}

#[derive(Serialize)]
struct SessionsEventsResponse {
    events: Vec<LogEventJson>,
}

async fn sessions_events_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<SessionsEventsParams>,
) -> std::result::Result<ApiSuccess<SessionsEventsResponse>, StackError> {
    let limit = params.limit.min(MAX_LOGS_LIMIT);
    let store = state.state.lock().await;
    let exists = store.get_session(&id)?.is_some();
    if !exists {
        return Err(StackError::SessionNotFound { id });
    }
    let events = store.query_session_events(&id, params.after.as_deref(), limit)?;
    drop(store);
    Ok(ApiSuccess::new(SessionsEventsResponse {
        events: events
            .into_iter()
            .map(|e| LogEventJson {
                id: e.id,
                created_at: e.created_at,
                level: e.level,
                kind: e.kind,
                message: e.message,
                payload_json: e.payload_json,
            })
            .collect(),
    }))
}

#[derive(Serialize)]
struct SessionsCancelResponse {
    session_id: String,
}

async fn sessions_cancel_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> std::result::Result<ApiSuccess<SessionsCancelResponse>, StackError> {
    state
        .agent_supervisor
        .cancel_session(&id, &state.state)
        .await?;
    Ok(ApiSuccess::new(SessionsCancelResponse { session_id: id }))
}

async fn sessions_close_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> std::result::Result<ApiSuccess<SessionResponse>, StackError> {
    let record = state
        .agent_supervisor
        .close_session(&id, &state.state)
        .await?;
    Ok(ApiSuccess::new(SessionResponse::from(record)))
}

// ----- Tests ----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::routing::get;
    use http::Method;
    use tower::ServiceExt;

    fn new_state(session: &str, admin: &str) -> AppState {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = tempdir.path().join("state.sqlite");
        let store = StateStore::open(&path).expect("state open");
        store.migrate().expect("migrate");
        // tempdir is dropped at the end of scope; for unit tests we leak it
        // because the AppState holds the StateStore (and its sqlite handle).
        std::mem::forget(tempdir);
        AppState::new(test_config(), store, session.to_owned(), admin.to_owned())
    }

    fn test_config() -> Config {
        let toml_text = include_str!("../tests/fixtures/valid-acp-stack.toml");
        crate::config::load_config_from_str(toml_text).expect("sample config parses")
    }

    #[tokio::test]
    async fn require_admin_rejects_session_keys_end_to_end() {
        let state = new_state("acps_session_abc", "acps_admin_xyz");
        // Synthetic admin-tier sub-router for proving require_admin works,
        // even though no admin-tier route ships externally in this batch.
        let admin_only = Router::new()
            .route("/ping", get(|| async { "pong" }))
            .route_layer(middleware::from_fn_with_state(state.clone(), require_admin))
            .with_state(state.clone());
        let app = Router::new()
            .nest("/v1", admin_only)
            .layer(middleware::from_fn_with_state(state.clone(), authenticate))
            .with_state(state.clone());

        let req = Request::builder()
            .method(Method::GET)
            .uri("/v1/ping")
            .header(AUTHORIZATION, "Bearer acps_session_abc")
            .body(Body::empty())
            .expect("request build");
        let response = app.oneshot(req).await.expect("response");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("read body");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("envelope json");
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "auth.wrong_kind");
    }

    #[tokio::test]
    async fn require_admin_accepts_admin_keys() {
        let state = new_state("acps_session_abc", "acps_admin_xyz");
        let admin_only = Router::new()
            .route("/ping", get(|| async { "pong" }))
            .route_layer(middleware::from_fn_with_state(state.clone(), require_admin))
            .with_state(state.clone());
        let app = Router::new()
            .nest("/v1", admin_only)
            .layer(middleware::from_fn_with_state(state.clone(), authenticate))
            .with_state(state.clone());

        let req = Request::builder()
            .method(Method::GET)
            .uri("/v1/ping")
            .header(AUTHORIZATION, "Bearer acps_admin_xyz")
            .body(Body::empty())
            .expect("request build");
        let response = app.oneshot(req).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn parse_bearer_accepts_well_formed_header() {
        let header = http::HeaderValue::from_static("Bearer acps_xyz");
        assert_eq!(parse_bearer(&header).as_deref(), Some("acps_xyz"));
    }

    #[test]
    fn parse_bearer_rejects_missing_prefix() {
        let header = http::HeaderValue::from_static("Token acps_xyz");
        assert!(parse_bearer(&header).is_none());
    }

    #[test]
    fn parse_bearer_rejects_internal_whitespace() {
        let header = http::HeaderValue::from_static("Bearer acps xyz");
        assert!(parse_bearer(&header).is_none());
    }

    #[test]
    fn parse_bearer_rejects_empty_token() {
        let header = http::HeaderValue::from_static("Bearer ");
        assert!(parse_bearer(&header).is_none());
    }
}
