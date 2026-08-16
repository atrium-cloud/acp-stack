//! pidfd support probing and workload process-identity handling.

use super::*;

pub(in crate::runtime::sandbox::supervise) fn preflight_pidfd_support()
-> std::result::Result<(), String> {
    let pidfd = pidfd_open(std::process::id() as i32)
        .map_err(|error| pidfd_preflight_error("pidfd_open", &error))?;
    pidfd_send_signal(&pidfd, 0).map_err(|error| pidfd_preflight_error("pidfd_send_signal", &error))
}

pub(super) fn pidfd_preflight_error(action: &str, error: &std::io::Error) -> String {
    format!(
        "network-isolated sandboxing requires {action}; the kernel or seccomp policy rejected it: {error}"
    )
}

pub(super) fn pidfd_open(pid: i32) -> std::result::Result<OwnedFd, std::io::Error> {
    // SAFETY: pidfd_open takes a process id and zero flags and returns a new
    // owned descriptor on success.
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if raw < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the successful syscall returned a fresh descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(raw as i32) })
}

pub(super) fn pidfd_send_signal(
    pidfd: &OwnedFd,
    signo: i32,
) -> std::result::Result<(), std::io::Error> {
    // SAFETY: pidfd is owned and valid; a null siginfo pointer and zero
    // flags are the documented pidfd_send_signal form.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            signo,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn unshare_children(unshare_pid: i32) -> std::result::Result<Vec<i32>, std::io::Error> {
    let raw = std::fs::read_to_string(format!("/proc/{unshare_pid}/task/{unshare_pid}/children"))?;
    raw.split_whitespace()
        .map(|value| {
            value.parse::<i32>().map_err(|error| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid child pid `{value}`: {error}"),
                )
            })
        })
        .collect()
}

pub(super) fn validate_workload_child_identity(expected_pid: i32, children: &[i32]) -> Result<()> {
    if children == [expected_pid] {
        return Ok(());
    }
    Err(StackError::SandboxFailed {
        reason: format!(
            "the sandbox workload changed while opening its pidfd (expected child {expected_pid}, found {children:?})"
        ),
    })
}

pub(super) fn open_workload_pidfd(unshare_pid: i32) -> Result<WorkloadPidFd> {
    let children = unshare_children(unshare_pid).map_err(|source| StackError::SandboxFailed {
        reason: format!("reading the sandbox workload pid failed: {source}"),
    })?;
    let [workload_pid] = children.as_slice() else {
        return Err(StackError::SandboxFailed {
            reason: format!(
                "the sandbox chain must have exactly one workload child at readiness, found {children:?}"
            ),
        });
    };
    let pidfd = pidfd_open(*workload_pid).map_err(|source| StackError::SandboxFailed {
        reason: format!("opening pidfd for sandbox workload {workload_pid} failed: {source}"),
    })?;
    let revalidated =
        unshare_children(unshare_pid).map_err(|source| StackError::SandboxFailed {
            reason: format!("revalidating the sandbox workload pid failed: {source}"),
        })?;
    validate_workload_child_identity(*workload_pid, &revalidated)?;
    Ok(WorkloadPidFd(pidfd))
}
