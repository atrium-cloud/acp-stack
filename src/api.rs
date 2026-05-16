mod core;

pub(crate) mod auth;
pub(crate) mod routes;
pub(crate) mod ws;

pub(crate) use auth::{ensure_envelope, log_api_request, track_active_requests};
pub(crate) use core::shutdown_signal;
pub use core::{AppState, build_router, serve};
pub(crate) use routes::commands::commands_submit_handler;
pub(crate) use routes::config::config_export_handler;
pub(crate) use routes::deps::deps_check_handler;
pub(crate) use routes::logs::logs_events_handler;
pub(crate) use routes::permissions::permissions_pending_handler;
pub(crate) use routes::security::security_check_handler;
pub(crate) use routes::status::status_handler;
pub(crate) use routes::workspace::{
    files_content_get_handler, files_content_put_handler, files_list_handler,
};
