#![cfg(feature = "test-fixtures")]

//! Read-side coverage for the session routes, against the placebo ACP fixture.

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
