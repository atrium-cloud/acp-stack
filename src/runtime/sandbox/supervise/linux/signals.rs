//! Signal forwarding: the self-pipe, the poll loops that multiplex child exit
//! against pending signals, and the shutdown/exit-mirroring paths.

use super::*;

/// Write end of the self-pipe, set once before the handlers are installed.
static SIGNAL_PIPE_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn forward_signal(signo: libc::c_int) {
    let fd = SIGNAL_PIPE_WRITE_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        let byte = signo as u8;
        // SAFETY: write(2) is async-signal-safe; the pipe is non-blocking so
        // a full buffer drops the byte instead of deadlocking the handler.
        unsafe { libc::write(fd, std::ptr::from_ref(&byte).cast(), 1) };
    }
}

/// Route SIGINT/SIGTERM through a non-blocking self-pipe so the main loops
/// can reap the workload and run teardown before mirroring the signal.
pub(super) fn install_signal_pipe() -> Result<i32> {
    let mut fds = [0i32; 2];
    // SAFETY: fds is a valid out-array for pipe2.
    let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) };
    if rc != 0 {
        let errno = std::io::Error::last_os_error();
        return Err(StackError::SandboxFailed {
            reason: format!("creating the signal pipe failed: {errno}"),
        });
    }
    SIGNAL_PIPE_WRITE_FD.store(fds[1], Ordering::SeqCst);
    for signo in [libc::SIGINT, libc::SIGTERM] {
        // SAFETY: a zeroed sigaction with a valid handler pointer and an
        // emptied mask is a well-formed argument.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = forward_signal as *const () as usize;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            if libc::sigaction(signo, &action, std::ptr::null_mut()) != 0 {
                let errno = std::io::Error::last_os_error();
                return Err(StackError::SandboxFailed {
                    reason: format!("installing the handler for signal {signo} failed: {errno}"),
                });
            }
        }
    }
    Ok(fds[0])
}

/// Non-blocking drain of the signal pipe; returns the last pending signal.
pub(super) fn drain_signal_pipe(signal_fd: i32) -> Option<i32> {
    let mut last: Option<i32> = None;
    let mut byte = 0u8;
    loop {
        // SAFETY: single-byte read from the owned non-blocking pipe.
        let rc = unsafe { libc::read(signal_fd, std::ptr::from_mut(&mut byte).cast(), 1) };
        if rc == 1 {
            last = Some(i32::from(byte));
            continue;
        }
        return last;
    }
}

/// Poll `fd` for readability alongside the signal pipe for one tick.
pub(super) fn poll_two(fd: i32, signal_fd: i32, timeout_ms: i32) -> (bool, bool) {
    let mut pollfds = [
        libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: signal_fd,
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    // SAFETY: pollfds is a valid array for the duration of the call.
    let rc = unsafe { libc::poll(pollfds.as_mut_ptr(), 2, timeout_ms) };
    if rc <= 0 {
        // Timeout, or EINTR — the interrupting signal lands in the pipe and
        // is picked up on the next iteration either way.
        return (false, false);
    }
    let readable =
        |revents: libc::c_short| revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0;
    (readable(pollfds[0].revents), readable(pollfds[1].revents))
}

pub(super) enum ReadyOutcome {
    Ready,
    ChildExited(ExitStatus),
    Signaled(i32),
}

pub(super) fn wait_for_ready(
    parent_sync: i32,
    signal_fd: i32,
    child: &mut Child,
    diag: &Diag,
) -> ReadyOutcome {
    loop {
        let (sync_readable, signal_readable) = poll_two(parent_sync, signal_fd, POLL_TICK_MS);
        if signal_readable && let Some(signo) = drain_signal_pipe(signal_fd) {
            return ReadyOutcome::Signaled(signo);
        }
        if sync_readable {
            match read_byte(parent_sync) {
                Ok(Some(_)) => return ReadyOutcome::Ready,
                Ok(None) | Err(_) => {
                    // EOF: the chain died before the sync point (e.g. mask
                    // failure). Reap it and mirror its status.
                    match child.wait() {
                        Ok(status) => return ReadyOutcome::ChildExited(status),
                        Err(source) => {
                            diag.line(&format!("waiting for the sandbox chain failed: {source}"));
                            std::process::exit(SETUP_FAILED_EXIT);
                        }
                    }
                }
            }
        }
    }
}

/// Wait for the workload, forwarding SIGINT/SIGTERM to the chain. The
/// first forwarded signal arms a grace deadline that escalates to SIGKILL
/// on the unshare process (cascading to the workload via `--kill-child`),
/// so the supervisor can never hang on shutdown even if the workload
/// ignores the signal.
pub(super) fn wait_for_child(
    child: &mut Child,
    unshare_pid: i32,
    workload_pidfd: &WorkloadPidFd,
    signal_fd: i32,
    diag: &Diag,
) -> ExitStatus {
    let mut kill_deadline: Option<Instant> = None;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status,
            Ok(None) => {}
            Err(source) => {
                diag.line(&format!("waiting for the sandbox chain failed: {source}"));
                std::process::exit(SETUP_FAILED_EXIT);
            }
        }
        if let Some(deadline) = kill_deadline
            && Instant::now() >= deadline
        {
            // SAFETY: unshare_pid is our direct child; worst case the pid
            // is already reaped and kill returns ESRCH, harmless here.
            unsafe { libc::kill(unshare_pid, libc::SIGKILL) };
            kill_deadline = None;
        }
        let (_, signal_readable) = poll_two(signal_fd, signal_fd, POLL_TICK_MS);
        if signal_readable && let Some(signo) = drain_signal_pipe(signal_fd) {
            forward_shutdown_signal(unshare_pid, Some(workload_pidfd), signo, diag);
            if kill_deadline.is_none() {
                kill_deadline = Some(Instant::now() + SIGNAL_KILL_GRACE);
            }
        }
    }
}

