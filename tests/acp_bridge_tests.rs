#![cfg(feature = "test-fixtures")]

//! Drives `AcpBridge::spawn` against the placebo ACP fixture so the spawn and
//! handshake path is exercised without a third-party agent.

#[path = "acp_bridge_tests/support.rs"]
mod support;

#[path = "acp_bridge_tests/capability_matrix.rs"]
mod capability_matrix;
#[path = "acp_bridge_tests/filesystem.rs"]
mod filesystem;
#[path = "acp_bridge_tests/sessions.rs"]
mod sessions;
#[path = "acp_bridge_tests/sink_ordering.rs"]
mod sink_ordering;
#[path = "acp_bridge_tests/spawn.rs"]
mod spawn;
#[path = "acp_bridge_tests/terminal.rs"]
mod terminal;
