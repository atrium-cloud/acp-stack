#![allow(dead_code)]

//! Shared helpers for the integration-test binaries.

pub mod agent;
pub mod api;
pub mod cli;
pub mod commands;
pub mod config;
pub mod sessions;
pub mod state;

use std::path::Path;

/// Serializes HOME mutations across the parallel `#[tokio::test]` functions in a test binary.
/// Every HOME read in the binary MUST go through `HomeEnvGuard::set`; one that does not races the
/// unsafe `set_var` below and is undefined behavior on multi-threaded runs.
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
