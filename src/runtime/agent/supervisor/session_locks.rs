//! Per-session mutual exclusion for prompt submission and cancel.

use super::*;

use std::sync::{Mutex, PoisonError};

use tokio::sync::OwnedMutexGuard;

/// One async mutex per session id, created on first use and removed once no
/// task holds or waits on it, so the map stays as small as the set of sessions
/// with a submit or cancel in progress.
#[derive(Default)]
pub(super) struct SessionLocks {
    entries: Mutex<HashMap<String, SessionLockEntry>>,
}

struct SessionLockEntry {
    lock: Arc<TokioMutex<()>>,
    /// Holders plus waiters. Counted explicitly rather than read off the Arc so
    /// a waiter whose future is dropped mid-wait still releases the entry.
    leases: usize,
}

/// Held for the duration of one submit or cancel on a session.
pub(super) struct SessionLockGuard<'a> {
    // Fields drop in declaration order: the mutex is released before the lease
    // can remove the entry, so a newcomer never gets a second mutex for a
    // session that is still held.
    _guard: OwnedMutexGuard<()>,
    _lease: SessionLockLease<'a>,
}

struct SessionLockLease<'a> {
    registry: &'a SessionLocks,
    session_id: String,
}

impl SessionLocks {
    pub(super) async fn lock(&self, session_id: &str) -> SessionLockGuard<'_> {
        let (lease, lock) = self.lease(session_id);
        let guard = lock.lock_owned().await;
        SessionLockGuard {
            _guard: guard,
            _lease: lease,
        }
    }

    fn lease(&self, session_id: &str) -> (SessionLockLease<'_>, Arc<TokioMutex<()>>) {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = entries
            .entry(session_id.to_owned())
            .or_insert_with(|| SessionLockEntry {
                lock: Arc::new(TokioMutex::new(())),
                leases: 0,
            });
        entry.leases += 1;
        let lease = SessionLockLease {
            registry: self,
            session_id: session_id.to_owned(),
        };
        (lease, Arc::clone(&entry.lock))
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

impl Drop for SessionLockLease<'_> {
    fn drop(&mut self) {
        let mut entries = self
            .registry
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = entries.get_mut(&self.session_id) {
            entry.leases = entry.leases.saturating_sub(1);
            if entry.leases == 0 {
                entries.remove(&self.session_id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use futures::FutureExt;

    /// Bound on awaiting a lock that must be free, so a regression fails instead of hanging.
    const FREE_LOCK_BUDGET: Duration = Duration::from_secs(5);

    #[tokio::test]
    async fn the_same_session_is_held_by_one_task_at_a_time() {
        let locks = SessionLocks::default();
        let held = locks.lock("sess_a").await;
        let mut waiter = Box::pin(locks.lock("sess_a"));
        assert!((&mut waiter).now_or_never().is_none());

        drop(held);
        let second = tokio::time::timeout(FREE_LOCK_BUDGET, waiter)
            .await
            .expect("the waiter takes the lock once it is released");
        assert_eq!(locks.len(), 1);
        drop(second);
        assert_eq!(locks.len(), 0);
    }

    #[tokio::test]
    async fn different_sessions_do_not_exclude_each_other() {
        let locks = SessionLocks::default();
        let _held = locks.lock("sess_a").await;
        let other = locks.lock("sess_b").now_or_never();
        assert!(other.is_some(), "another session's lock must be free");
        assert_eq!(locks.len(), 2);
    }

    #[tokio::test]
    async fn released_sessions_leave_no_entry_behind() {
        let locks = SessionLocks::default();
        for session_id in ["sess_a", "sess_b", "sess_c"] {
            drop(locks.lock(session_id).await);
        }
        assert_eq!(locks.len(), 0);
    }

    #[tokio::test]
    async fn an_abandoned_waiter_does_not_keep_the_entry_alive() {
        let locks = SessionLocks::default();
        let held = locks.lock("sess_a").await;
        let mut waiter = Box::pin(locks.lock("sess_a"));
        assert!((&mut waiter).now_or_never().is_none());

        drop(waiter);
        assert_eq!(locks.len(), 1);
        drop(held);
        assert_eq!(locks.len(), 0);
    }
}
