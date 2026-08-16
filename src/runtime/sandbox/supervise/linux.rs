//! Linux implementation of the network sandbox supervisor: the spawn/release
//! lifecycle and the provider monitor runtime. Argument parsing, descriptor
//! plumbing, pidfd handling, and signal forwarding live in the siblings.

use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

use rand::RngExt;

use super::super::{
    SANDBOX_DIAG_FD, SANDBOX_EXEC_SUBCOMMAND, SANDBOX_PROVIDER_SUPERVISE_SUBCOMMAND,
};
use super::{
    ENV_NETWORK_ID, ENV_NETWORK_NAMESPACE, ENV_NETWORK_PID, ENV_NETWORK_PROTOCOL,
    NETWORK_PROTOCOL_VERSION, RELEASE_BYTE, SETUP_FAILED_EXIT, TEARDOWN_FAILED_EXIT, read_byte,
    set_cloexec, write_byte,
};
use crate::config::SandboxProviderStderr;
use crate::error::{Result, StackError};

mod args;
mod fds;
mod pidfd;
mod signals;

use args::*;
use fds::*;
use pidfd::*;
use signals::*;

pub(super) use pidfd::preflight_pidfd_support;

#[cfg(test)]
mod tests;

// CONSTANTS

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

pub(super) fn run(raw_args: Vec<String>) -> Result<()> {
    let options = parse_args(raw_args)?;
    let diag_fd = options.diag_fd;
    if let Err(error) = run_with_options(options) {
        Diag { fd: diag_fd }.line(&error.to_string());
        std::process::exit(SETUP_FAILED_EXIT);
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
    let provider_phase = |phase: ProviderPhase, workload_pid: Option<i32>| {
        run_provider(
            phase,
            &options,
            &ns_path,
            &network_id,
            workload_pid,
            signal_fd,
        )
    };

    // Phase 3: provider setup gates workload execution. No provider means
    // deny-all networking: the namespace stays exactly as unshare made it.
    if !options.provider.is_empty()
        && let Err(failure) = provider_phase(ProviderPhase::Setup, Some(unshare_pid))
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
        if let Err(teardown_failure) = provider_phase(ProviderPhase::Teardown, None) {
            diag.line(&teardown_failure.describe("teardown after failed setup"));
        }
        match failure {
            ProviderFailure::Interrupted(signo) => exit_for_signal(signo),
            ProviderFailure::Failed(_) => std::process::exit(SETUP_FAILED_EXIT),
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
        provider_phase(ProviderPhase::Teardown, None)
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
    for name in [ENV_NETWORK_PROTOCOL, ENV_NETWORK_ID, ENV_NETWORK_NAMESPACE] {
        let value = std::env::var_os(name).ok_or_else(|| StackError::SandboxFailed {
            reason: format!("sandbox-provider-supervise requires environment variable {name}"),
        })?;
        command.env(name, value);
    }
    if let Some(value) = std::env::var_os(ENV_NETWORK_PID) {
        command.env(ENV_NETWORK_PID, value);
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
                eprintln!("acps sandbox-provider-supervise: waiting for provider failed: {source}");
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
    std::process::exit(SETUP_FAILED_EXIT);
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
    command.args([SANDBOX_PROVIDER_SUPERVISE_SUBCOMMAND, "--liveness-fd"]);
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
    command.env(ENV_NETWORK_PROTOCOL, NETWORK_PROTOCOL_VERSION);
    command.env(ENV_NETWORK_ID, network_id);
    command.env(ENV_NETWORK_NAMESPACE, ns_path);
    if let Some(pid) = workload_pid {
        command.env(ENV_NETWORK_PID, pid.to_string());
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
    let dup_fd =
        unsafe { libc::fcntl(options.diag_fd, libc::F_DUPFD_CLOEXEC, SANDBOX_DIAG_FD + 1) };
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
