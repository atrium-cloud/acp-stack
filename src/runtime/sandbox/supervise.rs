//! Supervisor for network-isolated sandbox spawns (`acps __sandbox-supervise`).
//!
//! With a `network-provider` extension declared (`[extensions.<name>]`), each
//! wrapped spawn gets a fresh network namespace. The supervisor sits between
//! the daemon and the `unshare --net` chain and owns the namespace lifecycle:
//!
//! 1. Spawn the chain with a private sync socketpair; [`super::run_exec`] inside
//!    the namespaces signals readiness and then pauses before privilege drop.
//! 2. Hold `/proc/<unshare-pid>/ns/net` open — the fd keeps the namespace alive
//!    without bind mounts and gives the provider a `setns`/`nsenter`-able path
//!    that stays valid through teardown, even after the workload dies.
//! 3. Run the operator provider's `setup` under a liveness-monitored process
//!    group; release the workload only on exit 0. Failure or timeout is
//!    fail-closed: the workload never execs.
//! 4. Wait for the workload (stdio passes through untouched — for the agent
//!    harness stdin/stdout are the ACP transport), run `teardown` while the
//!    namespace fd is still open, then mirror the workload's exit or signal.
//!
//! The provider runs with the supervisor's privileges and a cleared environment
//! (only the `ACPS_SANDBOX_NETWORK_*` contract variables): agent env and secrets
//! never reach it. A private liveness socket makes supervisor death kill its
//! complete in-contract process group. Its stdout is discarded so it cannot
//! corrupt the ACP transport; its stderr goes to the daemon-stderr diagnostic
//! fd or to null, per `provider_stderr`.

use crate::error::{Result, StackError};

// CONSTANTS

/// Provider protocol version, exposed as `ACPS_SANDBOX_NETWORK_PROTOCOL`.
pub const NETWORK_PROTOCOL_VERSION: &str = "1";
pub const ENV_NETWORK_PROTOCOL: &str = "ACPS_SANDBOX_NETWORK_PROTOCOL";
pub const ENV_NETWORK_ID: &str = "ACPS_SANDBOX_NETWORK_ID";
pub const ENV_NETWORK_NAMESPACE: &str = "ACPS_SANDBOX_NETWORK_NAMESPACE";
pub const ENV_NETWORK_PID: &str = "ACPS_SANDBOX_NETWORK_PID";

/// Supervisor exit code when provider `setup` failed or timed out (the workload
/// was never executed).
pub const SETUP_FAILED_EXIT: i32 = 120;
/// Supervisor exit code when the workload succeeded but provider `teardown`
/// failed; a workload failure is preserved instead, with the teardown error
/// reported on the diagnostic fd.
pub const TEARDOWN_FAILED_EXIT: i32 = 121;

/// Sync handshake bytes: the in-namespace helper sends `READY` once masking is
/// done (proving the namespaces exist), the supervisor sends `RELEASE` once
/// provider setup succeeded.
const READY_BYTE: u8 = b'R';
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const RELEASE_BYTE: u8 = b'G';

/// Called by [`super::run_exec`] when the wrapper carries `--sync-fd`: signal
/// readiness to the supervisor, then block until it releases the workload.
/// EOF means the supervisor died or provider setup failed — fail closed and
/// never exec. On release the fd is marked close-on-exec so the workload does
/// not inherit the sync channel.
pub fn wait_for_release(fd: i32) -> Result<()> {
    write_byte(fd, READY_BYTE)?;
    match read_byte(fd)? {
        Some(_) => {
            set_cloexec(fd)?;
            Ok(())
        }
        None => Err(StackError::SandboxFailed {
            reason: "the network sandbox supervisor closed the sync channel before releasing \
                     the workload (provider setup failed or the supervisor died)"
                .to_owned(),
        }),
    }
}

fn write_byte(fd: i32, byte: u8) -> Result<()> {
    loop {
        // SAFETY: the buffer is a valid single byte for the duration of the call.
        let rc = unsafe { libc::write(fd, std::ptr::from_ref(&byte).cast(), 1) };
        if rc == 1 {
            return Ok(());
        }
        let errno = std::io::Error::last_os_error();
        if errno.kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        return Err(StackError::SandboxFailed {
            reason: format!("write to sandbox sync fd {fd} failed: {errno}"),
        });
    }
}

/// Blocking single-byte read; `Ok(None)` is EOF (peer closed without writing).
fn read_byte(fd: i32) -> Result<Option<u8>> {
    let mut byte = 0u8;
    loop {
        // SAFETY: the buffer is a valid single byte for the duration of the call.
        let rc = unsafe { libc::read(fd, std::ptr::from_mut(&mut byte).cast(), 1) };
        match rc {
            1 => return Ok(Some(byte)),
            0 => return Ok(None),
            _ => {
                let errno = std::io::Error::last_os_error();
                if errno.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(StackError::SandboxFailed {
                    reason: format!("read from sandbox sync fd {fd} failed: {errno}"),
                });
            }
        }
    }
}

