//! Process-control helpers for the command supervisor: SIGTERM to the
//! child's process group, SIGKILL by captured pid after the direct child has
//! been reaped, and the (currently unused) `PendingHandle` reserved for an
//! oneshot bridge into the gateway.
//!
//! Note: a tokio-`Child`-flavored SIGKILL helper already lives in
//! `crate::runtime::process_runner::kill_tokio_process_group`. The
//! supervisor uses that one directly for the in-flight cancel/timeout path.
//! The pid-based variant below is kept here because it runs *after*
//! `child.wait()` has reaped the direct child — at which point `child.id()`
//! may return `None`, so we cannot route the call through a `&mut Child`.

use tokio::sync::oneshot;

use crate::error::Result;
use crate::state::CommandRecord;

#[cfg(unix)]
pub(crate) fn send_terminate(child: &tokio::process::Child) {
    if let Some(pid) = child.id() {
        // SAFETY: we own the child pid; negative pid targets the whole process
        // group, which we set with `process_group(0)` at spawn time.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGTERM);
        }
    }
}

#[cfg(not(unix))]
pub(crate) fn send_terminate(child: &tokio::process::Child) {
    let _ = child.start_kill();
}

/// SIGKILL the process group for a captured pid. Safe to call after the
/// direct child has been reaped — the kernel may have recycled the pid, in
/// which case the kill is a harmless no-op or hits an unrelated foreground
/// group, which on a single-user runtime user owned by us is acceptable.
///
/// Distinct from `process_runner::kill_tokio_process_group` because that
/// helper needs a live `&mut Child` to read the pid; this one takes a pid
/// captured before `child.wait()` so the post-wait grandchild reap still
/// works after the kernel removed the child from the process table.
#[cfg(unix)]
pub(crate) fn kill_process_group_pid(pid: i32) {
    // SAFETY: negative pid targets the process group we created via
    // `process_group(0)` at spawn time. Caller must only pass pids it owns.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
pub(crate) fn kill_process_group_pid(_pid: i32) {}

// Unused at the moment but reserved for callers that want to bridge into the
// gateway via an oneshot. Keeps the API extensible without changing public
// surface later.
#[allow(dead_code)]
pub(super) struct PendingHandle {
    pub(super) tx: oneshot::Sender<Result<CommandRecord>>,
}
