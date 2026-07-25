#![allow(dead_code)]

//! Shared helpers for the integration-test binaries.
//!
//! Each `tests/*.rs` file compiles to its own test binary, and `tests/common`
//! is a subdirectory module rather than a standalone binary, so the statics
//! defined here are instantiated once per binary that includes the module.
//! That per-binary instantiation is what preserves the serialization semantics
//! of the guards below: a `HOME_LOCK` shared across binaries would be a process
//! boundary too coarse to matter, but per-binary it serializes exactly the
//! parallel `#[tokio::test]` functions that would otherwise race.

pub mod agent;
pub mod api;
pub mod cli;
pub mod commands;
pub mod config;
pub mod sessions;
pub mod state;

use std::path::Path;

/// Serializes HOME mutations across the parallel-by-default `#[tokio::test]`
/// functions in a test binary. Handlers that resolve paths through `home_dir()`
/// require the test to pin HOME for its full body; without this lock two such
/// tests would step on each other's HOME and observe random subsets of the
/// other's tempdir state.
///
/// WARNING: this is sound only while every HOME read in the including binary
/// goes through a test holding this guard. A test (or helper) that reads HOME
/// without taking `HomeEnvGuard::set` races the unsafe `set_var` below and is
/// undefined behavior on multi-threaded runs — route it through the guard.
static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub struct HomeEnvGuard<'a> {
    _lock: std::sync::MutexGuard<'a, ()>,
    previous: Option<std::ffi::OsString>,
}

impl HomeEnvGuard<'_> {
    pub fn set(home: &Path) -> Self {
        let lock = HOME_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = std::env::var_os("HOME");
        // SAFETY: HOME_LOCK serializes tests that mutate HOME via this guard.
        // Tests in this binary that depend on HOME route through here, so
        // there's no read racing the mutation.
        unsafe {
            std::env::set_var("HOME", home);
        }
        Self {
            _lock: lock,
            previous,
        }
    }
}

impl Drop for HomeEnvGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: lock still held; restore the prior HOME (or remove if unset
        // coming in) before releasing it so the next test sees a clean slate.
        unsafe {
            match self.previous.take() {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
        }
    }
}
