//! Process-wide env guard shared by lib tests that rewrite HOME or the discovery fixture paths.

use std::sync::{Mutex, MutexGuard};

/// Serializes tests that rewrite process-wide env (HOME, discovery fixture
/// paths) so they cannot observe each other's mutation; drop restores the
/// prior values. Paths are the only values these tests set, so the guard
/// takes them directly.
static TEST_ENV_LOCK: Mutex<()> = Mutex::new(());

/// The bare lock, for tests that save and restore env themselves.
pub(crate) fn lock_test_env() -> MutexGuard<'static, ()> {
    TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(feature = "test-fixtures")]
pub(crate) struct TestEnvGuard {
    _lock: MutexGuard<'static, ()>,
    previous: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

#[cfg(feature = "test-fixtures")]
impl TestEnvGuard {
    pub(crate) fn set(pairs: &[(&'static str, &std::path::Path)]) -> Self {
        let lock = lock_test_env();
        let mut previous = Vec::with_capacity(pairs.len());
        // SAFETY: TEST_ENV_LOCK serializes every lib test that mutates
        // these process-wide variables, and it is acquired
        // exactly once for the whole batch.
        unsafe {
            for (key, value) in pairs {
                previous.push((*key, std::env::var_os(key)));
                std::env::set_var(key, value);
            }
        }
        Self {
            _lock: lock,
            previous,
        }
    }
}

#[cfg(feature = "test-fixtures")]
impl Drop for TestEnvGuard {
    fn drop(&mut self) {
        // SAFETY: the lock is still held; restore before releasing it.
        unsafe {
            for (key, previous) in std::mem::take(&mut self.previous) {
                match previous {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}