fn set_cloexec(fd: i32) -> Result<()> {
    // SAFETY: fcntl on an owned fd with valid F_SETFD arguments.
    let rc = unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    if rc != 0 {
        let errno = std::io::Error::last_os_error();
        return Err(StackError::SandboxFailed {
            reason: format!("marking fd {fd} close-on-exec failed: {errno}"),
        });
    }
    Ok(())
}

/// Entry point for `acps __sandbox-supervise`. Terminates the process itself
/// (mirroring the workload's exit or signal status) on every path after the
/// child chain is spawned; an `Err` return means the supervisor could not even
/// start (bad argv, socketpair/spawn failure).
#[cfg(target_os = "linux")]
pub fn run_supervise(raw_args: Vec<String>) -> Result<()> {
    linux::run(raw_args)
}

#[cfg(not(target_os = "linux"))]
pub fn run_supervise(_raw_args: Vec<String>) -> Result<()> {
    Err(StackError::SandboxFailed {
        reason: "network isolation is only supported on Linux".to_owned(),
    })
}

/// Entry point for the provider process-group monitor. The main sandbox
/// supervisor owns the peer of its liveness fd; EOF means that supervisor died
/// and the entire provider process group must be killed immediately.
#[cfg(target_os = "linux")]
pub fn run_provider_supervise(raw_args: Vec<String>) -> Result<()> {
    linux::run_provider_supervise(raw_args)
}

#[cfg(not(target_os = "linux"))]
pub fn run_provider_supervise(_raw_args: Vec<String>) -> Result<()> {
    Err(StackError::SandboxFailed {
        reason: "network isolation is only supported on Linux".to_owned(),
    })
}

#[cfg(target_os = "linux")]
pub(super) fn preflight_pidfd_support() -> std::result::Result<(), String> {
    linux::preflight_pidfd_support()
}

#[cfg(not(target_os = "linux"))]
pub(super) fn preflight_pidfd_support() -> std::result::Result<(), String> {
    Err("network-isolated sandboxing requires Linux pidfd support".to_owned())
}

#[cfg(target_os = "linux")]
mod linux {
    use std::fs::File;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::process::{Child, Command, ExitStatus, Stdio};
    use std::sync::atomic::{AtomicI32, Ordering};
    use std::time::{Duration, Instant};

    use rand::RngExt;

    use super::{RELEASE_BYTE, read_byte, set_cloexec, write_byte};
    use crate::config::SandboxProviderStderr;
    use crate::error::{Result, StackError};

    /// Poll tick for loops that multiplex child exit against the signal pipe.
    const POLL_TICK_MS: i32 = 100;

    /// Grace window between a forwarded shutdown signal and the SIGKILL
    /// escalation on the unshare process.
    const SIGNAL_KILL_GRACE: Duration = Duration::from_secs(2);

    /// Trusted working directory shared by the provider monitor and provider.
    const PROVIDER_WORKING_DIRECTORY: &str = "/";

    struct SuperviseOptions {
        diag_fd: i32,
        provider: Vec<String>,
        provider_timeout: Duration,
        provider_stderr: SandboxProviderStderr,
        child_command: Vec<String>,
    }

    struct ProviderSuperviseOptions {
        liveness_fd: i32,
        provider_stderr: SandboxProviderStderr,
        provider_command: Vec<String>,
    }

    struct WorkloadPidFd(OwnedFd);

    /// Diagnostic writer for the daemon-stderr fd. Falls back to the process
    /// stderr when the fd is not wired (e.g. direct invocation in tests) so
    /// failures are never silent.
    struct Diag {
        fd: i32,
    }

    impl Diag {
        fn line(&self, message: &str) {
            let formatted = format!("acps sandbox-supervise: {message}\n");
            if write_all(self.fd, formatted.as_bytes()).is_err() {
                eprint!("{formatted}");
            }
        }
    }

