//! Process-control helpers for the command supervisor: SIGTERM to the child's
//! process group, SIGKILL by captured pid after the child has been reaped, and
//! the signal name a reaped child died on.

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
pub(crate) fn send_terminate(child: &mut tokio::process::Child) {
    if let Err(error) = child.start_kill() {
        tracing::warn!(
            error = %crate::error::report(&error),
            pid = child.id(),
            "command terminate failed; the timeout kill follows",
        );
    }
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

/// Name of the signal that ended a reaped process, or `None` when it exited with a code.
#[cfg(unix)]
pub(crate) fn exit_signal(status: &std::process::ExitStatus) -> Option<String> {
    use std::os::unix::process::ExitStatusExt;
    status.signal().map(signal_name)
}

#[cfg(not(unix))]
pub(crate) fn exit_signal(_status: &std::process::ExitStatus) -> Option<String> {
    None
}

#[cfg(unix)]
fn signal_name(signal: i32) -> String {
    match signal {
        libc::SIGHUP => "SIGHUP".to_owned(),
        libc::SIGINT => "SIGINT".to_owned(),
        libc::SIGQUIT => "SIGQUIT".to_owned(),
        libc::SIGABRT => "SIGABRT".to_owned(),
        libc::SIGKILL => "SIGKILL".to_owned(),
        libc::SIGSEGV => "SIGSEGV".to_owned(),
        libc::SIGPIPE => "SIGPIPE".to_owned(),
        libc::SIGALRM => "SIGALRM".to_owned(),
        libc::SIGTERM => "SIGTERM".to_owned(),
        other => format!("SIG{other}"),
    }
}

// Reserved for callers that want to bridge into the gateway via an oneshot.
#[allow(dead_code)]
pub(super) struct PendingHandle {
    pub(super) tx: oneshot::Sender<Result<CommandRecord>>,
}
