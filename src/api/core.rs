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

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::middleware;
use axum::routing::{get, post};
use tokio::net::TcpListener;
use tokio::sync::Mutex as TokioMutex;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;
use zeroize::Zeroizing;

use super::auth::{
    authenticate, enforce_http_origin, ensure_envelope, log_api_request, require_admin,
    require_session, track_active_requests,
};
use super::routes::agent::{
    agent_capabilities_handler, agent_install_handler, agent_restart_handler, agent_start_handler,
    agent_stop_handler,
};
use super::routes::commands::{
    commands_cancel_handler, commands_get_handler, commands_list_handler, commands_submit_handler,
};
use super::routes::config::{
    config_export_handler, config_import_handler, config_validate_handler, secrets_delete_handler,
    secrets_list_handler, secrets_set_handler,
};
use super::routes::deps::{deps_check_handler, deps_get_handler};
use super::routes::logs::{
    logs_commands_handler, logs_events_handler, logs_permissions_handler, logs_security_handler,
    logs_sessions_handler,
};
use super::routes::metrics::metrics_summary_handler;
use super::routes::permissions::{
    permissions_approve_handler, permissions_deny_handler, permissions_get_handler,
    permissions_pending_handler,
};
use super::routes::providers::{models_handler, providers_handler};
use super::routes::security::security_check_handler;
use super::routes::sessions::{
    sessions_cancel_handler, sessions_close_handler, sessions_create_handler,
    sessions_events_handler, sessions_get_handler, sessions_list_handler, sessions_load_handler,
    sessions_prompt_handler, sessions_prompt_status_handler, sessions_resume_handler,
};
use super::routes::status::{status_agent_handler, status_connections_handler, status_handler};
use super::routes::workspace::{
    files_content_get_handler, files_content_put_handler, files_delete_handler,
    files_download_handler, files_list_handler, files_upload_handler, workspace_metadata_handler,
};
use super::routes::ws::{
    ws_connections_handler, ws_disconnect_connections_handler, ws_disconnect_sessions_handler,
    ws_sessions_handler,
};
use super::ws::ws_handler;
use crate::auth::KeyKind;
use crate::commands::CommandGateway;
use crate::config::Config;
use crate::error::{Result, StackError};
use crate::events::EventHub;
use crate::permissions::PermissionService;
use crate::state::StateStore;
use crate::supervisor::AgentSupervisor;

/// Shared handler/middleware state. Cheap to clone (Arc-only inside).
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    /// Mutable agent-block cache. Initialized from `config.agent` at
    /// startup and updated by `POST /v1/agent/restart` after it reads
    /// the freshly-on-disk config. Handlers that pass session-affecting
    /// agent fields (`agent.model`, `agent.mode`, `agent.provider`)
    /// into the supervisor read from this so a post-restart session
    /// creation honors the new values; handlers that only read static
    /// fields (`agent.id`, install spec, adapter metadata) can keep
    /// using `config.agent` since those don't change on a model swap.
    pub live_agent_config: Arc<TokioMutex<crate::config::AgentConfig>>,
    pub effective_bind: Arc<String>,
    pub runtime_paths: Arc<RuntimePaths>,
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
    pub ws_registry: Arc<super::ws_registry::WsRegistry>,
}

#[derive(Clone, Debug)]
pub struct RuntimePaths {
    pub config_path: PathBuf,
    pub state_path: PathBuf,
}

impl RuntimePaths {
    pub fn new(config_path: PathBuf, state_path: PathBuf) -> Self {
        Self {
            config_path,
            state_path,
        }
    }

    fn from_state_defaults(state: &StateStore) -> Self {
        let config_path = crate::config::default_config_path()
            .unwrap_or_else(|_| PathBuf::from(".config/acp-stack/acp-stack.toml"));
        Self::new(config_path, state.path().to_path_buf())
    }
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
        state: StateStore,
        session_key: String,
        admin_key: String,
        effective_bind: String,
    ) -> Self {
        let runtime_paths = RuntimePaths::from_state_defaults(&state);
        Self::with_effective_bind_and_runtime_paths(
            config,
            state,
            session_key,
            admin_key,
            effective_bind,
            runtime_paths,
        )
    }

    pub fn with_effective_bind_and_runtime_paths(
        mut config: Config,
        mut state: StateStore,
        session_key: String,
        admin_key: String,
        effective_bind: String,
        runtime_paths: RuntimePaths,
    ) -> Self {
        // Adapter metadata is runtime-populated from the active registry.
        // We resolve once at AppState construction so every handler that
        // reads `config.agent.adapter` (status, capabilities, etc.) sees
        // the same value. Failures here are non-fatal: an agent whose id is
        // unknown to the registry simply has no adapter metadata, which is
        // the correct outcome for native agents and operator escape hatches.
        if config.agent.adapter.is_none() {
            if let Ok(registry) = load_active_registry() {
                populate_agent_adapter_from_registry(&mut config, &registry);
            }
        }
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
        let ws_registry = Arc::new(super::ws_registry::WsRegistry::default());
        let live_agent_config = Arc::new(TokioMutex::new(config_arc.agent.clone()));
        Self {
            config: config_arc,
            live_agent_config,
            effective_bind: Arc::new(effective_bind),
            runtime_paths: Arc::new(runtime_paths),
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
            ws_registry,
        }
    }
}

