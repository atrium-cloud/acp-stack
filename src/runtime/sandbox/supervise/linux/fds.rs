//! Descriptor plumbing: the sync and liveness socketpairs, close-on-exec
//! handling, and raw writes to the diagnostic and liveness channels.

use super::*;

pub(super) fn write_all(fd: i32, mut bytes: &[u8]) -> std::result::Result<(), std::io::Error> {
    while !bytes.is_empty() {
        // SAFETY: the buffer is valid for the duration of the call.
        let rc = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        if rc < 0 {
            let errno = std::io::Error::last_os_error();
            if errno.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(errno);
        }
        bytes = &bytes[rc as usize..];
    }
    Ok(())
}

/// Socketpair for the release handshake. Both ends are close-on-exec; the
/// child end has the flag cleared so it survives the `unshare → acps
/// __sandbox-exec` exec chain (nothing else in that chain closes fds).
pub(super) fn sync_socketpair() -> Result<(i32, i32)> {
    let mut fds = [0i32; 2];
    // SAFETY: fds is a valid out-array for socketpair.
    let rc = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    };
    if rc != 0 {
        let errno = std::io::Error::last_os_error();
        return Err(StackError::SandboxFailed {
            reason: format!("creating the sandbox sync socketpair failed: {errno}"),
        });
    }
    // SAFETY: fds[1] is an owned, freshly created fd.
    let rc = unsafe { libc::fcntl(fds[1], libc::F_SETFD, 0) };
    if rc != 0 {
        let errno = std::io::Error::last_os_error();
        return Err(StackError::SandboxFailed {
            reason: format!("clearing close-on-exec on the sync fd failed: {errno}"),
        });
    }
    Ok((fds[0], fds[1]))
}

pub(super) fn provider_liveness_socketpair() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0i32; 2];
    // SAFETY: fds is a valid out-array for socketpair.
    let rc = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    };
    if rc != 0 {
        return Err(StackError::SandboxFailed {
            reason: format!(
                "creating the provider liveness socketpair failed: {}",
                std::io::Error::last_os_error()
            ),
        });
    }
    // SAFETY: both descriptors were freshly created and are transferred to
    // exactly one OwnedFd each.
    let supervisor_end = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let monitor_end = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    clear_cloexec(monitor_end.as_raw_fd())?;
    Ok((supervisor_end, monitor_end))
}

fn clear_cloexec(fd: i32) -> Result<()> {
    // SAFETY: fcntl on an owned descriptor with valid F_SETFD arguments.
    let rc = unsafe { libc::fcntl(fd, libc::F_SETFD, 0) };
    if rc != 0 {
        return Err(StackError::SandboxFailed {
            reason: format!(
                "clearing close-on-exec on fd {fd} failed: {}",
                std::io::Error::last_os_error()
            ),
        });
    }
    Ok(())
}

pub(super) fn close_fd(fd: i32, diag: &Diag) {
    // SAFETY: fd is owned by the supervisor and closed exactly once.
    let rc = unsafe { libc::close(fd) };
    if rc != 0 {
        let errno = std::io::Error::last_os_error();
        diag.line(&format!("closing fd {fd} failed: {errno}"));
    }
}
