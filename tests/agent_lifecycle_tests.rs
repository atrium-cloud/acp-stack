#![cfg(feature = "test-fixtures")]

//! End-to-end coverage for the agent lifecycle HTTP routes, driving a real `acps` HTTP server
//! against the standalone placebo ACP fixture.

mod common;

#[path = "agent_lifecycle_tests/array_auth.rs"]
mod array_auth;
#[path = "agent_lifecycle_tests/config_refresh.rs"]
mod config_refresh;
#[path = "agent_lifecycle_tests/crash_restart.rs"]
mod crash_restart;
#[path = "agent_lifecycle_tests/providers_models.rs"]
mod providers_models;
#[path = "agent_lifecycle_tests/startup.rs"]
mod startup;
