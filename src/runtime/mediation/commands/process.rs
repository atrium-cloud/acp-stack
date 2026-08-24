//! Process-control helpers for the command supervisor: SIGTERM to the child's
//! process group, and SIGKILL by captured pid after the child has been reaped.

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

/// SIGKILL the process group for a pid captured before `child.wait()`, which
/// is what makes the post-wait grandchild reap possible at all
/// (`kill_tokio_process_group` needs a live `&mut Child`).
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

// Reserved for callers that want to bridge into the gateway via an oneshot.
#[allow(dead_code)]
pub(super) struct PendingHandle {
    pub(super) tx: oneshot::Sender<Result<CommandRecord>>,
}
