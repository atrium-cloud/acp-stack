#![cfg(all(feature = "dev-tools", feature = "test-fixtures"))]

mod common;

#[path = "cli_observability_tests/support.rs"]
mod support;

#[path = "cli_observability_tests/agent_commands.rs"]
mod agent_commands;
#[path = "cli_observability_tests/array.rs"]
mod array;
#[path = "cli_observability_tests/basics_config.rs"]
mod basics_config;
#[path = "cli_observability_tests/init_status.rs"]
mod init_status;
#[path = "cli_observability_tests/installer_deps.rs"]
mod installer_deps;
#[path = "cli_observability_tests/logs_errors.rs"]
mod logs_errors;
#[path = "cli_observability_tests/permissions_repair.rs"]
mod permissions_repair;