    fn write_all(fd: i32, mut bytes: &[u8]) -> std::result::Result<(), std::io::Error> {
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

    pub(super) fn preflight_pidfd_support() -> std::result::Result<(), String> {
        let pidfd = pidfd_open(std::process::id() as i32)
            .map_err(|error| pidfd_preflight_error("pidfd_open", &error))?;
        pidfd_send_signal(&pidfd, 0)
            .map_err(|error| pidfd_preflight_error("pidfd_send_signal", &error))
    }

    fn pidfd_preflight_error(action: &str, error: &std::io::Error) -> String {
        format!(
            "network-isolated sandboxing requires {action}; the kernel or seccomp policy rejected it: {error}"
        )
    }

    fn pidfd_open(pid: i32) -> std::result::Result<OwnedFd, std::io::Error> {
        // SAFETY: pidfd_open takes a process id and zero flags and returns a new
        // owned descriptor on success.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if raw < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: the successful syscall returned a fresh descriptor.
        Ok(unsafe { OwnedFd::from_raw_fd(raw as i32) })
    }

    fn pidfd_send_signal(pidfd: &OwnedFd, signo: i32) -> std::result::Result<(), std::io::Error> {
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
        let raw =
            std::fs::read_to_string(format!("/proc/{unshare_pid}/task/{unshare_pid}/children"))?;
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

    fn validate_workload_child_identity(expected_pid: i32, children: &[i32]) -> Result<()> {
        if children == [expected_pid] {
            return Ok(());
        }
        Err(StackError::SandboxFailed {
            reason: format!(
                "the sandbox workload changed while opening its pidfd (expected child {expected_pid}, found {children:?})"
            ),
        })
    }

    fn open_workload_pidfd(unshare_pid: i32) -> Result<WorkloadPidFd> {
        let children =
            unshare_children(unshare_pid).map_err(|source| StackError::SandboxFailed {
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

    pub(super) fn run(raw_args: Vec<String>) -> Result<()> {
        let options = parse_args(raw_args)?;
        let diag_fd = options.diag_fd;
        if let Err(error) = run_with_options(options) {
            Diag { fd: diag_fd }.line(&error.to_string());
            std::process::exit(super::SETUP_FAILED_EXIT);
        }
        Ok(())
    }

    fn run_with_options(options: SuperviseOptions) -> Result<()> {
        // The raw diag fd must not leak into the unshare chain or the provider;
        // provider stderr gets an explicit dup instead.
        set_cloexec(options.diag_fd)?;
        let diag = Diag {
            fd: options.diag_fd,
        };
        let signal_fd = install_signal_pipe()?;

        let (parent_sync, child_sync) = sync_socketpair()?;
        let mut child_command = options.child_command.clone();
        inject_sync_fd(&mut child_command, child_sync)?;
        let mut command = Command::new(&child_command[0]);
        command.args(&child_command[1..]);
        let supervisor_pid = std::process::id() as i32;
        // A SIGKILL of the supervisor must not orphan the chain: the daemon's
        // kill_on_drop and any direct-pid kill only reach the supervisor, so
        // tie unshare's lifetime to it. unshare's own --kill-child then reaps
        // the workload. pdeathsig survives exec because unshare never changes
        // credentials.
        // SAFETY: prctl/getppid are async-signal-safe; the closure only runs
        // in the forked child before exec.
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // The death signal only covers deaths after the prctl call;
                // re-check that the supervisor did not die during the fork gap.
                if libc::getppid() != supervisor_pid {
                    return Err(std::io::Error::other(
                        "the supervisor died before the sandbox chain started",
                    ));
                }
                Ok(())
            });
        }
        let mut child = command
            .spawn()
            .map_err(|source| StackError::SandboxFailed {
                reason: format!(
                    "spawning the sandbox chain `{}` failed: {}",
                    child_command[0], source
                ),
            })?;
        close_fd(child_sync, &diag);
        let unshare_pid = child.id() as i32;

        // Phase 1: wait until the in-namespace helper reports ready. Readiness
        // proves masking completed and the network namespace exists, so opening
        // /proc/<unshare-pid>/ns/net below cannot race the unshare(2) call and
        // capture the host namespace by mistake.
        match wait_for_ready(parent_sync, signal_fd, &mut child, &diag) {
            ReadyOutcome::Ready => {}
            ReadyOutcome::ChildExited(status) => {
                diag.line("the sandbox chain exited before reaching the sync point");
                mirror_status(status, None, &diag);
            }
            ReadyOutcome::Signaled(signo) => {
                terminate_child(unshare_pid, None, signo, &mut child, &diag);
                exit_for_signal(signo);
            }
        }

        // The helper is blocked on the release socket at this point, so its
        // identity is stable long enough to open and revalidate a pidfd. Hold
        // that handle through shutdown so a recycled numeric pid can never be
        // signaled.
        let workload_pidfd = open_workload_pidfd(unshare_pid)?;

        // Phase 2: retain the namespace handle for the whole spawn lifetime.
        let ns_file = File::open(format!("/proc/{unshare_pid}/ns/net")).map_err(|source| {
            StackError::SandboxFailed {
                reason: format!("opening /proc/{unshare_pid}/ns/net failed: {source}"),
            }
        })?;
        let ns_path = format!("/proc/{}/fd/{}", std::process::id(), ns_file.as_raw_fd());
        let network_id = generate_network_id();

        // Phase 3: provider setup gates workload execution. No provider means
        // deny-all networking: the namespace stays exactly as unshare made it.
        if !options.provider.is_empty()
            && let Err(failure) = run_provider(
                ProviderPhase::Setup,
                &options,
                &ns_path,
                &network_id,
                Some(unshare_pid),
                signal_fd,
            )
        {
            let reason = failure.describe("setup");
            diag.line(&reason);
            // Fail closed: close the sync channel unwritten so the helper sees
            // EOF and exits without ever executing the workload.
            close_fd(parent_sync, &diag);
            terminate_child(
                unshare_pid,
                Some(&workload_pidfd),
                libc::SIGKILL,
                &mut child,
                &diag,
            );
            if let Err(teardown_failure) = run_provider(
                ProviderPhase::Teardown,
                &options,
                &ns_path,
                &network_id,
                None,
                signal_fd,
            ) {
                diag.line(&teardown_failure.describe("teardown after failed setup"));
            }
            match failure {
                ProviderFailure::Interrupted(signo) => exit_for_signal(signo),
                ProviderFailure::Failed(_) => std::process::exit(super::SETUP_FAILED_EXIT),
            }
        }

        // Phase 4: release the workload and wait for it, forwarding SIGINT and
        // SIGTERM so the chain (and, via --kill-child, the workload) shuts down.
        // A failed release write (the helper died between ready and release)
        // must not skip teardown: setup already created host-side resources.
        // Closing the sync fd keeps the fail-closed guarantee — the helper can
        // only ever see the release byte or EOF.
        if let Err(error) = write_byte(parent_sync, RELEASE_BYTE) {
            diag.line(&format!("releasing the workload failed: {error}"));
        }
        close_fd(parent_sync, &diag);
        let status = wait_for_child(&mut child, unshare_pid, &workload_pidfd, signal_fd, &diag);

        // Phase 5: teardown runs while the namespace fd is still open, so the
        // provider can still enter the namespace even though the workload died.
        let teardown_error = if options.provider.is_empty() {
            None
        } else {
            run_provider(
                ProviderPhase::Teardown,
                &options,
                &ns_path,
                &network_id,
                None,
                signal_fd,
            )
            .err()
            .map(|failure| failure.describe("teardown"))
        };

        drop(ns_file);
        mirror_status(status, teardown_error, &diag);
    }

    pub(super) fn run_provider_supervise(raw_args: Vec<String>) -> Result<()> {
        let options = parse_provider_supervise_args(raw_args)?;
        if unsafe { libc::getpgrp() } != std::process::id() as i32 {
            return Err(StackError::SandboxFailed {
                reason: "sandbox-provider-supervise must be a process-group leader".to_owned(),
            });
        }
        // The provider must not inherit the liveness channel. Its monitor is
        // the only process that watches the supervisor peer for EOF.
        set_cloexec(options.liveness_fd)?;

        let mut command = Command::new(&options.provider_command[0]);
        command.args(&options.provider_command[1..]);
        command.current_dir(PROVIDER_WORKING_DIRECTORY);
        command.env_clear();
        for name in [
            super::ENV_NETWORK_PROTOCOL,
            super::ENV_NETWORK_ID,
            super::ENV_NETWORK_NAMESPACE,
        ] {
            let value = std::env::var_os(name).ok_or_else(|| StackError::SandboxFailed {
                reason: format!("sandbox-provider-supervise requires environment variable {name}"),
            })?;
            command.env(name, value);
        }
        if let Some(value) = std::env::var_os(super::ENV_NETWORK_PID) {
            command.env(super::ENV_NETWORK_PID, value);
        }
        command.stdin(Stdio::null());
        command.stdout(Stdio::null());
        command.stderr(match options.provider_stderr {
            SandboxProviderStderr::Daemon => Stdio::inherit(),
            SandboxProviderStderr::Null => Stdio::null(),
        });
        // The provider inherits the monitor's process group. It must not detach;
        // on liveness EOF the monitor SIGKILLs this entire group, including
        // itself and every provider descendant that honored the contract.
        let mut provider = command
            .spawn()
            .map_err(|source| StackError::SandboxFailed {
                reason: format!(
                    "spawning sandbox network provider `{}` failed: {source}",
                    options.provider_command[0]
                ),
            })?;

        loop {
            match provider.try_wait() {
                Ok(Some(status)) => report_provider_status(options.liveness_fd, status),
                Ok(None) => {}
                Err(source) => {
                    eprintln!(
                        "acps sandbox-provider-supervise: waiting for provider failed: {source}"
                    );
                    kill_own_process_group();
                }
            }
            match liveness_peer_closed(options.liveness_fd) {
                Ok(false) => {}
                Ok(true) => kill_own_process_group(),
                Err(error) => {
                    eprintln!("acps sandbox-provider-supervise: {error}");
                    kill_own_process_group();
                }
            }
        }
    }

    fn report_provider_status(liveness_fd: i32, status: ExitStatus) -> ! {
        let raw_status = status.into_raw().to_ne_bytes();
        if let Err(error) = write_all(liveness_fd, &raw_status) {
            eprintln!("acps sandbox-provider-supervise: reporting provider status failed: {error}");
            kill_own_process_group();
        }
        // Remain the process-group anchor until the supervisor receives the
        // provider status and kills the complete group. This closes both the
        // provider-exits-first orphan window and numeric PGID reuse window.
        loop {
            match liveness_peer_closed(liveness_fd) {
                Ok(false) => {}
                Ok(true) => kill_own_process_group(),
                Err(error) => {
                    eprintln!("acps sandbox-provider-supervise: {error}");
                    kill_own_process_group();
                }
            }
        }
    }

    fn liveness_peer_closed(fd: i32) -> Result<bool> {
        let mut pollfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: pollfd is valid for the duration of the call.
        let rc = unsafe { libc::poll(std::ptr::from_mut(&mut pollfd), 1, POLL_TICK_MS) };
        if rc == 0 {
            return Ok(false);
        }
        if rc < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                return Ok(false);
            }
            return Err(StackError::SandboxFailed {
                reason: format!("polling provider liveness fd {fd} failed: {error}"),
            });
        }
        Ok(pollfd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0)
    }