fn load_active_registry() -> Result<crate::agent_registry::RegistryCatalog> {
    match operator_registry_override_path() {
        Some(path) => crate::agent_registry::RegistryCatalog::load_with_override(&path),
        None => crate::agent_registry::RegistryCatalog::load_embedded(),
    }
}

fn operator_registry_override_path() -> Option<PathBuf> {
    crate::fs_util::home_dir()
        .ok()
        .map(|home| registry_override_path(&home))
}

fn registry_override_path(home: &Path) -> PathBuf {
    home.join(".config").join("acp-stack").join("agents.toml")
}

fn populate_agent_adapter_from_registry(
    config: &mut Config,
    registry: &crate::agent_registry::RegistryCatalog,
) {
    if let Some(entry) = registry.lookup(&config.agent.id) {
        if matches!(entry.kind, crate::agent_registry::RegistryKind::Adapter) {
            if let (Some(harness), Some(adapter)) = (&entry.harness, &entry.adapter) {
                config.agent.adapter = Some(crate::config::AgentAdapterConfig {
                    id: adapter.id.clone(),
                    name: entry.name.clone(),
                    upstream_agent: harness.id.clone(),
                    source_url: adapter.github.as_deref().and_then(|github| {
                        crate::agent_registry::github_url_from_value(
                            &entry.id,
                            "adapter.github",
                            github,
                        )
                        .ok()
                    }),
                });
            }
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
        .route("/v1/ws/connections", get(ws_connections_handler))
        .route("/v1/ws/sessions", get(ws_sessions_handler))
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
        // `/v1/providers` is pure embedded-mapping lookup. `/v1/models`
        // spawns a bounded provisional ACP session for picker data; both
        // are session-tier discovery surfaces.
        .route("/v1/providers", get(providers_handler))
        .route("/v1/models", get(models_handler))
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
        .route(
            "/v1/ws/connections/disconnect",
            post(ws_disconnect_connections_handler),
        )
        .route(
            "/v1/ws/sessions/disconnect",
            post(ws_disconnect_sessions_handler),
        )
        .route("/v1/agent/install", post(agent_install_handler))
        .route("/v1/agent/start", post(agent_start_handler))
        .route("/v1/agent/stop", post(agent_stop_handler))
        .route("/v1/agent/restart", post(agent_restart_handler))
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
pub(crate) async fn shutdown_signal() {
    // Non-unix hosts (tests, dev on Windows): only Ctrl-C is wired.
    if let Err(err) = tokio::signal::ctrl_c().await {
        tracing::warn!(error = %err, "ctrl-c handler install failed");
        std::future::pending::<()>().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_metadata_can_be_populated_from_override_registry() {
        let mut config = crate::config::load_config_from_str(include_str!(
            "../../tests/fixtures/valid-acp-stack.toml"
        ))
        .expect("fixture parses");
        let registry = crate::agent_registry::RegistryCatalog::from_toml(
            r#"
[[agents]]
id = "opencode"
name = "Private OpenCode Adapter"
kind = "adapter"
headless_compatible = true
support_doc = "docs/agents/private-opencode.md"

[agents.adapter]
id = "opencode-acp"
github = "example/opencode-acp"

[agents.adapter.install.npm]
package = "@private/opencode-acp"
creates = "opencode-acp"

[agents.harness]
id = "private-opencode"

[agents.harness.install.npm]
package = "@private/opencode"
creates = "private-opencode"
"#,
        )
        .expect("override registry parses");

        populate_agent_adapter_from_registry(&mut config, &registry);

        let adapter = config.agent.adapter.expect("adapter metadata populated");
        assert_eq!(adapter.id, "opencode-acp");
        assert_eq!(adapter.name, "Private OpenCode Adapter");
        assert_eq!(adapter.upstream_agent, "private-opencode");
        assert_eq!(
            adapter.source_url.as_deref(),
            Some("https://github.com/example/opencode-acp")
        );
    }
}
