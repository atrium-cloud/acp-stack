#![cfg(feature = "test-fixtures")]

//! End-to-end coverage for `POST /v1/agent/switch` and the native-config
//! import routes: target install, skill porting, provider-secret migration,
//! source cleanup, and the inspect/import/cancel rollback loop.
//!
//! All tests drive a real `acps` HTTP server against a `Config` whose
//! `[agent].command` is the standalone placebo ACP fixture.

mod common;

#[path = "agent_switch_tests/array_targets.rs"]
mod array_targets;
#[path = "agent_switch_tests/native_config.rs"]
mod native_config;
#[path = "agent_switch_tests/secrets_drop.rs"]
mod secrets_drop;
#[path = "agent_switch_tests/skills.rs"]
mod skills;
#[path = "agent_switch_tests/switch_install.rs"]
mod switch_install;
