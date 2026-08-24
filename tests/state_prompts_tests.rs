mod common;

// A Cargo integration-test crate root owns `tests/`, so group modules need explicit paths.
#[path = "state_prompts_tests/permissions.rs"]
mod permissions;
#[path = "state_prompts_tests/restart_blockers.rs"]
mod restart_blockers;
#[path = "state_prompts_tests/stalled_stuck.rs"]
mod stalled_stuck;
#[path = "state_prompts_tests/taxonomy.rs"]
mod taxonomy;
