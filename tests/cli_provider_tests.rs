#![cfg(all(feature = "dev-tools", feature = "test-fixtures"))]

mod common;

#[path = "cli_provider_tests/claude_code.rs"]
mod claude_code;
#[path = "cli_provider_tests/codex.rs"]
mod codex;
#[path = "cli_provider_tests/efforts.rs"]
mod efforts;
#[path = "cli_provider_tests/install_registry.rs"]
mod install_registry;
#[path = "cli_provider_tests/modes.rs"]
mod modes;
#[path = "cli_provider_tests/per_agent.rs"]
mod per_agent;
#[path = "cli_provider_tests/provider_use.rs"]
mod provider_use;
#[path = "cli_provider_tests/subagent_free.rs"]
mod subagent_free;
#[path = "cli_provider_tests/subagent_set.rs"]
mod subagent_set;
