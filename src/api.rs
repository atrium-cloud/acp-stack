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

use axum::Extension;
use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, DefaultBodyLimit, Multipart, Path, Query, Request, State};
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
use crate::commands::{CommandGateway, SubmitRequest};
use crate::config::{AgentAdapterConfig, Config};
use crate::envelope::{ApiError, ApiSuccess};
use crate::error::{Result, StackError};
use crate::events::EventHub;
use crate::fs_util::home_dir;
use crate::permissions::PermissionService;
use crate::secrets::SecretStore;
use crate::state::InstallerRunInput;
use crate::state::{AuthFailureFilter, EventFilter, PromptRecord, SessionRecord, StateStore};
use crate::supervisor::{AgentSnapshot, AgentSupervisor, parse_prompt_blocks};
use crate::workspace::{
    self, FileMetadata, FileRead, PathIntent, WorkspaceListing, resolve_workspace_path,
};

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
    pub commands: CommandGateway,
    pub permissions: PermissionService,
    pub auth_failure_blocker: Arc<crate::http_hardening::AuthFailureBlocker>,
    pub rate_limiter: Arc<crate::http_hardening::RateLimiter>,
}

impl AppState {
    /// Resolve the `events.source` label for a request based on the tier tag
    /// the auth pipeline (or the local UDS listener) attached to the request.
    /// `KeyKind::Local` is the only writer of `EVENT_SOURCE_LOCAL`; everything
    /// else is attributed to the public HTTP API.
    pub fn event_source_for(kind: Option<KeyKind>) -> &'static str {
        match kind {
            Some(KeyKind::Local) => crate::state::EVENT_SOURCE_LOCAL,
            _ => crate::state::EVENT_SOURCE_API,
        }
    }
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
        mut state: StateStore,
        session_key: String,
        admin_key: String,
        effective_bind: String,
    ) -> Self {
        let api_cap = config.api.max_request_bytes;
        let security_cap = config.security.http.max_request_bytes;
        let cap = api_cap.min(security_cap);
        let event_hub = EventHub::new();
        // Wire the hub into the store now so that every `append_event` write
        // also fans out on the `logs` topic. CLI tools that open the store
        // outside the daemon leave it unattached.
        state.attach_event_hub(event_hub.clone());
        // SQLite is local and `usize::MAX` covers any byte count we'd allow on
        // a HTTP request. Saturating cast keeps 32-bit targets safe.
        let max_request_bytes = usize::try_from(cap).unwrap_or(usize::MAX);
        let config_arc = Arc::new(config);
        let state_arc = Arc::new(TokioMutex::new(state));
        let permissions = PermissionService::new(
            state_arc.clone(),
            event_hub.clone(),
            config_arc.permissions.effective_request_timeout(),
            config_arc.permissions.effective_timeout_action(),
        );
        let commands = CommandGateway::new(
            state_arc.clone(),
            event_hub.clone(),
            config_arc.clone(),
            permissions.clone(),
        );
        let auth_failure_blocker = Arc::new(
            crate::http_hardening::AuthFailureBlocker::from_config(&config_arc.security.http),
        );
        let rate_limiter = Arc::new(crate::http_hardening::RateLimiter::from_config(
            &config_arc.security.http,
        ));
        Self {
            config: config_arc,
            effective_bind: Arc::new(effective_bind),
            state: state_arc,
            session_key: Arc::new(Zeroizing::new(session_key)),
            admin_key: Arc::new(Zeroizing::new(admin_key)),
            max_request_bytes,
            active_requests: Arc::new(AtomicU64::new(0)),
            agent_supervisor: Arc::new(AgentSupervisor::new()),
            event_hub,
            commands,
            permissions,
            auth_failure_blocker,
            rate_limiter,
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
        .route("/v1/commands/{id}/cancel", post(commands_cancel_handler))
        .route("/v1/deps", get(deps_get_handler))
        .route("/v1/deps/check", post(deps_check_handler))
        .route("/v1/permissions/pending", get(permissions_pending_handler))
        .route("/v1/permissions/{id}", get(permissions_get_handler))
        .route(
            "/v1/permissions/{id}/approve",
            post(permissions_approve_handler),
        )
        .route("/v1/permissions/{id}/deny", post(permissions_deny_handler))
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
        .route("/v1/config/import", post(config_import_handler))
        .route(
            "/v1/secrets",
            get(secrets_list_handler).post(secrets_set_handler),
        )
        .route(
            "/v1/secrets/{name}",
            axum::routing::delete(secrets_delete_handler),
        )
        .layer(RequestBodyLimitLayer::new(limit))
        .layer(DefaultBodyLimit::disable())
        .route_layer(middleware::from_fn_with_state(state.clone(), require_admin));

    let cors_layer = crate::http_hardening::build_cors_layer(&state.config.security.http);

    let mut router = Router::new()
        .merge(session_routes)
        .merge(admin_routes)
        // `log_api_request` sits INSIDE `authenticate` so it can read the
        // resolved `KeyKind` extension on the request. It records one
        // durable `api.request` event per completed request — see metrics
        // §api_connection_metrics.
        .layer(middleware::from_fn_with_state(
            state.clone(),
            log_api_request,
        ))
        // Authenticate runs OUTSIDE the tier gate. It sets the resolved
        // KeyKind on the request extensions; require_session then matches
        // against the tier required by this router.
        .layer(middleware::from_fn_with_state(state.clone(), authenticate));
    if let Some(cors) = cors_layer {
        router = router.layer(cors).layer(middleware::from_fn_with_state(
            state.clone(),
            enforce_http_origin,
        ));
    }
    router
        .layer(TraceLayer::new_for_http())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            track_active_requests,
        ))
        // Outermost. Catches framework-generated rejections (oversize body
        // 413, axum's malformed-query 400, malformed-body 400, fallback 404)
        // and rewraps them in the standard envelope so every error response
        // the client sees has the documented `{ok:false, error:{...}}` shape.
        .layer(middleware::from_fn_with_state(
            state.clone(),
            ensure_envelope,
        ))
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
pub(crate) async fn shutdown_signal() {
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

async fn ws_handler(
    State(state): State<AppState>,
    headers: http::HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    // Enforce Origin allowlist on upgrade. Browser clients always send an
    // Origin header; CLI/local clients don't. We honor the allowlist only
    // when an Origin is present, so local tools continue to work. The
    // self-check already warns about wildcard origins on public binds.
    let origin = headers
        .get(http::header::ORIGIN)
        .and_then(|value| value.to_str().ok());
    if !crate::http_hardening::origin_allowed(origin, &state.config.security.http) {
        let origin_text = origin.unwrap_or("").to_owned();
        persist_security_event(
            &state,
            crate::state::EVENT_SOURCE_API,
            "warn",
            "security.ws_origin_denied",
            "rejected ws upgrade with disallowed Origin",
            serde_json::json!({"origin": origin_text}),
        )
        .await;
        return reject(
            StatusCode::FORBIDDEN,
            "auth.origin_not_allowed",
            "Origin is not in the configured allowlist",
        );
    }
    let app_state = state.clone();
    ws.on_upgrade(move |socket| ws_connection(socket, app_state))
        .into_response()
}

async fn ws_connection(mut socket: WebSocket, state: AppState) {
    let mut receiver = state.event_hub.subscribe();
    let mut subscribed_topics = HashSet::<String>::new();
    let connection_id = next_ws_connection_id();
    let started_at = std::time::Instant::now();
    persist_ws_lifecycle_event(
        &state,
        "ws.client_connected",
        serde_json::json!({"connection_id": connection_id}),
    )
    .await;

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

    let duration_ms = started_at.elapsed().as_millis().min(i64::MAX as u128) as i64;
    let mut topics: Vec<String> = subscribed_topics.into_iter().collect();
    topics.sort();
    persist_ws_lifecycle_event(
        &state,
        "ws.client_disconnected",
        serde_json::json!({
            "connection_id": connection_id,
            "topics": topics,
            "duration_ms": duration_ms,
        }),
    )
    .await;
}

/// Monotonically-increasing connection identifier. Pairs the connect/disconnect
/// events for a single client across the durable event log. Reset per process
/// — durability of the pair is provided by the timestamp + connection_id
/// composite, not the counter alone.
fn next_ws_connection_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
    format!("ws_{nanos}_{seq}")
}