/// Deliver a shutdown signal to the workload chain. `unshare --fork`
/// ignores SIGINT/SIGTERM while waiting for its child (verified against
/// util-linux 2.41), so the signal must go to unshare's direct child (the
/// workload, post-exec) as well; unshare itself then propagates a
/// signal-death upward by re-raising it.
fn forward_shutdown_signal(
    unshare_pid: i32,
    workload_pidfd: Option<&WorkloadPidFd>,
    signo: i32,
    diag: &Diag,
) {
    if let Some(workload_pidfd) = workload_pidfd
        && let Err(error) = pidfd_send_signal(&workload_pidfd.0, signo)
        && error.raw_os_error() != Some(libc::ESRCH)
    {
        diag.line(&format!(
            "forwarding signal {signo} to the sandbox workload pidfd failed: {error}"
        ));
    }
    // unshare is our direct, unreaped child, so its pid cannot be reused
    // before this signal is sent.
    let rc = unsafe { libc::kill(unshare_pid, signo) };
    if rc != 0 {
        let errno = std::io::Error::last_os_error();
        diag.line(&format!("forwarding signal {signo} failed: {errno}"));
    }
}

/// Kill the unshare process (cascading to the workload via `--kill-child`)
/// and reap it.
pub(super) fn terminate_child(
    unshare_pid: i32,
    workload_pidfd: Option<&WorkloadPidFd>,
    signo: i32,
    child: &mut Child,
    diag: &Diag,
) {
    if signo == libc::SIGKILL {
        // SAFETY: unshare_pid is our direct, not-yet-reaped child.
        let rc = unsafe { libc::kill(unshare_pid, signo) };
        if rc != 0 {
            let errno = std::io::Error::last_os_error();
            diag.line(&format!("killing the sandbox chain failed: {errno}"));
        }
    } else {
        forward_shutdown_signal(unshare_pid, workload_pidfd, signo, diag);
        // Give the chain a moment to exit on the forwarded signal, then
        // escalate so the supervisor never hangs on shutdown.
        let deadline = Instant::now() + SIGNAL_KILL_GRACE;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Ok(None) | Err(_) => break,
            }
        }
        // SAFETY: same pid as above.
        unsafe { libc::kill(unshare_pid, libc::SIGKILL) };
    }
    if let Err(source) = child.wait() {
        diag.line(&format!("reaping the sandbox chain failed: {source}"));
    }
}

/// Terminate the supervisor mirroring the workload's status, surfacing a
/// teardown failure per the contract: workload success + failed teardown is
/// an error exit; a workload failure is preserved with the teardown error
/// only reported.
pub(super) fn mirror_status(status: ExitStatus, teardown_error: Option<String>, diag: &Diag) -> ! {
    if let Some(message) = &teardown_error {
        diag.line(message);
    }
    if let Some(signo) = status.signal() {
        exit_for_signal(signo);
    }
    let code = status.code().unwrap_or(SETUP_FAILED_EXIT);
    if code == 0 && teardown_error.is_some() {
        std::process::exit(TEARDOWN_FAILED_EXIT);
    }
    std::process::exit(code);
}

/// Die by the given signal so the daemon observes the workload's true
/// signal status, falling back to the 128+n convention.
pub(super) fn exit_for_signal(signo: i32) -> ! {
    // SAFETY: restoring the default disposition and re-raising is the
    // standard way for a wrapper to preserve signal-death semantics.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = libc::SIG_DFL;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(signo, &action, std::ptr::null_mut());
        libc::raise(signo);
    }
    std::process::exit(128 + signo);
}
