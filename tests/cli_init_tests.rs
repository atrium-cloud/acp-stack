#![cfg(all(feature = "dev-tools", feature = "test-fixtures"))]

mod common;

#[path = "cli_init_tests/auto_update.rs"]
mod auto_update;
#[path = "cli_init_tests/basic_mcp.rs"]
mod basic_mcp;
#[path = "cli_init_tests/custom_agent.rs"]
mod custom_agent;
#[path = "cli_init_tests/deps_updates.rs"]
mod deps_updates;
#[path = "cli_init_tests/mcp_validation.rs"]
mod mcp_validation;
#[path = "cli_init_tests/skills_flags.rs"]
mod skills_flags;
#[path = "cli_init_tests/workspace_edge.rs"]
mod workspace_edge;