async fn persist_ws_lifecycle_event(state: &AppState, kind: &str, payload: serde_json::Value) {
    let payload_text = match serde_json::to_string(&payload) {
        Ok(text) => text,
        Err(err) => {
            tracing::warn!(error = %err, kind, "failed to serialize ws lifecycle event payload");
            return;
        }
    };
    let store = state.state.lock().await;
    if let Err(err) = store.append_event_with_source(
        "info",
        kind,
        crate::state::EVENT_SOURCE_API,
        "",
        &payload_text,
    ) {
        tracing::warn!(error = %err, kind, "failed to persist ws lifecycle event");
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
        if topic.starts_with("sessions.")
            || topic.starts_with("commands.")
            || topic == "workspace"
            || topic == "agent"
            || topic == "status"
            || topic == "logs"
            || topic == "permissions"
        {
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
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0.ip());
    let resolved_ip = peer
        .map(|ip| crate::http_hardening::client_ip(req.headers(), ip, &state.config.security.http));
    let client_ip = resolved_ip.map(|ip| ip.to_string());

    // Short-circuit blocked IPs before bearer comparison. The blocker entry
    // is reset on successful authenticate via record_success below.
    if let Some(ip) = resolved_ip {
        if let Some(until) = state.auth_failure_blocker.check(ip) {
            let until_secs = until
                .saturating_duration_since(std::time::Instant::now())
                .as_secs();
            persist_security_event(
                &state,
                crate::state::EVENT_SOURCE_API,
                "warn",
                "security.ip_block_active",
                "blocked IP attempted auth",
                serde_json::json!({
                    "ip": ip.to_string(),
                    "remaining_seconds": until_secs,
                    "route": route,
                }),
            )
            .await;
            return reject(
                StatusCode::TOO_MANY_REQUESTS,
                "auth.ip_blocked",
                "IP temporarily blocked due to repeated auth failures",
            );
        }
    }

    // Per-IP rate limit ticks on every request before bearer parsing. This is
    // the always-on cap; bursts that exceed it cost the attacker zero local
    // CPU since we reject before constant-time compare runs.
    if let Some(ip) = resolved_ip {
        if let Err(scope) = state.rate_limiter.check_per_ip(ip) {
            persist_rate_limit_event(&state, &route, ip, scope, None).await;
            return reject(
                StatusCode::TOO_MANY_REQUESTS,
                "auth.rate_limited",
                "rate limit exceeded",
            );
        }
    }

    // Spec hardening (`docs/specs/security.md`): reject duplicate or malformed
    // Authorization headers. `headers().get_all` exposes every value, so we
    // can count them and refuse anything other than exactly one. Without this
    // check, a request with two Authorization headers would silently take the
    // first value, which is an auth ambiguity attackers can exploit.
    let mut auth_values = req.headers().get_all(AUTHORIZATION).iter();
    let header = match (auth_values.next(), auth_values.next()) {
        (None, _) => {
            if let Some(response) = check_unauthenticated_rate(&state, resolved_ip, &route).await {
                return response;
            }
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
            if let Some(response) = check_unauthenticated_rate(&state, resolved_ip, &route).await {
                return response;
            }
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
            if let Some(response) = check_unauthenticated_rate(&state, resolved_ip, &route).await {
                return response;
            }
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
            // Per-key rate limit. Fingerprint is sha256 of the bearer truncated
            // to 16 hex chars; the raw key never enters the limiter map or any
            // event payload.
            let fingerprint = crate::http_hardening::key_fingerprint(&bearer);
            if let Err(scope) = state.rate_limiter.check_per_key(&fingerprint) {
                if let Some(ip) = resolved_ip {
                    persist_rate_limit_event(&state, &route, ip, scope, Some(&fingerprint)).await;
                }
                return reject(
                    StatusCode::TOO_MANY_REQUESTS,
                    "auth.rate_limited",
                    "rate limit exceeded",
                );
            }
            if let Some(ip) = resolved_ip {
                state.auth_failure_blocker.record_success(ip);
            }
            req.extensions_mut().insert(kind);
            next.run(req).await
        }
        None => {
            if let Some(response) = check_unauthenticated_rate(&state, resolved_ip, &route).await {
                return response;
            }
            match log_failure(
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
            }
        }
    }
}

/// Check the unauthenticated rate limit and return a 429 response if the
/// IP's unauthenticated bucket is exhausted. Called from every auth-failure
/// branch so floods of bad credentials are throttled below the
/// auth-failure-block threshold without burning bearer-compare CPU.
async fn check_unauthenticated_rate(
    state: &AppState,
    resolved_ip: Option<std::net::IpAddr>,
    route: &str,
) -> Option<Response> {
    let ip = resolved_ip?;
    match state.rate_limiter.check_unauthenticated(ip) {
        Ok(()) => None,
        Err(scope) => {
            persist_rate_limit_event(state, route, ip, scope, None).await;
            Some(reject(
                StatusCode::TOO_MANY_REQUESTS,
                "auth.rate_limited",
                "rate limit exceeded",
            ))
        }
    }
}

/// Append a `security.rate_limited` durable event with scope label and
/// (when authenticated) the truncated key fingerprint. The raw bearer is
/// never persisted.
async fn persist_rate_limit_event(
    state: &AppState,
    route: &str,
    ip: std::net::IpAddr,
    scope: crate::http_hardening::RateLimitScope,
    key_fingerprint: Option<&str>,
) {
    let mut payload = serde_json::json!({
        "scope": scope.as_str(),
        "ip": ip.to_string(),
        "route": route,
    });
    if let (Some(fp), Some(map)) = (key_fingerprint, payload.as_object_mut()) {
        map.insert(
            "key_fingerprint".to_owned(),
            serde_json::Value::String(fp.to_owned()),
        );
    }
    persist_security_event(
        state,
        crate::state::EVENT_SOURCE_API,
        "warn",
        "security.rate_limited",
        "rate limit exceeded",
        payload,
    )
    .await;
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

pub(crate) async fn track_active_requests(
    State(state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let _guard = ActiveRequestGuard::new(state.active_requests.clone());
    next.run(req).await
}

/// Per-completed-request audit event. Emits one `api.request` row into the
/// durable `events` table with `{method, path, status, duration_ms, key_kind}`.
/// Skips routes that the metrics layer should ignore — WS upgrades and the
/// high-cardinality `/v1/status*` polls — so request volume stays bounded.
///
/// The event powers the `api_connections` block in `/v1/metrics/summary`.
pub(crate) async fn log_api_request(
    State(state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let started_at = std::time::Instant::now();
    let method = req.method().as_str().to_owned();
    let raw_path = req.uri().path().to_owned();
    // Prefer the matched path template (e.g. `/v1/sessions/{id}`) to keep
    // event cardinality bounded. Falls back to the raw path for routes that
    // don't have a matched template (rare; mostly framework fallbacks).
    let path = req
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|m| m.as_str().to_owned())
        .unwrap_or_else(|| raw_path.clone());
    let resolved_kind = req.extensions().get::<KeyKind>().copied();
    let key_kind_label = resolved_kind.map(|k| k.as_wire_str());
    // Pick the `events.source` label up-front so the audit row's `source`
    // column reflects the caller tier (`local` for UDS-driven acpctl calls,
    // `api` otherwise). The handler runs next; the source decision is fixed
    // at this point because the tier tag is set by `authenticate` /
    // `tag_local` BEFORE this middleware sees the request.
    let event_source = AppState::event_source_for(resolved_kind);

    let response = next.run(req).await;

    // The cardinality skip (`/v1/status*`, `/v1/ws`) targets public-API
    // polling. `acpctl` calls arrive on the local UDS as one-shot operations
    // and the spec (`docs/specs/acpctl/acpctl.md:47`) requires every action
    // to be logged with `source = "local"`, so keep the audit row in that
    // case.
    let is_local_caller = matches!(resolved_kind, Some(KeyKind::Local));
    if !is_local_caller
        && (should_skip_api_request_log(&path) || should_skip_api_request_log(&raw_path))
    {
        return response;
    }

    let status = response.status().as_u16();
    let duration_ms = started_at.elapsed().as_millis().min(i64::MAX as u128) as i64;
    let payload = serde_json::json!({
        "method": method,
        "path": path,
        "status": status,
        "duration_ms": duration_ms,
        "key_kind": key_kind_label,
    });
    let payload_text = payload.to_string();
    // Best-effort: a failed audit insert must not break the response. Surface
    // it at warn so the operator can see the divergence in tracing logs.
    let store = state.state.lock().await;
    if let Err(err) =
        store.append_event_with_source("info", "api.request", event_source, "", &payload_text)
    {
        tracing::warn!(error = %err, path, "failed to persist api.request event");
    }
    response
}

/// Returns true when the path should not produce an `api.request` row.
/// `/v1/ws` and `/v1/status*` are excluded — the first generates its own
/// `ws.client_connected` / `ws.client_disconnected` pair, the second is a
/// frequent poll surface whose cardinality would dwarf real traffic.
fn should_skip_api_request_log(path: &str) -> bool {
    path == "/v1/ws" || path.starts_with("/v1/status")
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
                .map(|info| {
                    crate::http_hardening::client_ip(
                        req.headers(),
                        info.0.ip(),
                        &state.config.security.http,
                    )
                    .to_string()
                });
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
pub(crate) async fn ensure_envelope(
    State(state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let route = req.uri().path().to_owned();
    let method = req.method().as_str().to_owned();
    // Capture the caller tier from the request extensions BEFORE consuming
    // `req` into `next.run`. The tag is set by either `authenticate` (TCP
    // router) or `tag_local` (UDS router) on inner layers; the body limit's
    // 413 short-circuit can fire before the tag-setting layer runs, so this
    // may be `None` — that's fine, the helper defaults to `api`.
    let caller = req.extensions().get::<KeyKind>().copied();
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
    if status == StatusCode::PAYLOAD_TOO_LARGE {
        let source = AppState::event_source_for(caller);
        persist_security_event(
            &state,
            source,
            "warn",
            "security.request_oversized",
            "rejected oversized request body",
            serde_json::json!({
                "route": route,
                "method": method,
                "limit_bytes": state.max_request_bytes,
            }),
        )
        .await;
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

/// Reject disallowed browser HTTP origins before `CorsLayer` handles the
/// request so the denial is both JSON-shaped and durable. WebSocket upgrades
/// are left to `ws_handler`, which publishes `security.ws_origin_denied`.
async fn enforce_http_origin(
    State(state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    if req.uri().path() == "/v1/ws" {
        return next.run(req).await;
    }
    let origin = req
        .headers()
        .get(http::header::ORIGIN)
        .and_then(|value| value.to_str().ok());
    if !crate::http_hardening::origin_allowed(origin, &state.config.security.http) {
        let route = req.uri().path().to_owned();
        let method = req.method().as_str().to_owned();
        let origin_text = origin.unwrap_or("").to_owned();
        persist_security_event(
            &state,
            crate::state::EVENT_SOURCE_API,
            "warn",
            "security.cors_origin_denied",
            "rejected HTTP request with disallowed Origin",
            serde_json::json!({
                "origin": origin_text,
                "route": route,
                "method": method,
            }),
        )
        .await;
        return reject(
            StatusCode::FORBIDDEN,
            "auth.origin_not_allowed",
            "Origin is not in the configured allowlist",
        );
    }
    next.run(req).await
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
    drop(store);
    // Tick the in-memory blocker. If this failure just tripped the threshold,
    // emit a security.ip_block_applied event so operators see the block via
    // GET /v1/logs/security.
    if let Some(ip_str) = client_ip {
        if let Ok(ip) = ip_str.parse::<std::net::IpAddr>() {
            if state.auth_failure_blocker.record_failure(ip) {
                let block_secs = state.auth_failure_blocker.block_duration().as_secs();
                persist_security_event(
                    state,
                    crate::state::EVENT_SOURCE_API,
                    "warn",
                    "security.ip_block_applied",
                    "IP blocked due to repeated auth failures",
                    serde_json::json!({
                        "ip": ip_str,
                        "block_duration_seconds": block_secs,
                        "route": route,
                    }),
                )
                .await;
            }
        }
    }
    Ok(())
}

/// Append a `security.*` event. Best-effort: if the events table write fails,
/// log a warning rather than failing the surrounding request. The event lands
/// on the `logs` WS topic and is merged into `GET /v1/logs/security`.
///
/// `source` lets the caller attribute the event to either the public API
/// (`EVENT_SOURCE_API`) or the local UDS (`EVENT_SOURCE_LOCAL`) so a security
/// event triggered by an acpctl call lands with `source = "local"` to match
/// its `api.request` row.
async fn persist_security_event(
    state: &AppState,
    source: &'static str,
    level: &str,
    kind: &str,
    message: &str,
    data: serde_json::Value,
) {
    let payload = match serde_json::to_string(&data) {
        Ok(text) => text,
        Err(err) => {
            tracing::warn!(error = %err, "failed to serialize security event payload");
            return;
        }
    };
    let store = state.state.lock().await;
    if let Err(err) = store.append_event_with_source(level, kind, source, message, &payload) {
        tracing::warn!(error = %err, kind, "failed to persist security event");
    }
}

fn reject(status: StatusCode, code: &str, message: &str) -> Response {
    ApiError::new(code, message).into_response_with(status)
}

// ----- Handlers -------------------------------------------------------------

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
            adapter: agent.adapter.clone(),
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
pub(crate) struct ConfigExportResponse {
    toml: String,
}

pub(crate) async fn config_export_handler(
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
    crate::config::load_config_from_str(&body)?;
    Ok(ApiSuccess::new(ConfigValidateResponse { valid: true }))
}

#[derive(Serialize)]
struct ConfigImportResponse {
    imported: bool,
    restart_required: bool,
}

/// POST /v1/config/import (admin-tier). Parses TOML from the raw body,
/// rejects auth-ref changes, atomically writes the canonical form to the
/// default config path, and records a `server.config_imported` audit event.
/// The running daemon retains its old `AppState`; the client must restart
/// the daemon for the new config to take effect.
async fn config_import_handler(
    State(state): State<AppState>,
    body: String,
) -> std::result::Result<ApiSuccess<ConfigImportResponse>, StackError> {
    let incoming = crate::config::load_config_from_str(&body)?;
    crate::config::compare_auth_refs(&state.config.auth, &incoming.auth)?;
    let canonical = incoming.to_canonical_toml()?;
    let target = crate::config::default_config_path()?;
    if let Some(parent) = target.parent() {
        crate::fs_util::create_dir_owner_only(parent)?;
    }
    crate::fs_util::atomic_write_owner_only(&target, canonical.as_bytes())?;

    // Audit event: durable record that an import landed. Pin the path so the
    // operator's `acps logs events` shows which file changed. The import has
    // already succeeded on disk, so an event-write failure must not fail the
    // response — but it must also not be silently dropped (CLAUDE.md error
    // rule). Log at warn so monitoring sees the divergence.
    let payload = serde_json::json!({
        "path": target.to_string_lossy(),
        "size_bytes": canonical.len(),
    });
    match serde_json::to_string(&payload) {
        Ok(payload_text) => {
            let store = state.state.lock().await;
            if let Err(err) = store.append_event_with_source(
                "info",
                "server.config_imported",
                crate::state::EVENT_SOURCE_API,
                "config imported via /v1/config/import",
                &payload_text,
            ) {
                tracing::warn!(error = %err, "failed to record server.config_imported audit event");
            }
        }
        Err(err) => {
            tracing::warn!(error = %err, "failed to serialize config-import audit payload");
        }
    }

    Ok(ApiSuccess::new(ConfigImportResponse {
        imported: true,
        restart_required: true,
    }))
}

#[derive(Serialize)]
struct SecretsListResponse {
    names: Vec<String>,
}

#[derive(Deserialize)]
struct SecretsSetBody {
    name: String,
    value: String,
}

#[derive(Serialize)]
struct SecretsSetResponse {
    name: String,
    action: &'static str,
}

#[derive(Serialize)]
struct SecretsDeleteResponse {
    name: String,
    deleted: bool,
}

async fn secrets_list_handler(
    State(_state): State<AppState>,
) -> std::result::Result<ApiSuccess<SecretsListResponse>, StackError> {
    let home = home_dir()?;
    let store = crate::secrets::SecretStore::open(&home)?;
    let names = store.list_names().iter().map(|s| (*s).to_owned()).collect();
    Ok(ApiSuccess::new(SecretsListResponse { names }))
}

async fn secrets_set_handler(
    State(state): State<AppState>,
    Json(body): Json<SecretsSetBody>,
) -> std::result::Result<ApiSuccess<SecretsSetResponse>, StackError> {
    crate::secrets::reject_auth_ref_mutation(&body.name, &state.config)?;
    let home = home_dir()?;
    let mut store = crate::secrets::SecretStore::open(&home)?;
    let action = if store.contains(&body.name) {
        "updated"
    } else {
        "set"
    };
    store.set(&body.name, &body.value)?;
    Ok(ApiSuccess::new(SecretsSetResponse {
        name: body.name,
        action,
    }))
}

async fn secrets_delete_handler(
    Path(name): Path<String>,
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<SecretsDeleteResponse>, StackError> {
    crate::secrets::reject_auth_ref_mutation(&name, &state.config)?;
    let home = home_dir()?;
    let mut store = crate::secrets::SecretStore::open(&home)?;
    store.delete(&name)?;
    Ok(ApiSuccess::new(SecretsDeleteResponse {
        name,
        deleted: true,
    }))
}

/// Per-request cap on `GET /v1/logs/events?limit=`. An authenticated session
/// could otherwise request billions of rows and turn a log query into a
/// memory-pressure attack. Operators with longer-tail queries should page.
const MAX_LOGS_LIMIT: u32 = 1000;

#[derive(Deserialize, Default)]
#[serde(default)]
pub(crate) struct LogsEventsParams {
    #[serde(default = "default_logs_limit")]
    limit: u32,
    level: Option<String>,
    kind: Option<String>,
    source: Option<String>,
    session_id: Option<String>,
    command_id: Option<String>,
    permission_id: Option<String>,
    since: Option<String>,
    until: Option<String>,
    after: Option<String>,
}

fn default_logs_limit() -> u32 {
    100
}

/// Split a `kind` query param into either an exact match or a dotted prefix.
/// A trailing `.` (e.g. `command.`) is treated as a prefix; anything else is
/// matched exactly.
fn split_kind_filter(kind: Option<&str>) -> (Option<&str>, Option<&str>) {
    match kind {
        Some(k) if k.ends_with('.') => (None, Some(k)),
        Some(k) => (Some(k), None),
        None => (None, None),
    }
}

#[derive(Serialize)]
pub(crate) struct LogsEventsResponse {
    events: Vec<LogEventJson>,
    next_cursor: Option<String>,
}

#[derive(Serialize)]
struct LogEventJson {
    id: String,
    created_at: String,
    level: String,
    kind: String,
    message: String,
    payload_json: String,
    source: String,
}

impl From<crate::state::Event> for LogEventJson {
    fn from(e: crate::state::Event) -> Self {
        Self {
            id: e.id,
            created_at: e.created_at,
            level: e.level,
            kind: e.kind,
            message: e.message,
            payload_json: e.payload_json,
            source: e.source,
        }
    }
}

pub(crate) async fn logs_events_handler(
    Query(params): Query<LogsEventsParams>,
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<LogsEventsResponse>, StackError> {
    let limit = params.limit.min(MAX_LOGS_LIMIT);
    let (kind_exact, kind_prefix) = split_kind_filter(params.kind.as_deref());
    let store = state.state.lock().await;
    let events = store.query_events(EventFilter {
        limit,
        level: params.level.as_deref(),
        kind: kind_exact,
        kind_prefix,
        source: params.source.as_deref(),
        session_id: params.session_id.as_deref(),
        command_id: params.command_id.as_deref(),
        permission_id: params.permission_id.as_deref(),
        since: params.since.as_deref(),
        until: params.until.as_deref(),
        after_id: params.after.as_deref(),
    })?;
    drop(store);
    let next_cursor = paging_cursor(&events, limit);
    let events = events.into_iter().map(LogEventJson::from).collect();
    Ok(ApiSuccess::new(LogsEventsResponse {
        events,
        next_cursor,
    }))
}

/// Return the cursor for the next page when `rows` saturated `limit`. When the
/// caller asked for `limit` rows and we returned fewer, there is no next page.
fn paging_cursor<T>(rows: &[T], limit: u32) -> Option<String>
where
    T: HasRowId,
{
    if (rows.len() as u32) < limit {
        return None;
    }
    rows.last().map(|row| row.row_id().to_owned())
}

trait HasRowId {
    fn row_id(&self) -> &str;
}

impl HasRowId for crate::state::Event {
    fn row_id(&self) -> &str {
        &self.id
    }
}

impl HasRowId for crate::state::SessionRecord {
    fn row_id(&self) -> &str {
        &self.id
    }
}

impl HasRowId for crate::state::CommandRecord {
    fn row_id(&self) -> &str {
        &self.id
    }
}

impl HasRowId for crate::state::AuthFailure {
    fn row_id(&self) -> &str {
        &self.id
    }
}

#[derive(Serialize)]
struct LogsSessionsResponse {
    sessions: Vec<SessionLogJson>,
    next_cursor: Option<String>,
}

#[derive(Serialize)]
struct SessionLogJson {
    id: String,
    created_at: String,
    updated_at: String,
    status: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct LogsSessionsParams {
    #[serde(default = "default_logs_limit")]
    limit: u32,
    since: Option<String>,
    until: Option<String>,
    status: Option<String>,
    after: Option<String>,
}

async fn logs_sessions_handler(
    Query(params): Query<LogsSessionsParams>,
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<LogsSessionsResponse>, StackError> {
    let limit = params.limit.min(MAX_LOGS_LIMIT);
    let store = state.state.lock().await;
    let sessions = store.query_sessions(crate::state::SessionFilter {
        limit,
        since: params.since.as_deref(),
        until: params.until.as_deref(),
        status: params.status.as_deref(),
        after_id: params.after.as_deref(),
    })?;
    drop(store);
    let next_cursor = paging_cursor(&sessions, limit);
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
        next_cursor,
    }))
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub(crate) struct LogsLimitParams {
    #[serde(default = "default_logs_limit")]
    limit: u32,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct LogsCommandsParams {
    #[serde(default = "default_logs_limit")]
    limit: u32,
    since: Option<String>,
    until: Option<String>,
    status: Option<String>,
    after: Option<String>,
}

#[derive(Serialize)]
struct LogsCommandsResponse {
    commands: Vec<CommandLogJson>,
    next_cursor: Option<String>,
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
    Query(params): Query<LogsCommandsParams>,
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<LogsCommandsResponse>, StackError> {
    let limit = params.limit.min(MAX_LOGS_LIMIT);
    let store = state.state.lock().await;
    let commands = store.query_commands(crate::state::CommandFilter {
        limit,
        since: params.since.as_deref(),
        until: params.until.as_deref(),
        status: params.status.as_deref(),
        after_id: params.after.as_deref(),
    })?;
    drop(store);
    let next_cursor = paging_cursor(&commands, limit);
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
        next_cursor,
    }))
}

#[derive(Debug, Deserialize)]
pub(crate) struct CommandSubmitRequest {
    command: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    env: Option<std::collections::HashMap<String, String>>,
    #[serde(default, rename = "timeout")]
    timeout_override: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct CommandResponse {
    id: String,
    created_at: String,
    updated_at: String,
    status: String,
    command: String,
    exit_status: Option<i64>,
    started_at: Option<String>,
    finished_at: Option<String>,
    cwd: Option<String>,
    duration_ms: Option<i64>,
    truncated: bool,
}

impl From<crate::state::CommandRecord> for CommandResponse {
    fn from(record: crate::state::CommandRecord) -> Self {
        Self {
            id: record.id,
            created_at: record.created_at,
            updated_at: record.updated_at,
            status: record.status,
            command: record.command,
            exit_status: record.exit_status,
            started_at: record.started_at,
            finished_at: record.finished_at,
            cwd: record.cwd,
            duration_ms: record.duration_ms,
            truncated: record.truncated,
        }
    }
}

#[derive(Debug, Serialize)]
struct CommandsListResponse {
    items: Vec<CommandResponse>,
}

pub(crate) async fn commands_submit_handler(
    State(state): State<AppState>,
    Json(body): Json<CommandSubmitRequest>,
) -> std::result::Result<ApiSuccess<CommandResponse>, StackError> {
    let request = SubmitRequest {
        command: body.command,
        cwd: body.cwd,
        env: body.env,
        timeout_override: body.timeout_override,
    };
    let record = state.commands.submit(request).await?;
    Ok(ApiSuccess::new(record.into()))
}

async fn commands_get_handler(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<CommandResponse>, StackError> {
    let record = state.commands.get(&id).await?;
    Ok(ApiSuccess::new(record.into()))
}

async fn commands_list_handler(
    Query(params): Query<LogsLimitParams>,
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<CommandsListResponse>, StackError> {
    let limit = params.limit.min(MAX_LOGS_LIMIT);
    let records = state.commands.list(limit).await?;
    Ok(ApiSuccess::new(CommandsListResponse {
        items: records.into_iter().map(CommandResponse::from).collect(),
    }))
}

async fn commands_cancel_handler(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<CommandResponse>, StackError> {
    let record = state.commands.cancel(&id).await?;
    Ok(ApiSuccess::new(record.into()))
}

#[derive(Serialize)]
pub(crate) struct PermissionsListResponse {
    permissions: Vec<crate::permissions::PermissionRequestView>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct PermissionApproveBody {
    option_id: Option<String>,
    reason: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct PermissionDenyBody {
    reason: Option<String>,
}

pub(crate) async fn permissions_pending_handler(
    Query(params): Query<LogsLimitParams>,
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<PermissionsListResponse>, StackError> {
    let limit = params.limit.min(MAX_LOGS_LIMIT);
    let permissions = state.permissions.pending(limit).await?;
    Ok(ApiSuccess::new(PermissionsListResponse { permissions }))
}

async fn permissions_get_handler(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<crate::permissions::PermissionRequestView>, StackError> {
    let view = state.permissions.get(&id).await?;
    Ok(ApiSuccess::new(view))
}

async fn permissions_approve_handler(
    Path(id): Path<String>,
    State(state): State<AppState>,
    body: Option<Json<PermissionApproveBody>>,
) -> std::result::Result<ApiSuccess<crate::permissions::PermissionDecisionView>, StackError> {
    let Json(body) = body.unwrap_or_default();
    // The deciding principal is the bearer-token tier. These routes are
    // session-tier (per docs/specs/security.md:20); the principal is always
    // "session-key" and that's what's recorded in `permission_decisions`. If
    // the tier policy ever splits approve vs deny across keys, surface the
    // resolved KeyKind from the request extension here.
    let decision = state
        .permissions
        .approve(&id, body.option_id, body.reason, "session-key")
        .await?;
    Ok(ApiSuccess::new(decision))
}

async fn deps_get_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<crate::deps::DepsReport>, StackError> {
    Ok(ApiSuccess::new(crate::deps::check_dependencies(
        &state.config,
    )))
}

pub(crate) async fn deps_check_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<crate::deps::DepsReport>, StackError> {
    Ok(ApiSuccess::new(crate::deps::check_dependencies(
        &state.config,
    )))
}

async fn permissions_deny_handler(
    Path(id): Path<String>,
    State(state): State<AppState>,
    body: Option<Json<PermissionDenyBody>>,
) -> std::result::Result<ApiSuccess<crate::permissions::PermissionDecisionView>, StackError> {
    let Json(body) = body.unwrap_or_default();
    // Hardcoded "session-key" mirrors `permissions_approve_handler`; see the
    // rationale comment there.
    let decision = state
        .permissions
        .deny(&id, body.reason, "session-key")
        .await?;
    Ok(ApiSuccess::new(decision))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct LogsPermissionsParams {
    #[serde(default = "default_logs_limit")]
    limit: u32,
    kind: Option<String>,
    source: Option<String>,
    since: Option<String>,
    until: Option<String>,
    after: Option<String>,
    permission_id: Option<String>,
}

async fn logs_permissions_handler(
    Query(params): Query<LogsPermissionsParams>,
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<LogsEventsResponse>, StackError> {
    let limit = params.limit.min(MAX_LOGS_LIMIT);
    let (kind_exact, kind_prefix) = split_kind_filter(params.kind.as_deref());
    let store = state.state.lock().await;
    let events = store.query_permission_events(EventFilter {
        limit,
        level: None,
        kind: kind_exact,
        kind_prefix,
        source: params.source.as_deref(),
        session_id: None,
        command_id: None,
        permission_id: params.permission_id.as_deref(),
        since: params.since.as_deref(),
        until: params.until.as_deref(),
        after_id: params.after.as_deref(),
    })?;
    drop(store);
    let next_cursor = paging_cursor(&events, limit);
    let events = events.into_iter().map(LogEventJson::from).collect();
    Ok(ApiSuccess::new(LogsEventsResponse {
        events,
        next_cursor,
    }))
}

#[derive(Serialize)]
struct LogsSecurityResponse {
    auth_failures: Vec<AuthFailureJson>,
    events: Vec<LogEventJson>,
    auth_failures_next_cursor: Option<String>,
    events_next_cursor: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct LogsSecurityParams {
    #[serde(default = "default_logs_limit")]
    limit: u32,
    since: Option<String>,
    until: Option<String>,
    after: Option<String>,
    auth_failures_after: Option<String>,
    events_after: Option<String>,
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

/// `GET /v1/logs/security` returns both `auth_failures` rows (durable record
/// of every rejected authentication) and `events` rows whose `kind` starts
/// with `security.*` (rate-limit hits, IP blocks, denied origins, oversized
/// requests, etc.). Two independent streams keep their existing schemas;
/// clients merge them on `created_at` for a unified timeline.
async fn logs_security_handler(
    Query(params): Query<LogsSecurityParams>,
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<LogsSecurityResponse>, StackError> {
    let limit = params.limit.min(MAX_LOGS_LIMIT);
    let auth_failures_after = params
        .auth_failures_after
        .as_deref()
        .or(params.after.as_deref());
    let events_after = params.events_after.as_deref().or(params.after.as_deref());
    let store = state.state.lock().await;
    let auth_failures = store.query_auth_failures(AuthFailureFilter {
        limit,
        since: params.since.as_deref(),
        until: params.until.as_deref(),
        after_id: auth_failures_after,
    })?;
    let security_events = store.query_security_events(EventFilter {
        limit,
        since: params.since.as_deref(),
        until: params.until.as_deref(),
        after_id: events_after,
        ..EventFilter::default()
    })?;
    drop(store);
    let auth_failures_next_cursor = paging_cursor(&auth_failures, limit);
    let events_next_cursor = paging_cursor(&security_events, limit);
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
        events: security_events
            .into_iter()
            .map(LogEventJson::from)
            .collect(),
        auth_failures_next_cursor,
        events_next_cursor,
    }))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct MetricsSummaryParams {
    /// Window start. Accepts RFC3339 (e.g. `2026-05-16T00:00:00Z`) or a
    /// duration suffix (`1h`, `30m`, `2d`). Defaults to 24h ago.
    since: Option<String>,
    /// Window end. Same format as `since`. Defaults to "now".
    until: Option<String>,
}

#[derive(Serialize)]
struct MetricsSummaryResponse {
    window: MetricsWindowJson,
    counts: MetricsCountsJson,
    sessions: MetricsSessionsJson,
    turns: MetricsTurnsJson,
    commands: MetricsCommandsJson,
    permissions: MetricsPermissionsJson,
    security: MetricsSecurityJson,
    api_connections: MetricsApiConnectionsJson,
    ws_connections: MetricsWsConnectionsJson,
    usage: MetricsUsageJson,
}

#[derive(Serialize)]
struct MetricsWindowJson {
    since: String,
    until: String,
}

#[derive(Serialize)]
struct MetricsCountsJson {
    events: i64,
    sessions: i64,
    commands: i64,
    auth_failures: i64,
    agent_lifecycle: i64,
    installer_runs: i64,
    agent_capabilities: i64,
    prompts: i64,
    permission_requests: i64,
    permission_decisions: i64,
}

#[derive(Serialize)]
struct MetricsSessionsJson {
    active: i64,
    closed: i64,
    average_duration_ms: Option<i64>,
    p50_duration_ms: Option<i64>,
    p95_duration_ms: Option<i64>,
}

#[derive(Serialize)]
struct MetricsTurnsJson {
    total: i64,
    by_status: std::collections::BTreeMap<String, i64>,
    average_per_session: Option<f64>,
}

#[derive(Serialize)]
struct MetricsCommandsJson {
    total: i64,
    by_status: std::collections::BTreeMap<String, i64>,
    average_duration_ms: Option<i64>,
    p50_duration_ms: Option<i64>,
    p95_duration_ms: Option<i64>,
    truncated_count: i64,
}

#[derive(Serialize)]
struct MetricsPermissionsJson {
    total: i64,
    by_outcome: std::collections::BTreeMap<String, i64>,
    average_response_ms: Option<i64>,
    p50_response_ms: Option<i64>,
    p95_response_ms: Option<i64>,
}

#[derive(Serialize)]
struct MetricsSecurityJson {
    auth_failures: i64,
    by_reason: std::collections::BTreeMap<String, i64>,
    events_by_kind: std::collections::BTreeMap<String, i64>,
}

#[derive(Serialize)]
struct MetricsApiConnectionsJson {
    request_count: Option<i64>,
    by_status: std::collections::BTreeMap<String, i64>,
    average_duration_ms: Option<i64>,
}

#[derive(Serialize)]
struct MetricsWsConnectionsJson {
    connections_opened: Option<i64>,
    connections_closed: Option<i64>,
    average_duration_ms: Option<i64>,
}

#[derive(Serialize)]
struct MetricsUsageJson {
    tokens_input: Option<i64>,
    tokens_output: Option<i64>,
    context_window_max: Option<i64>,
}

async fn metrics_summary_handler(
    Query(params): Query<MetricsSummaryParams>,
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<MetricsSummaryResponse>, StackError> {
    let now = chrono::Utc::now();
    let until = match params.until.as_deref() {
        Some(raw) => parse_metrics_bound(raw, now)?,
        None => now,
    };
    let since = match params.since.as_deref() {
        Some(raw) => parse_metrics_bound(raw, now)?,
        None => now - chrono::Duration::hours(24),
    };
    if since > until {
        return Err(StackError::InvalidParam {
            field: "since",
            reason: "must be earlier than until".to_owned(),
        });
    }
    let window = crate::state::MetricsWindow {
        since: format_rfc3339_nanos(since),
        until: format_rfc3339_nanos(until),
    };
    let store = state.state.lock().await;
    let summary = store.metrics_summary(window)?;
    drop(store);
    Ok(ApiSuccess::new(MetricsSummaryResponse::from(summary)))
}

/// Parse either an RFC3339 timestamp or a duration suffix relative to `now`.
/// Accepts `Ns`, `Nm`, `Nh`, `Nd`. The duration suffix subtracts from `now`
/// so callers can pass `since=1h` to mean "an hour ago".
fn parse_metrics_bound(
    raw: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> std::result::Result<chrono::DateTime<chrono::Utc>, StackError> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Ok(dt.with_timezone(&chrono::Utc));
    }
    let duration =
        crate::time_util::parse_duration_suffix(raw).ok_or_else(|| StackError::InvalidParam {
            field: "since/until",
            reason: format!("not a valid RFC3339 timestamp or duration: {raw}"),
        })?;
    Ok(now - duration)
}

fn format_rfc3339_nanos(dt: chrono::DateTime<chrono::Utc>) -> String {
    dt.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

impl From<crate::state::MetricsSummary> for MetricsSummaryResponse {
    fn from(summary: crate::state::MetricsSummary) -> Self {
        Self {
            window: MetricsWindowJson {
                since: summary.window.since,
                until: summary.window.until,
            },
            counts: MetricsCountsJson {
                events: summary.counts.events,
                sessions: summary.counts.sessions,
                commands: summary.counts.commands,
                auth_failures: summary.counts.auth_failures,
                agent_lifecycle: summary.counts.agent_lifecycle,
                installer_runs: summary.counts.installer_runs,
                agent_capabilities: summary.counts.agent_capabilities,
                prompts: summary.counts.prompts,
                permission_requests: summary.counts.permission_requests,
                permission_decisions: summary.counts.permission_decisions,
            },
            sessions: MetricsSessionsJson {
                active: summary.sessions.active,
                closed: summary.sessions.closed,
                average_duration_ms: summary.sessions.average_duration_ms,
                p50_duration_ms: summary.sessions.p50_duration_ms,
                p95_duration_ms: summary.sessions.p95_duration_ms,
            },
            turns: MetricsTurnsJson {
                total: summary.turns.total,
                by_status: summary.turns.by_status,
                average_per_session: summary.turns.average_per_session,
            },
            commands: MetricsCommandsJson {
                total: summary.commands.total,
                by_status: summary.commands.by_status,
                average_duration_ms: summary.commands.average_duration_ms,
                p50_duration_ms: summary.commands.p50_duration_ms,
                p95_duration_ms: summary.commands.p95_duration_ms,
                truncated_count: summary.commands.truncated_count,
            },
            permissions: MetricsPermissionsJson {
                total: summary.permissions.total,
                by_outcome: summary.permissions.by_outcome,
                average_response_ms: summary.permissions.average_response_ms,
                p50_response_ms: summary.permissions.p50_response_ms,
                p95_response_ms: summary.permissions.p95_response_ms,
            },
            security: MetricsSecurityJson {
                auth_failures: summary.security.auth_failures,
                by_reason: summary.security.by_reason,
                events_by_kind: summary.security.events_by_kind,
            },
            api_connections: MetricsApiConnectionsJson {
                request_count: summary.api_connections.request_count,
                by_status: summary.api_connections.by_status,
                average_duration_ms: summary.api_connections.average_duration_ms,
            },
            ws_connections: MetricsWsConnectionsJson {
                connections_opened: summary.ws_connections.connections_opened,
                connections_closed: summary.ws_connections.connections_closed,
                average_duration_ms: summary.ws_connections.average_duration_ms,
            },
            usage: MetricsUsageJson {
                tokens_input: summary.usage.tokens_input,
                tokens_output: summary.usage.tokens_output,
                context_window_max: summary.usage.context_window_max,
            },
        }
    }
}

#[derive(Serialize)]
pub(crate) struct SecurityCheckResponse {
    ok: bool,
    findings: Vec<crate::security::SecurityFinding>,
    auth_failure_count: i64,
}

pub(crate) async fn security_check_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<SecurityCheckResponse>, StackError> {
    let store = state.state.lock().await;
    let counts = store.counts()?;
    let recent_cutoff =
        (Utc::now() - Duration::minutes(1)).to_rfc3339_opts(SecondsFormat::Nanos, true);
    let recent_auth_failures = store.count_auth_failures_since(&recent_cutoff)?;
    drop(store);
    let findings = crate::security::check(crate::security::SecurityCheckInputs {
        effective_bind: state.effective_bind.as_str(),
        http: &state.config.security.http,
        session_key_empty: state.session_key.is_empty(),
        admin_key_empty: state.admin_key.is_empty(),
        recent_auth_failures,
    });
    Ok(ApiSuccess::new(SecurityCheckResponse {
        ok: findings.is_empty(),
        findings,
        auth_failure_count: counts.auth_failures,
    }))
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

/// Resolve every configured `[mcp.servers]` entry into the SDK `McpServer`
/// type. Returns an empty Vec when no MCP servers are configured, so the
/// secret store is only opened when there's something to resolve.
fn open_mcp_servers(config: &Config) -> Result<Vec<agent_client_protocol::schema::McpServer>> {
    if config.mcp.servers.is_empty() {
        return Ok(Vec::new());
    }
    let home = home_dir()?;
    let store = SecretStore::open(&home)?;
    crate::mcp::resolve_mcp_servers(&config.mcp, &store)
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
            Some(state.permissions.clone()),
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
    let exit_status = state
        .agent_supervisor
        .stop(&state.state, &state.event_hub)
        .await?;
    let stopped_at = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
    Ok(ApiSuccess::new(AgentStopResponse {
        stopped_at,
        exit_status,
    }))
}

#[derive(Serialize)]
struct AgentCapabilitiesResponseBody {
    agent_id: String,
    adapter: Option<AgentAdapterConfig>,
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
        adapter: state.config.agent.adapter.clone(),
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
    let sessions = store.query_sessions(crate::state::SessionFilter {
        limit,
        ..Default::default()
    })?;
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
    let mcp_servers = open_mcp_servers(&state.config)?;
    let server_names = crate::mcp::server_names(&mcp_servers);
    let record = state
        .agent_supervisor
        .create_session(
            &state.config.agent,
            &state.config.workspace.root,
            Some(cwd),
            mcp_servers,
            &state.state,
        )
        .await?;
    persist_mcp_attached(&state, &record.id, &server_names).await;
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
    let mcp_servers = open_mcp_servers(&state.config)?;
    let server_names = crate::mcp::server_names(&mcp_servers);
    let record = state
        .agent_supervisor
        .load_session(
            &id,
            cwd,
            mcp_servers,
            &state.config.workspace.root,
            &state.state,
        )
        .await?;
    persist_mcp_attached(&state, &record.id, &server_names).await;
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
    let mcp_servers = open_mcp_servers(&state.config)?;
    let server_names = crate::mcp::server_names(&mcp_servers);
    let record = state
        .agent_supervisor
        .resume_session(
            &id,
            cwd,
            mcp_servers,
            &state.config.workspace.root,
            &state.state,
        )
        .await?;
    persist_mcp_attached(&state, &record.id, &server_names).await;
    Ok(ApiSuccess::new(SessionResponse::from(record)))
}

async fn persist_mcp_attached(state: &AppState, session_id: &str, names: &[String]) {
    if names.is_empty() {
        return;
    }
    let payload = serde_json::json!({
        "session_id": session_id,
        "server_names": names,
    });
    let payload_text = payload.to_string();
    let store = state.state.lock().await;
    if let Err(err) = store.append_session_event_with_source(
        session_id,
        "info",
        "mcp.session_attached",
        crate::state::EVENT_SOURCE_API,
        "mcp servers attached to session",
        &payload_text,
    ) {
        tracing::warn!(error = %err, session_id, "failed to record mcp.session_attached event");
    }
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
        events: events.into_iter().map(LogEventJson::from).collect(),
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
    cancel_pending_acp_permissions_for_session(&state, &id, "session-canceled").await;
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
    cancel_pending_acp_permissions_for_session(&state, &id, "session-closed").await;
    Ok(ApiSuccess::new(SessionResponse::from(record)))
}

/// When a session closes or is canceled, any in-flight ACP-source permission
/// rows for that session must be settled — otherwise the operator UI shows
/// stale "pending" rows that won't resolve until the per-request timer fires
/// (default 5 minutes). The ACP-side prompt-turn is already dead; the durable
/// row should reflect that immediately.
async fn cancel_pending_acp_permissions_for_session(
    state: &AppState,
    session_id: &str,
    reason: &str,
) {
    // Read every pending row, filter by source=acp + subject_id=session.
    // The list is small in practice (one prompt turn at a time); no need to
    // push the filter into SQL.
    let pending = match state.permissions.pending(MAX_LOGS_LIMIT).await {
        Ok(rows) => rows,
        Err(err) => {
            tracing::warn!(error = %err, session_id, "failed to load pending permissions for session close");
            return;
        }
    };
    for row in pending {
        if row.source != "acp" {
            continue;
        }
        if row.subject_id.as_deref() != Some(session_id) {
            continue;
        }
        if let Err(err) = state.permissions.cancel(&row.id, reason).await {
            tracing::warn!(
                error = %err,
                permission_id = %row.id,
                session_id,
                "failed to cancel pending ACP permission on session teardown",
            );
        }
    }
}

// ----- Workspace files ------------------------------------------------------

#[derive(Serialize)]
struct WorkspaceMetadataResponse {
    root: String,
    uploads_path: String,
    default_shell: String,
    max_file_bytes: u64,
}

async fn workspace_metadata_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<WorkspaceMetadataResponse>, StackError> {
    let workspace = &state.config.workspace;
    let uploads_path = workspace_relative_string(&workspace.root, &workspace.uploads);
    Ok(ApiSuccess::new(WorkspaceMetadataResponse {
        root: workspace.root.clone(),
        uploads_path,
        default_shell: workspace.default_shell.clone(),
        max_file_bytes: workspace.max_file_bytes,
    }))
}

#[derive(Deserialize)]
pub(crate) struct FilesPathParams {
    path: String,
}

#[derive(Serialize)]
pub(crate) struct FilesListResponse {
    path: String,
    entries: Vec<FilesListEntry>,
}

#[derive(Serialize)]
struct FilesListEntry {
    name: String,
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
    modified: String,
}

pub(crate) async fn files_list_handler(
    State(state): State<AppState>,
    Query(params): Query<FilesPathParams>,
) -> std::result::Result<ApiSuccess<FilesListResponse>, StackError> {
    let root = state.config.workspace.root.clone();
    let requested = params.path.clone();
    let listing: WorkspaceListing = tokio::task::spawn_blocking(move || {
        let absolute = resolve_workspace_path(
            std::path::Path::new(&root),
            &requested,
            PathIntent::ReadExisting,
        )?;
        workspace::list_directory(&absolute)
    })
    .await
    .map_err(spawn_blocking_to_io)??;
    Ok(ApiSuccess::new(FilesListResponse {
        path: params.path,
        entries: listing
            .entries
            .into_iter()
            .map(|entry| FilesListEntry {
                name: entry.name,
                kind: entry_kind_to_str(entry.kind).to_owned(),
                size: entry.size,
                modified: entry.modified.to_rfc3339(),
            })
            .collect(),
    }))
}

#[derive(Serialize)]
pub(crate) struct FilesContentResponse {
    path: String,
    encoding: String,
    content: String,
    size: u64,
    modified: String,
}

pub(crate) async fn files_content_get_handler(
    State(state): State<AppState>,
    Query(params): Query<FilesPathParams>,
) -> std::result::Result<ApiSuccess<FilesContentResponse>, StackError> {
    let root = state.config.workspace.root.clone();
    let requested = params.path.clone();
    let max_bytes = state.config.workspace.max_file_bytes;
    let read: FileRead = tokio::task::spawn_blocking(move || {
        let absolute = resolve_workspace_path(
            std::path::Path::new(&root),
            &requested,
            PathIntent::ReadExisting,
        )?;
        workspace::read_file(&absolute, max_bytes)
    })
    .await
    .map_err(spawn_blocking_to_io)??;
    let (encoding, content) = encode_file_content(&read.content);
    Ok(ApiSuccess::new(FilesContentResponse {
        path: params.path,
        encoding: encoding.to_owned(),
        content,
        size: read.size,
        modified: read.modified.to_rfc3339(),
    }))
}

async fn files_download_handler(
    State(state): State<AppState>,
    Query(params): Query<FilesPathParams>,
) -> std::result::Result<Response, StackError> {
    let root = state.config.workspace.root.clone();
    let requested = params.path.clone();
    let max_bytes = state.config.workspace.max_file_bytes;
    let read: FileRead = tokio::task::spawn_blocking(move || {
        let absolute = resolve_workspace_path(
            std::path::Path::new(&root),
            &requested,
            PathIntent::ReadExisting,
        )?;
        workspace::read_file(&absolute, max_bytes)
    })
    .await
    .map_err(spawn_blocking_to_io)??;
    let filename = std::path::Path::new(&params.path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".to_owned());
    let disposition = format!(
        "attachment; filename=\"{}\"",
        sanitize_disposition_filename(&filename)
    );
    let response = Response::builder()
        .status(StatusCode::OK)
        .header(http::header::CONTENT_TYPE, "application/octet-stream")
        .header(http::header::CONTENT_LENGTH, read.size)
        .header(http::header::CONTENT_DISPOSITION, disposition)
        .body(Body::from(read.content))
        .map_err(|_| StackError::WorkspaceIo {
            requested: params.path.clone(),
            source: std::io::Error::other("failed to build download response"),
        })?;
    Ok(response)
}

#[derive(Deserialize)]
pub(crate) struct FilesContentPutBody {
    path: String,
    encoding: String,
    content: String,
}

pub(crate) async fn files_content_put_handler(
    State(state): State<AppState>,
    Extension(kind): Extension<KeyKind>,
    Json(body): Json<FilesContentPutBody>,
) -> std::result::Result<ApiSuccess<FileMutationResponse>, StackError> {
    let bytes = decode_request_content(&body.encoding, &body.content)?;
    let max_bytes = state.config.workspace.max_file_bytes;
    if bytes.len() as u64 > max_bytes {
        return Err(StackError::WorkspaceTooLarge { limit: max_bytes });
    }
    let root = state.config.workspace.root.clone();
    let requested = body.path.clone();
    let metadata: FileMetadata = tokio::task::spawn_blocking(move || {
        let absolute = resolve_workspace_path(
            std::path::Path::new(&root),
            &requested,
            PathIntent::WriteOrCreate,
        )?;
        workspace::write_file_atomic(&absolute, &bytes)
    })
    .await
    .map_err(spawn_blocking_to_io)??;

    publish_workspace_mutation(
        &state,
        kind,
        "workspace.write",
        &body.path,
        Some(metadata.size),
    )
    .await?;

    Ok(ApiSuccess::new(FileMutationResponse {
        path: body.path,
        size: metadata.size,
        modified: metadata.modified.to_rfc3339(),
    }))
}

async fn files_upload_handler(
    State(state): State<AppState>,
    Extension(kind): Extension<KeyKind>,
    mut multipart: Multipart,
) -> std::result::Result<ApiSuccess<FileUploadResponse>, StackError> {
    let mut path: Option<String> = None;
    let mut filename: Option<String> = None;
    let mut content: Option<Vec<u8>> = None;

    while let Some(field) = multipart.next_field().await.map_err(|err| {
        tracing::debug!(error = %err, "rejecting malformed multipart upload");
        StackError::WorkspaceUploadInvalid {
            reason: "multipart body is malformed",
        }
    })? {
        match field.name() {
            Some("path") => {
                path =
                    Some(
                        field
                            .text()
                            .await
                            .map_err(|_| StackError::WorkspaceUploadInvalid {
                                reason: "multipart `path` field could not be read as text",
                            })?,
                    );
            }
            Some("file") => {
                filename = field.file_name().map(|s| s.to_owned());
                // Stream chunks instead of buffering the whole part: the HTTP
                // body cap (api.max_request_bytes) may be larger than
                // workspace.max_file_bytes, and we want to stop accumulating
                // bytes the moment we cross the per-file limit instead of
                // letting an authenticated client push the bigger cap of
                // memory through this handler.
                let max_bytes = state.config.workspace.max_file_bytes;
                let mut buffer: Vec<u8> = Vec::new();
                let mut field = field;
                loop {
                    match field.chunk().await {
                        Ok(Some(chunk)) => {
                            if (buffer.len() as u64).saturating_add(chunk.len() as u64) > max_bytes
                            {
                                return Err(StackError::WorkspaceTooLarge { limit: max_bytes });
                            }
                            buffer.extend_from_slice(&chunk);
                        }
                        Ok(None) => break,
                        Err(_) => {
                            return Err(StackError::WorkspaceUploadInvalid {
                                reason: "multipart `file` field could not be read",
                            });
                        }
                    }
                }
                content = Some(buffer);
            }
            _ => {}
        }
    }

    let path = path.ok_or(StackError::WorkspaceUploadInvalid {
        reason: "multipart upload is missing the required `path` field",
    })?;
    let content = content.ok_or(StackError::WorkspaceUploadInvalid {
        reason: "multipart upload is missing the required `file` field",
    })?;
    let filename = filename.unwrap_or_default();

    let max_bytes = state.config.workspace.max_file_bytes;
    if content.len() as u64 > max_bytes {
        return Err(StackError::WorkspaceTooLarge { limit: max_bytes });
    }

    // Resolution against `workspace.root` (not against `workspace.uploads`)
    // even though the request path is uploads-relative. This means a symlink
    // at `workspace.uploads` that points outside the root gets caught by the
    // resolver's canonicalize-and-starts_with check; resolving directly under
    // `workspace.uploads` would treat it as its own containment root and let
    // an escape slip through. The config validator already rejects an
    // `uploads` path that is not lexically under `root`.
    if std::path::Path::new(&path).is_absolute() {
        return Err(StackError::WorkspacePathInvalid {
            reason: "upload `path` must be relative to workspace.uploads".to_owned(),
            requested: path,
        });
    }
    let workspace_relative_path = join_workspace_relative(
        &state.config.workspace.root,
        &state.config.workspace.uploads,
        &path,
    );
    let workspace_root = state.config.workspace.root.clone();
    let target_relative = workspace_relative_path.clone();
    let bytes = content;
    let metadata: FileMetadata = tokio::task::spawn_blocking(move || {
        let absolute = resolve_workspace_path(
            std::path::Path::new(&workspace_root),
            &target_relative,
            PathIntent::WriteOrCreate,
        )?;
        workspace::write_file_atomic(&absolute, &bytes)
    })
    .await
    .map_err(spawn_blocking_to_io)??;

    publish_workspace_mutation(
        &state,
        kind,
        "workspace.upload",
        &workspace_relative_path,
        Some(metadata.size),
    )
    .await?;

    Ok(ApiSuccess::new(FileUploadResponse {
        path: workspace_relative_path,
        filename,
        size: metadata.size,
        modified: metadata.modified.to_rfc3339(),
    }))
}

async fn files_delete_handler(
    State(state): State<AppState>,
    Extension(kind): Extension<KeyKind>,
    Query(params): Query<FilesPathParams>,
) -> std::result::Result<ApiSuccess<FileDeleteResponse>, StackError> {
    let root = state.config.workspace.root.clone();
    let requested = params.path.clone();
    tokio::task::spawn_blocking(move || {
        let absolute = resolve_workspace_path(
            std::path::Path::new(&root),
            &requested,
            PathIntent::WriteOrCreate,
        )?;
        workspace::delete_file(&absolute)
    })
    .await
    .map_err(spawn_blocking_to_io)??;

    publish_workspace_mutation(&state, kind, "workspace.delete", &params.path, None).await?;

    Ok(ApiSuccess::new(FileDeleteResponse {
        path: params.path,
        deleted: true,
    }))
}

fn decode_request_content(
    encoding: &str,
    content: &str,
) -> std::result::Result<Vec<u8>, StackError> {
    match encoding {
        "utf8" => Ok(content.as_bytes().to_vec()),
        "base64" => {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD
                .decode(content)
                .map_err(|_| StackError::WorkspaceEncodingInvalid {
                    reason: "content is not valid base64",
                })
        }
        _ => Err(StackError::WorkspaceEncodingInvalid {
            reason: "encoding must be `utf8` or `base64`",
        }),
    }
}

/// Compose the workspace-relative path for an upload destination. The upload
/// request's `path` is interpreted relative to `workspace.uploads`; this helper
/// joins `uploads`'s workspace-relative form with the request path so callers
/// can read the file back via the read routes.
fn join_workspace_relative(workspace_root: &str, uploads_root: &str, request_path: &str) -> String {
    let uploads_rel = workspace_relative_string(workspace_root, uploads_root);
    let trimmed = request_path.trim_start_matches('/');
    if uploads_rel.is_empty() {
        trimmed.to_owned()
    } else if trimmed.is_empty() {
        uploads_rel
    } else {
        format!("{uploads_rel}/{trimmed}")
    }
}

async fn publish_workspace_mutation(
    state: &AppState,
    caller: KeyKind,
    event_kind: &str,
    path: &str,
    size: Option<u64>,
) -> std::result::Result<(), StackError> {
    let mut data = serde_json::json!({ "path": path });
    if let Some(size) = size {
        if let Some(obj) = data.as_object_mut() {
            obj.insert(
                "size".to_owned(),
                serde_json::Value::Number(serde_json::Number::from(size)),
            );
        }
    }
    let payload_json = serde_json::to_string(&data).map_err(|_| StackError::WorkspaceIo {
        requested: path.to_owned(),
        source: std::io::Error::other("failed to serialize workspace event payload"),
    })?;
    let event = {
        let store = state.state.lock().await;
        // `message` is empty: the kind + payload carry the structured detail,
        // and we want sanitized logs that do not echo user paths into the
        // text column (`logs/events` is session-tier-readable).
        store.append_event_with_source(
            "info",
            event_kind,
            AppState::event_source_for(Some(caller)),
            "",
            &payload_json,
        )?
    };
    state.event_hub.publish_workspace_event(&event, data);
    Ok(())
}

#[derive(Serialize)]
pub(crate) struct FileMutationResponse {
    path: String,
    size: u64,
    modified: String,
}

#[derive(Serialize)]
struct FileUploadResponse {
    path: String,
    filename: String,
    size: u64,
    modified: String,
}

#[derive(Serialize)]
struct FileDeleteResponse {
    path: String,
    deleted: bool,
}

fn entry_kind_to_str(kind: workspace::EntryKind) -> &'static str {
    match kind {
        workspace::EntryKind::File => "file",
        workspace::EntryKind::Directory => "directory",
        workspace::EntryKind::Symlink => "symlink",
        workspace::EntryKind::Other => "other",
    }
}

fn encode_file_content(bytes: &[u8]) -> (&'static str, String) {
    match std::str::from_utf8(bytes) {
        Ok(text) => ("utf8", text.to_owned()),
        Err(_) => {
            use base64::Engine as _;
            (
                "base64",
                base64::engine::general_purpose::STANDARD.encode(bytes),
            )
        }
    }
}

/// `workspace.root` and `workspace.uploads` are both absolute paths in
/// config. Most callers want the uploads path expressed as workspace-relative
/// so they can use it directly with `/v1/files*` routes.
fn workspace_relative_string(root: &str, absolute: &str) -> String {
    let root = std::path::Path::new(root);
    let absolute = std::path::Path::new(absolute);
    match absolute.strip_prefix(root) {
        Ok(rel) => rel.to_string_lossy().into_owned(),
        Err(_) => absolute.display().to_string(),
    }
}

/// `Content-Disposition` filename values are quoted strings; backslash and
/// double-quote must be escaped, and bare control chars are not allowed.
/// Non-ASCII characters are dropped here to stay inside the simple
/// `filename="..."` form. Clients that need exact non-ASCII filenames should
/// rely on the response body, not the header.
fn sanitize_disposition_filename(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        match c {
            '\\' | '"' => {
                out.push('\\');
                out.push(c);
            }
            c if c.is_ascii() && !c.is_control() => out.push(c),
            _ => out.push('_'),
        }
    }
    out
}

/// A panic in `spawn_blocking` should propagate as a 500; the join failure is
/// strictly an internal fault, so we surface a generic `WorkspaceIo` rather
/// than a path-specific code.
fn spawn_blocking_to_io(error: tokio::task::JoinError) -> StackError {
    StackError::WorkspaceIo {
        requested: "<background task>".to_owned(),
        source: std::io::Error::other(error.to_string()),
    }
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
