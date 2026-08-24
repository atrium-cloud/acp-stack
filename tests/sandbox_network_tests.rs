//! Behavior of the network-isolation supervisor (`acps __sandbox-supervise`).
//!
//! Requires Linux and `CAP_SYS_ADMIN`; the veth case also needs `ip`/`nsenter`
//! and `CAP_NET_ADMIN`. Every case is ignored by default, and capability probes
//! fail hard so a misconfigured runner cannot report green while asserting
//! nothing.

#![cfg(target_os = "linux")]

// A Cargo integration-test crate root owns `tests/`, not `tests/<crate>/`, so the
// group modules need explicit paths.
#[path = "sandbox_network_tests/support.rs"]
mod support;

#[path = "sandbox_network_tests/namespace_e2e.rs"]
mod namespace_e2e;
#[path = "sandbox_network_tests/setup_teardown.rs"]
mod setup_teardown;
#[path = "sandbox_network_tests/signals.rs"]
mod signals;
#[path = "sandbox_network_tests/teardown_chain.rs"]
mod teardown_chain;
