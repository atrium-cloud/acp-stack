use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::middleware::Next;
use axum::response::Response;
use http::StatusCode;
use http::header::AUTHORIZATION;

use super::core::AppState;
use crate::auth::{AuthFailureReason, KeyKind, constant_time_eq, record_auth_failure};
use crate::envelope::ApiError;

// ----- Middleware -----------------------------------------------------------

/// Extract `Authorization: Bearer <key>`, classify it against the cached
/// session / admin keys, tag the request with the resolved `KeyKind`. On any
/// failure, write an `auth_failures` row and return 401.
pub(super) async fn authenticate(
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
pub(super) async fn require_session(
    State(state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    enforce_tier(KeyKind::Session, state, req, next).await
}

pub(super) async fn require_admin(
    State(state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Response {
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
pub(super) async fn enforce_http_origin(
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
pub(super) async fn persist_security_event(
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

pub(super) fn reject(status: StatusCode, code: &str, message: &str) -> Response {
    ApiError::new(code, message).into_response_with(status)
}

// ----- Tests ----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::to_bytes;
    use axum::middleware;
    use axum::routing::get;
    use http::Method;
    use tower::ServiceExt;

    use crate::config::Config;
    use crate::state::StateStore;

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
        let toml_text = include_str!("../../tests/fixtures/valid-acp-stack.toml");
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
