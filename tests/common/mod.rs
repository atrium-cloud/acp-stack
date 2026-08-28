#![allow(dead_code)]

//! Shared helpers for the integration-test binaries.
//!
//! The suite is designed to run on a developer machine with the fixture guards active:
//! `ACP_STACK_TEST_DISPOSABLE_HOST` must stay unset locally (CI/docker set it to `1`). The
//! command helpers in `cli.rs` strip it from spawned `acps` processes so an accidental export
//! cannot unguard them; in-process harnesses get their isolation from the injected
//! `runtime_paths.home` instead.

pub mod agent;
pub mod api;
pub mod cli;
pub mod commands;
pub mod config;
pub mod sessions;
pub mod state;

use std::path::Path;

/// Serializes HOME mutations across the parallel `#[tokio::test]` functions in a test binary.
/// Routes read the harness-injected `runtime_paths.home`, so this guard is only for tests that
/// drive code resolving HOME from the process env in-process; every such test in a binary MUST use
/// it, since an unguarded read races the unsafe `set_var` below on multi-threaded runs.
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
        // SAFETY: HOME_LOCK serializes every HOME read and write in this binary.
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
        // SAFETY: the lock is still held, so the restore cannot race another test.
        unsafe {
            match self.previous.take() {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
        }
    }
}
