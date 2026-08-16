#![cfg(all(feature = "dev-tools", feature = "test-fixtures"))]

mod common;

#[path = "cli_init_provider_tests/support.rs"]
mod support;

#[path = "cli_init_provider_tests/agent_specific.rs"]
mod agent_specific;
#[path = "cli_init_provider_tests/custom_provider.rs"]
mod custom_provider;
#[path = "cli_init_provider_tests/model_flag.rs"]
mod model_flag;
#[path = "cli_init_provider_tests/modes.rs"]
mod modes;
#[path = "cli_init_provider_tests/prerequisites.rs"]
mod prerequisites;
#[path = "cli_init_provider_tests/resume.rs"]
mod resume;
#[path = "cli_init_provider_tests/rollback_rejection.rs"]
mod rollback_rejection;
