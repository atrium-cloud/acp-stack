#![cfg(feature = "test-fixtures")]

//! Read-side coverage for the session routes: list (agent sync, time bounds,
//! target resolution), the compact `-/status` summary, snapshot/changes, and
//! the not-found / unsupported-capability / auth-tier error paths.
//!
//! The placebo ACP fixture stands in for a real ACP agent;
//! `tests/acp_bridge_tests.rs` exercises the lower-level bridge layer.

mod common;

// A Cargo integration-test crate root owns `tests/`, not `tests/<crate>/`, so the
// group modules need explicit paths.
#[path = "sessions_query_tests/commands.rs"]
mod commands;
#[path = "sessions_query_tests/events_snapshots.rs"]
mod events_snapshots;
#[path = "sessions_query_tests/lifecycle.rs"]
mod lifecycle;
#[path = "sessions_query_tests/list.rs"]
mod list;
#[path = "sessions_query_tests/status.rs"]
mod status;
