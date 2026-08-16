#![cfg(all(feature = "dev-tools", feature = "test-fixtures"))]

mod common;

// A Cargo integration-test crate root owns `tests/`, not `tests/<crate>/`, so the
// group modules need explicit paths.
#[path = "cli_security_sessions_tests/metrics_ws.rs"]
mod metrics_ws;
#[path = "cli_security_sessions_tests/security.rs"]
mod security;
#[path = "cli_security_sessions_tests/sessions_auth.rs"]
mod sessions_auth;
#[path = "cli_security_sessions_tests/sessions_status_prompt.rs"]
mod sessions_status_prompt;
