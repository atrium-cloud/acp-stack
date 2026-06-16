mod core;

pub(crate) mod auth;
pub(crate) mod routes;
pub(crate) mod ws;
pub(crate) mod ws_registry;

pub(crate) use auth::{ensure_envelope, log_api_request, track_active_requests};
pub(crate) use core::shutdown_signal;
pub use core::{AppState, RuntimePaths, build_router, serve};
pub(crate) use routes::metrics::metrics_summary_handler;
pub(crate) use routes::security::security_check_handler;
pub(crate) use routes::sessions::{sessions_list_handler, sessions_status_handler};
pub(crate) use routes::status::{health_ready_handler, status_agent_handler, status_handler};
pub(crate) use routes::ws::{ws_connections_handler, ws_sessions_handler};