    fn kill_own_process_group() -> ! {
        // SAFETY: the provider monitor is its process-group leader; a negative
        // pgid targets the monitor, provider, and all in-contract descendants.
        unsafe {
            libc::kill(-libc::getpgrp(), libc::SIGKILL);
        }
        std::process::exit(super::SETUP_FAILED_EXIT);
    }

    fn parse_args(raw_args: Vec<String>) -> Result<SuperviseOptions> {
        let mut diag_fd: Option<i32> = None;
        let mut provider: Vec<String> = Vec::new();
        let mut provider_timeout: Option<Duration> = None;
        let mut provider_stderr: Option<SandboxProviderStderr> = None;
        let mut child_command: Vec<String> = Vec::new();
        let mut iter = raw_args.into_iter();
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--diag-fd" => {
                    let value = next_value(&mut iter, "--diag-fd")?;
                    diag_fd =
                        Some(
                            value
                                .parse::<i32>()
                                .map_err(|_| StackError::SandboxFailed {
                                    reason: format!(
                                        "--diag-fd expects an fd number, got `{value}`"
                                    ),
                                })?,
                        );
                }
                "--provider-timeout" => {
                    let value = next_value(&mut iter, "--provider-timeout")?;
                    let parsed = crate::config::parse_duration_string(&value).ok_or_else(|| {
                        StackError::SandboxFailed {
                            reason: format!("--provider-timeout `{value}` is not a valid duration"),
                        }
                    })?;
                    provider_timeout = Some(parsed);
                }
                "--provider-stderr" => {
                    let value = next_value(&mut iter, "--provider-stderr")?;
                    provider_stderr = Some(match value.as_str() {
                        "daemon" => SandboxProviderStderr::Daemon,
                        "null" => SandboxProviderStderr::Null,
                        other => {
                            return Err(StackError::SandboxFailed {
                                reason: format!(
                                    "--provider-stderr expects `daemon` or `null`, got `{other}`"
                                ),
                            });
                        }
                    });
                }
                "--provider-arg" => {
                    provider.push(next_value(&mut iter, "--provider-arg")?);
                }
                "--" => {
                    child_command = iter.collect();
                    break;
                }
                other => {
                    return Err(StackError::SandboxFailed {
                        reason: format!("unexpected sandbox-supervise argument `{other}`"),
                    });
                }
            }
        }
        if child_command.is_empty() {
            return Err(StackError::SandboxFailed {
                reason: "sandbox-supervise requires a command after `--`".to_owned(),
            });
        }
        Ok(SuperviseOptions {
            diag_fd: diag_fd.ok_or_else(|| missing_flag("--diag-fd"))?,
            provider,
            provider_timeout: provider_timeout.ok_or_else(|| missing_flag("--provider-timeout"))?,
            provider_stderr: provider_stderr.ok_or_else(|| missing_flag("--provider-stderr"))?,
            child_command,
        })
    }

    fn parse_provider_supervise_args(raw_args: Vec<String>) -> Result<ProviderSuperviseOptions> {
        let mut liveness_fd: Option<i32> = None;
        let mut provider_stderr: Option<SandboxProviderStderr> = None;
        let mut provider_command = Vec::new();
        let mut iter = raw_args.into_iter();
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--liveness-fd" => {
                    let value = next_value(&mut iter, "--liveness-fd")?;
                    liveness_fd =
                        Some(
                            value
                                .parse::<i32>()
                                .map_err(|_| StackError::SandboxFailed {
                                    reason: format!(
                                        "--liveness-fd expects an fd number, got `{value}`"
                                    ),
                                })?,
                        );
                }
                "--provider-stderr" => {
                    provider_stderr = Some(parse_provider_stderr(&next_value(
                        &mut iter,
                        "--provider-stderr",
                    )?)?);
                }
                "--" => {
                    provider_command = iter.collect();
                    break;
                }
                other => {
                    return Err(StackError::SandboxFailed {
                        reason: format!("unexpected sandbox-provider-supervise argument `{other}`"),
                    });
                }
            }
        }
        if provider_command.is_empty() {
            return Err(StackError::SandboxFailed {
                reason: "sandbox-provider-supervise requires a provider command after `--`"
                    .to_owned(),
            });
        }
        Ok(ProviderSuperviseOptions {
            liveness_fd: liveness_fd.ok_or_else(|| missing_flag("--liveness-fd"))?,
            provider_stderr: provider_stderr.ok_or_else(|| missing_flag("--provider-stderr"))?,
            provider_command,
        })
    }

    fn parse_provider_stderr(value: &str) -> Result<SandboxProviderStderr> {
        match value {
            "daemon" => Ok(SandboxProviderStderr::Daemon),
            "null" => Ok(SandboxProviderStderr::Null),
            other => Err(StackError::SandboxFailed {
                reason: format!("--provider-stderr expects `daemon` or `null`, got `{other}`"),
            }),
        }
    }

    fn next_value(iter: &mut std::vec::IntoIter<String>, flag: &str) -> Result<String> {
        iter.next().ok_or_else(|| StackError::SandboxFailed {
            reason: format!("{flag} requires a value"),
        })
    }

    fn missing_flag(flag: &str) -> StackError {
        StackError::SandboxFailed {
            reason: format!("sandbox-supervise requires {flag}"),
        }
    }

    /// The `--sync-fd` value only exists at supervisor runtime, so the wrapper
    /// argv carries the `__sandbox-exec` invocation without it and the fd is
    /// injected here, right after the subcommand token.
    fn inject_sync_fd(child_command: &mut Vec<String>, sync_fd: i32) -> Result<()> {
        let position = child_command
            .iter()
            .position(|arg| arg == super::super::SANDBOX_EXEC_SUBCOMMAND)
            .ok_or_else(|| StackError::SandboxFailed {
                reason: format!(
                    "sandbox-supervise child command does not contain `{}`",
                    super::super::SANDBOX_EXEC_SUBCOMMAND
                ),
            })?;
        child_command.insert(position + 1, "--sync-fd".to_owned());
        child_command.insert(position + 2, sync_fd.to_string());
        Ok(())
    }

    /// Socketpair for the release handshake. Both ends are close-on-exec; the
    /// child end has the flag cleared so it survives the `unshare → acps
    /// __sandbox-exec` exec chain (nothing else in that chain closes fds).
    fn sync_socketpair() -> Result<(i32, i32)> {
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

    fn provider_liveness_socketpair() -> Result<(OwnedFd, OwnedFd)> {
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

    fn close_fd(fd: i32, diag: &Diag) {
        // SAFETY: fd is owned by the supervisor and closed exactly once.
        let rc = unsafe { libc::close(fd) };
        if rc != 0 {
            let errno = std::io::Error::last_os_error();
            diag.line(&format!("closing fd {fd} failed: {errno}"));
        }
    }

    // SIGNAL FORWARDING

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
    fn install_signal_pipe() -> Result<i32> {
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
                        reason: format!(
                            "installing the handler for signal {signo} failed: {errno}"
                        ),
                    });
                }
            }
        }
        Ok(fds[0])
    }

    /// Non-blocking drain of the signal pipe; returns the last pending signal.
    fn drain_signal_pipe(signal_fd: i32) -> Option<i32> {
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
    fn poll_two(fd: i32, signal_fd: i32, timeout_ms: i32) -> (bool, bool) {
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

    enum ReadyOutcome {
        Ready,
        ChildExited(ExitStatus),
        Signaled(i32),
    }

    fn wait_for_ready(
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
                                diag.line(&format!(
                                    "waiting for the sandbox chain failed: {source}"
                                ));
                                std::process::exit(super::SETUP_FAILED_EXIT);
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
    fn wait_for_child(
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
                    std::process::exit(super::SETUP_FAILED_EXIT);
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
    fn terminate_child(
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
    fn mirror_status(status: ExitStatus, teardown_error: Option<String>, diag: &Diag) -> ! {
        if let Some(message) = &teardown_error {
            diag.line(message);
        }
        if let Some(signo) = status.signal() {
            exit_for_signal(signo);
        }
        let code = status.code().unwrap_or(super::SETUP_FAILED_EXIT);
        if code == 0 && teardown_error.is_some() {
            std::process::exit(super::TEARDOWN_FAILED_EXIT);
        }
        std::process::exit(code);
    }

    /// Die by the given signal so the daemon observes the workload's true
    /// signal status, falling back to the 128+n convention.
    fn exit_for_signal(signo: i32) -> ! {
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

    // PROVIDER LIFECYCLE

    enum ProviderPhase {
        Setup,
        Teardown,
    }

    impl ProviderPhase {
        fn as_str(&self) -> &'static str {
            match self {
                ProviderPhase::Setup => "setup",
                ProviderPhase::Teardown => "teardown",
            }
        }

        /// Setup aborts on SIGINT/SIGTERM (the workload was never released, so
        /// bailing out is safe). Teardown is cleanup that must not be cut short
        /// by a shutdown signal — a killed teardown guarantees a host-side
        /// resource leak — so it is bounded by the provider timeout only.
        fn interruptible(&self) -> bool {
            matches!(self, ProviderPhase::Setup)
        }
    }

    enum ProviderFailure {
        Failed(String),
        Interrupted(i32),
    }

    impl ProviderFailure {
        fn describe(&self, phase: &str) -> String {
            match self {
                ProviderFailure::Failed(reason) => format!("provider {phase} failed: {reason}"),
                ProviderFailure::Interrupted(signo) => {
                    format!("provider {phase} interrupted by signal {signo}")
                }
            }
        }
    }

    fn run_provider(
        phase: ProviderPhase,
        options: &SuperviseOptions,
        ns_path: &str,
        network_id: &str,
        workload_pid: Option<i32>,
        signal_fd: i32,
    ) -> std::result::Result<(), ProviderFailure> {
        let (supervisor_liveness, monitor_liveness) =
            provider_liveness_socketpair().map_err(|error| {
                ProviderFailure::Failed(format!("creating provider monitor failed: {error}"))
            })?;
        let self_exe = std::env::current_exe().map_err(|source| {
            ProviderFailure::Failed(format!(
                "resolving the acps executable for the provider monitor failed: {source}"
            ))
        })?;
        let mut command = Command::new(self_exe);
        command.args([
            super::super::SANDBOX_PROVIDER_SUPERVISE_SUBCOMMAND,
            "--liveness-fd",
        ]);
        command.arg(monitor_liveness.as_raw_fd().to_string());
        command.args(["--provider-stderr", options.provider_stderr.as_str(), "--"]);
        command.arg(&options.provider[0]);
        command.arg(phase.as_str());
        command.args(&options.provider[1..]);
        // The monitor and provider must not resolve anything relative to the
        // agent-writable workload cwd inherited by the sandbox supervisor.
        command.current_dir(PROVIDER_WORKING_DIRECTORY);
        // The monitor forwards exactly these contract variables to the provider.
        command.env_clear();
        command.env(super::ENV_NETWORK_PROTOCOL, super::NETWORK_PROTOCOL_VERSION);
        command.env(super::ENV_NETWORK_ID, network_id);
        command.env(super::ENV_NETWORK_NAMESPACE, ns_path);
        if let Some(pid) = workload_pid {
            command.env(super::ENV_NETWORK_PID, pid.to_string());
        }
        command.stdin(Stdio::null());
        command.stdout(Stdio::null());
        // Monitor diagnostics always reach the daemon. It independently applies
        // provider_stderr when it spawns the actual provider.
        command.stderr(provider_monitor_stderr_stdio(options)?);
        // The monitor leads the group inherited by the actual provider.
        command.process_group(0);
        let spawn_result = command.spawn();
        drop(monitor_liveness);
        let mut provider = spawn_result.map_err(|source| {
            ProviderFailure::Failed(format!(
                "spawning the monitor for `{}` failed: {source}",
                options.provider[0]
            ))
        })?;
        // Keeping this endpoint alive is the provider monitor's parent-liveness
        // guarantee. It also carries the actual provider's raw wait status while
        // the monitor remains alive as the process-group anchor.
        let provider_pid = provider.id() as i32;
        let deadline = Instant::now() + options.provider_timeout;
        loop {
            let (status_readable, signal_readable) = if phase.interruptible() {
                poll_two(supervisor_liveness.as_raw_fd(), signal_fd, POLL_TICK_MS)
            } else {
                let (status_readable, _) = poll_two(
                    supervisor_liveness.as_raw_fd(),
                    supervisor_liveness.as_raw_fd(),
                    POLL_TICK_MS,
                );
                (status_readable, false)
            };
            if status_readable {
                let status = read_provider_status(supervisor_liveness.as_raw_fd());
                // Do not observe the monitor through Child::try_wait before
                // this kill. Whether it is still running or is an unreaped
                // zombie, its PID cannot be reused and safely anchors the PGID.
                kill_provider_group(provider_pid, &mut provider);
                let status = status.map_err(|source| {
                    ProviderFailure::Failed(format!(
                        "reading `{}` {} status from its monitor failed: {source}",
                        options.provider[0],
                        phase.as_str()
                    ))
                })?;
                if status.success() {
                    return Ok(());
                }
                return Err(ProviderFailure::Failed(format!(
                    "`{}` {} exited with {status}",
                    options.provider[0],
                    phase.as_str()
                )));
            }
            if Instant::now() >= deadline {
                kill_provider_group(provider_pid, &mut provider);
                return Err(ProviderFailure::Failed(format!(
                    "`{}` {} timed out after {:?}",
                    options.provider[0],
                    phase.as_str(),
                    options.provider_timeout
                )));
            }
            if signal_readable && let Some(signo) = drain_signal_pipe(signal_fd) {
                kill_provider_group(provider_pid, &mut provider);
                return Err(ProviderFailure::Interrupted(signo));
            }
        }
    }

    fn read_provider_status(fd: i32) -> std::result::Result<ExitStatus, std::io::Error> {
        let mut raw_status = [0u8; std::mem::size_of::<i32>()];
        let mut offset = 0usize;
        while offset < raw_status.len() {
            // SAFETY: the remaining slice is valid and writable for the read.
            let rc = unsafe {
                libc::read(
                    fd,
                    raw_status[offset..].as_mut_ptr().cast(),
                    raw_status.len() - offset,
                )
            };
            if rc > 0 {
                offset += rc as usize;
                continue;
            }
            if rc == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "provider monitor closed its status channel",
                ));
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
        Ok(ExitStatus::from_raw(i32::from_ne_bytes(raw_status)))
    }

    fn provider_monitor_stderr_stdio(
        options: &SuperviseOptions,
    ) -> std::result::Result<Stdio, ProviderFailure> {
        // SAFETY: F_DUPFD_CLOEXEC returns a fresh fd the Stdio takes ownership
        // of; the spawn machinery dup2s it onto the monitor's stderr slot.
        let dup_fd = unsafe {
            libc::fcntl(
                options.diag_fd,
                libc::F_DUPFD_CLOEXEC,
                super::super::SANDBOX_DIAG_FD + 1,
            )
        };
        if dup_fd < 0 {
            let errno = std::io::Error::last_os_error();
            return Err(ProviderFailure::Failed(format!(
                "duplicating the diagnostic fd for provider monitor stderr failed: {errno}"
            )));
        }
        // SAFETY: dup_fd is owned and unshared.
        Ok(unsafe { <Stdio as std::os::fd::FromRawFd>::from_raw_fd(dup_fd) })
    }

    fn kill_provider_group(provider_pid: i32, provider: &mut Child) {
        // SAFETY: the provider monitor was spawned with process_group(0), so its
        // pid is its pgid; the actual provider and descendants inherit it.
        unsafe { libc::kill(-provider_pid, libc::SIGKILL) };
        if let Err(source) = provider.wait() {
            eprintln!("acps sandbox-supervise: reaping the provider failed: {source}");
        }
    }

    /// Random per-spawn identifier for `ACPS_SANDBOX_NETWORK_ID`, generated at
    /// supervise time so concurrent spawns from identical argv stay unique.
    fn generate_network_id() -> String {
        let mut bytes = [0u8; 16];
        rand::rng().fill(&mut bytes);
        let mut out = String::with_capacity(32);
        for byte in bytes {
            out.push_str(&format!("{byte:02x}"));
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn pidfd_signals_the_opened_process_identity() {
            let mut child = Command::new("/bin/sleep")
                .arg("30")
                .spawn()
                .expect("spawn sleep");
            let pidfd = pidfd_open(child.id() as i32).expect("open child pidfd");
            pidfd_send_signal(&pidfd, libc::SIGTERM).expect("signal child through pidfd");
            let status = child.wait().expect("wait child");
            assert_eq!(status.signal(), Some(libc::SIGTERM));
        }

        #[test]
        fn pidfd_does_not_target_a_reused_pid_after_exit() {
            let mut child = Command::new("/bin/true").spawn().expect("spawn true");
            let pidfd = pidfd_open(child.id() as i32).expect("open child pidfd");
            child.wait().expect("reap child");
            let error = pidfd_send_signal(&pidfd, libc::SIGTERM)
                .expect_err("an exited process identity must reject signaling");
            assert_eq!(error.raw_os_error(), Some(libc::ESRCH));
        }

        #[test]
        fn workload_child_identity_requires_the_same_single_child() {
            validate_workload_child_identity(41, &[41]).expect("identity should match");
            assert!(validate_workload_child_identity(41, &[]).is_err());
            assert!(validate_workload_child_identity(41, &[42]).is_err());
            assert!(validate_workload_child_identity(41, &[41, 42]).is_err());
        }

        #[test]
        fn pidfd_preflight_error_names_the_rejected_syscall() {
            let message =
                pidfd_preflight_error("pidfd_send_signal", &std::io::Error::from_raw_os_error(1));
            assert!(message.contains("pidfd_send_signal"));
            assert!(message.contains("kernel or seccomp policy"));
        }
    }
}
