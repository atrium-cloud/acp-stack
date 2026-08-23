//! Shared process-spawning primitives used by the agent installer, the
//! deps-apply runner, and the ACP bridge.
//!
//! The three runners independently grew near-identical scaffolding for:
//!
//! - Spawning a child into a fresh Unix process group so a timeout-induced
//!   SIGKILL also reaches grandchildren the shell forked.
//! - Capping stdout/stderr at ingest so a chatty child cannot bloat the
//!   row that ends up in `installer_runs`.
//! - A bounded join on the reader threads so a stuck thread cannot wedge an
//!   HTTP request.
//! - Scrubbing/forwarding only the host env vars the child legitimately
//!   needs (PATH, HOME, LANG).
//!
//! Centralizing those primitives here prevents the three call sites from
//! drifting on security-relevant behavior. Alongside the leaf helpers this
//! module owns [`run_captured`], the one spawn → capture → wait composition
//! all three runners share; the timeout and wait-failure policies, which
//! genuinely differ per runner, stay at the call sites.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Bytes kept from the tail of stderr when the full stream blows past the
/// `cap_bytes` limit. The truncated prefix is rarely useful for a failed
/// install — the diagnostic that caused the exit code lives at the very end.
pub const STDERR_TAIL_BYTES: usize = 2 * 1024;

/// Worst-case wait for reader threads to drain after the child's pipes are
/// closed. After `kill_process_group` the pipes close almost immediately so
/// reader threads return; this bound exists for the kernel-level edge case
/// where the close is delayed, so a stuck thread cannot wedge an HTTP request.
pub const READER_JOIN_GRACE: Duration = Duration::from_secs(2);

/// Upper bound for any operator- or catalog-supplied install timeout. 24
/// hours is already far past any real install; the cap exists because
/// `run_captured` computes its deadline as `Instant::now() + timeout`, and
/// that addition panics on overflow for a near-u64::MAX value.
pub const MAX_INSTALL_TIMEOUT_SECS: u64 = 86_400;

/// Forward a single named host env var to a sync `Command`, if present on the
/// daemon. Unset on the host means unset on the child — never fabricated.
pub fn forward_host_env(command: &mut Command, name: &str) {
    if let Some(value) = std::env::var_os(name) {
        command.env(name, value);
    }
}

/// Same as [`forward_host_env`] but for a `tokio::process::Command`.
pub fn forward_host_env_tokio(command: &mut tokio::process::Command, name: &str) {
    if let Some(value) = std::env::var_os(name) {
        command.env(name, value);
    }
}

/// Prepend `extra_path_dirs` (in order) to the daemon's PATH and return a
/// joined `OsString` suitable for `Command::env("PATH", _)`. Empty fragments
/// are dropped so a caller can pass an unconditional slice without filtering.
pub fn path_env_with_extra_dirs(extra_path_dirs: &[&Path]) -> Option<OsString> {
    let existing = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = Vec::new();
    for dir in extra_path_dirs {
        if !dir.as_os_str().is_empty() {
            paths.push((*dir).to_path_buf());
        }
    }
    paths.extend(std::env::split_paths(&existing));
    std::env::join_paths(paths).ok()
}

/// Resolve a bare command name against the daemon's PATH. Absolute or
/// slash-containing paths are returned as-is when they point at an existing
/// regular file; otherwise the search walks the daemon's PATH directories.
/// Returns `None` if nothing matches.
pub fn resolve_in_path(name: &str) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }
    if name.contains('/') {
        let path = Path::new(name).to_path_buf();
        return if path.is_file() { Some(path) } else { None };
    }
    let path_env = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_env) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Detach a synchronous child into a brand-new session (and therefore a new
/// process group with `pgid == pid`). Beyond the group-kill guarantee this
/// also drops the controlling terminal: a vendor installer that opens
/// `/dev/tty` to prompt ("existing install detected, overwrite?") gets ENXIO
/// and takes its non-interactive branch instead of stopping on SIGTTIN until
/// our timeout fires. Callers must use this INSTEAD of `process_group(0)`:
/// `setsid` fails with EPERM for a process that is already a group leader.
#[cfg(unix)]
pub fn detach_into_new_session(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: pre_exec runs after fork and before exec; setsid/setpgid are
    // async-signal-safe. setsid makes the child a session leader with no
    // controlling terminal and pgid == pid, preserving the negative-pid
    // contract of `kill_process_group`. If setsid somehow fails, fall back
    // to a plain process group so the group-kill invariant still holds.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() != -1 {
                return Ok(());
            }
            // Report setsid's errno on double failure — it is the primary
            // call and its errno is the diagnostic one.
            let setsid_error = std::io::Error::last_os_error();
            if libc::setpgid(0, 0) == 0 {
                Ok(())
            } else {
                Err(setsid_error)
            }
        });
    }
}

#[cfg(not(unix))]
pub fn detach_into_new_session(_command: &mut Command) {}

/// Whether a process with this pid exists right now. `kill(pid, 0)` delivers
/// no signal; EPERM still proves the pid is occupied, only ESRCH proves it is
/// gone. Non-positive pids never address a single process, so they read as
/// not live. A Linux zombie also reads as not live: `kill(pid, 0)` succeeds
/// for one, but a zombie has already exited — treating it as live would blind
/// liveness-driven cleanup (the deps-apply abandoned-run reconcile) for as
/// long as the unreaped parent survives.
#[cfg(unix)]
pub fn process_is_live(pid: i64) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: kill with signal 0 performs only the existence/permission
    // check; no signal is delivered.
    let result = unsafe { libc::kill(pid, 0) };
    if result != 0 {
        return std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
    }
    !proc_stat_says_zombie(pid)
}

/// Linux-only zombie check via the state field of `/proc/{pid}/stat` (the
/// first character after the parenthesised comm, which may itself contain
/// parentheses — hence `rfind`). Hosts without procfs return false, leaving
/// the plain `kill` result in force.
#[cfg(unix)]
fn proc_stat_says_zombie(pid: libc::pid_t) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let Some(close_paren) = stat.rfind(')') else {
        return false;
    };
    stat[close_paren + 1..]
        .split_whitespace()
        .next()
        .is_some_and(|state| state == "Z")
}

#[cfg(not(unix))]
pub fn process_is_live(_pid: i64) -> bool {
    false
}

/// Kernel boot id, used to guard stored pids against reuse across reboots.
/// `None` where the kernel does not expose one (non-Linux dev hosts); callers
/// then fall back to the plain pid check.
pub fn current_boot_id() -> Option<String> {
    let raw = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

/// Env vars that steer installers/updaters away from interactive prompts.
/// Losing the controlling terminal (see [`detach_into_new_session`]) already
/// breaks `/dev/tty` prompts; these cover tools that decide interactivity
/// from the environment instead.
pub const NON_INTERACTIVE_ENV: &[(&str, &str)] = &[
    ("CI", "1"),
    ("NONINTERACTIVE", "1"),
    ("DEBIAN_FRONTEND", "noninteractive"),
    ("GIT_TERMINAL_PROMPT", "0"),
    ("TERM", "dumb"),
];

/// Apply [`NON_INTERACTIVE_ENV`] to a sync `Command`.
pub fn apply_non_interactive_env(command: &mut Command) {
    for (name, value) in NON_INTERACTIVE_ENV {
        command.env(name, value);
    }
}

/// The real interpreter behind `python3`, resolved against `path_env` (the
/// PATH the child will run with, so this resolves the same `python3` the child
/// would). `None` when there is no usable `python3`.
///
/// This exists for node-gyp, which spawns `python3` once per native module it
/// configures. Where that name is a wrapper script rather than a binary — the
/// shim a Python version manager installs is exactly that — every one of those
/// spawns pays the wrapper's cost, and a package tree with many native modules
/// can burn an entire install budget without a compiler ever running. Asking
/// the interpreter for its own path costs one wrapper invocation and leaves
/// node-gyp calling the binary directly.
///
/// Inert where `python3` is already a binary: `sys.executable` then names that
/// same file, so the caller sets the variable to the value node-gyp would have
/// resolved anyway.
///
/// The probe is bounded by [`PYTHON_PROBE_TIMEOUT`]: it runs before the
/// installer's step deadline is computed, so a hung version-manager shim would
/// otherwise wedge the install thread outside every budget. The correct
/// degradation for a shim that cannot answer in seconds is leaving
/// `npm_config_python` unset.
pub fn resolved_python_interpreter(path_env: Option<&OsString>) -> Option<PathBuf> {
    resolved_python_interpreter_with_timeout(path_env, PYTHON_PROBE_TIMEOUT)
}

/// Upper bound on the `python3 -c 'sys.executable'` probe. One interpreter
/// startup is all the probe needs; past a few seconds the `python3` on PATH is
/// a hung version-manager shim, and the correct degradation is leaving
/// `npm_config_python` unset rather than waiting on it.
const PYTHON_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Stream cap for the probe: `sys.executable` prints a single path, so
/// anything beyond a few KiB is a misbehaving shim, not an answer.
const PYTHON_PROBE_STREAM_CAP: usize = 4 * 1024;

/// The timeout-parameterized body of [`resolved_python_interpreter`], split
/// out so tests can shrink the bound instead of sleeping past the real one.
pub(crate) fn resolved_python_interpreter_with_timeout(
    path_env: Option<&OsString>,
    timeout: Duration,
) -> Option<PathBuf> {
    let mut probe = Command::new("python3");
    probe.args(["-c", "import sys; print(sys.executable)"]);
    if let Some(path) = path_env {
        probe.env("PATH", path);
    }
    let outcome = run_captured(&mut probe, timeout, PYTHON_PROBE_STREAM_CAP).ok()?;
    let stdout = match outcome {
        CaptureOutcome::Exited { status, stdout, .. } if status.success() => stdout,
        CaptureOutcome::Exited { .. } => return None,
        CaptureOutcome::TimedOut { mut child, .. } => {
            // The shim hung: kill the group and degrade to "no override".
            kill_process_group(&mut child);
            if let Err(error) = child.wait() {
                tracing::debug!(%error, "timed-out python probe reap failed");
            }
            return None;
        }
        CaptureOutcome::WaitFailed { mut child, .. } => {
            // `CaptureOutcome` hands back the live child on a wait failure;
            // leaving it running would leak the probe.
            kill_process_group(&mut child);
            if let Err(error) = child.wait() {
                tracing::debug!(%error, "unwaitable python probe reap failed");
            }
            return None;
        }
    };
    let resolved = PathBuf::from(stdout.trim());
    // A frozen or embedded interpreter reports an empty `sys.executable`;
    // pointing node-gyp at that would be worse than leaving it to search.
    resolved.is_file().then_some(resolved)
}

/// Unix process-group kill for a synchronous child. The child MUST have been
/// spawned with `process_group(0)` or [`detach_into_new_session`]; otherwise
/// the negative pid won't reach the grandchildren a shell forked.
#[cfg(unix)]
pub fn kill_process_group(child: &mut std::process::Child) {
    // SAFETY: libc::kill is async-signal-safe and we operate on a pid we own
    // (the process-group leader is the child itself because the caller used
    // `process_group(0)` or `detach_into_new_session`, both of which yield
    // pgid == pid). A negative pid addresses the whole process group,
    // so grandchildren forked by the shell also receive SIGKILL.
    unsafe {
        let pid = child.id() as i32;
        libc::kill(-pid, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
pub fn kill_process_group(child: &mut std::process::Child) {
    let _ = child.kill();
}

/// Tokio equivalent of [`kill_process_group`] for async children. Same
/// preconditions: the child must have been spawned with `process_group(0)`.
#[cfg(unix)]
pub fn kill_tokio_process_group(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        // SAFETY: see [`kill_process_group`].
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
pub fn kill_tokio_process_group(child: &mut tokio::process::Child) {
    let _ = child.start_kill();
}

/// Poll a synchronous child until it exits or `deadline` elapses. Returns
/// `Ok(None)` on timeout. Callers handle the kill+drain follow-up themselves
/// because the cleanup differs between callers.
pub fn wait_with_timeout(
    child: &mut std::process::Child,
    deadline: Instant,
) -> std::io::Result<Option<std::process::ExitStatus>> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(Some(status)),
            Ok(None) => {
                if Instant::now() >= deadline {
                    return Ok(None);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(err) => return Err(err),
        }
    }
}

/// Spawn a dedicated thread that drains `reader` to a lossy-UTF-8 string,
/// capped at `cap_bytes`. Bytes beyond the cap are discarded so the child
/// can keep writing without blocking on a full pipe buffer. Without a
/// dedicated drainer, a chatty child can fill the OS pipe buffer and wedge.
pub fn spawn_capped_reader<R>(reader: R, cap_bytes: usize) -> JoinHandle<String>
where
    R: Read + Send + 'static,
{
    std::thread::spawn(move || read_to_cap(reader, cap_bytes))
}

/// Synchronously read `reader` to a lossy-UTF-8 string, capped at `cap_bytes`.
/// The cap is enforced on ingest so a chatty child cannot bloat downstream
/// storage. Once the cap is hit the rest of the stream is drained into the
/// null sink so the child keeps making progress.
pub fn read_to_cap<R: Read>(mut reader: R, cap_bytes: usize) -> String {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if buf.len() + n > cap_bytes {
                    let remaining = cap_bytes.saturating_sub(buf.len());
                    buf.extend_from_slice(&chunk[..remaining]);
                    // Drain the rest without storing — the cap is enforced
                    // on ingest so a chatty installer can't bloat
                    // `installer_runs`.
                    let mut sink = std::io::sink();
                    let _ = std::io::copy(&mut reader, &mut sink);
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// Same cap as [`read_to_cap`], but also maintains a rolling buffer of the
/// LAST `tail_bytes` bytes ever seen on the stream. For a failed install
/// whose stderr blew past the prefix cap, the truncated prefix is rarely
/// useful — the actual diagnostic lives at the very end. The rolling tail
/// captures that without bloating the prefix.
pub fn read_to_cap_with_tail<R: Read>(
    mut reader: R,
    cap_bytes: usize,
    tail_bytes: usize,
) -> (String, String) {
    let mut prefix = Vec::with_capacity(4096);
    let mut tail = std::collections::VecDeque::with_capacity(tail_bytes);
    let mut prefix_full = false;
    let mut chunk = [0u8; 4096];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let bytes = &chunk[..n];
                if !prefix_full {
                    let remaining = cap_bytes.saturating_sub(prefix.len());
                    let take = n.min(remaining);
                    prefix.extend_from_slice(&bytes[..take]);
                    if prefix.len() >= cap_bytes {
                        prefix_full = true;
                    }
                }
                // Always keep updating the rolling tail, even after the
                // prefix cap fills.
                for byte in bytes {
                    if tail.len() == tail_bytes {
                        tail.pop_front();
                    }
                    tail.push_back(*byte);
                }
            }
            Err(_) => break,
        }
    }
    let prefix_string = String::from_utf8_lossy(&prefix).into_owned();
    let tail_buf: Vec<u8> = tail.into_iter().collect();
    // The tail may start mid-UTF-8-character if we truncated on a boundary;
    // nudge forward until we find a leading byte.
    let mut start = 0;
    while start < tail_buf.len() && (tail_buf[start] & 0xC0) == 0x80 {
        start += 1;
    }
    let tail_string = String::from_utf8_lossy(&tail_buf[start..]).into_owned();
    (prefix_string, tail_string)
}

/// Outcome of [`run_captured`].
///
/// Only the clean-exit path is completed by the helper, because that is the
/// one path every caller handles identically (kill the group, bounded-join the
/// readers, map the exit status). The timeout and wait-failure paths hand the
/// live child and the still-running reader threads back so each runner can
/// apply its own reap/kill/reporting policy, which genuinely differs: the
/// deps-apply runner reaps with a grace and warns because an escalated child is
/// root-owned and may refuse SIGKILL, the installer surfaces a typed timeout
/// error and discards the captured output, and the updater turns the timeout
/// into a persisted `timeout` row that keeps the output.
///
/// Dropping a `TimedOut` or `WaitFailed` value leaks the live child and both
/// reader threads — callers must kill/reap the child and join or drain the
/// readers themselves.
#[must_use]
pub enum CaptureOutcome {
    Exited {
        status: std::process::ExitStatus,
        stdout: String,
        stderr: String,
        /// Rolling tail of stderr; see [`read_to_cap_with_tail`].
        stderr_tail: String,
    },
    TimedOut {
        child: std::process::Child,
        stdout_reader: Option<JoinHandle<String>>,
        stderr_reader: Option<JoinHandle<(String, String)>>,
    },
    WaitFailed {
        source: std::io::Error,
        child: std::process::Child,
        stdout_reader: Option<JoinHandle<String>>,
        stderr_reader: Option<JoinHandle<(String, String)>>,
    },
}

/// Spawn `command` detached, capture both streams through capped reader
/// threads, and wait up to `timeout`.
///
/// The caller configures the program, arguments, cwd and environment; this
/// helper owns the parts that must not drift between runners: null stdin with
/// piped stdout/stderr, session detachment (so a timeout kill reaches
/// grandchildren and `/dev/tty` prompts cannot block), the per-stream ingest
/// cap, and the bounded wait. A spawn failure is returned as the raw
/// `io::Error` so each caller keeps its own mapping (typed error vs. logged
/// row).
pub fn run_captured(
    command: &mut Command,
    timeout: Duration,
    stream_cap_bytes: usize,
) -> std::io::Result<CaptureOutcome> {
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    detach_into_new_session(command);
    let mut child = command.spawn()?;

    let stdout_reader = child
        .stdout
        .take()
        .map(|stream| spawn_capped_reader(stream, stream_cap_bytes));
    // Always compute the stderr tail: the prefix is byte-identical to a plain
    // capped read, so callers that do not want the tail simply ignore it.
    let stderr_reader = child.stderr.take().map(|stream| {
        std::thread::spawn(move || {
            read_to_cap_with_tail(stream, stream_cap_bytes, STDERR_TAIL_BYTES)
        })
    });

    match wait_with_timeout(&mut child, Instant::now() + timeout) {
        Ok(Some(status)) => {
            // Kill the group even on a clean exit: a grandchild that inherited
            // stdout/stderr would otherwise hold the pipes open and block the
            // reader threads on EOF forever.
            kill_process_group(&mut child);
            let stdout = stdout_reader
                .and_then(join_reader_bounded)
                .unwrap_or_default();
            let (stderr, stderr_tail) = stderr_reader
                .and_then(join_reader_bounded)
                .unwrap_or_default();
            Ok(CaptureOutcome::Exited {
                status,
                stdout,
                stderr,
                stderr_tail,
            })
        }
        Ok(None) => Ok(CaptureOutcome::TimedOut {
            child,
            stdout_reader,
            stderr_reader,
        }),
        Err(source) => Ok(CaptureOutcome::WaitFailed {
            source,
            child,
            stdout_reader,
            stderr_reader,
        }),
    }
}

/// Poll-join a thread up to [`READER_JOIN_GRACE`]. After we kill the process
/// group the pipes close almost immediately so reader threads return; this
/// bound guards against a kernel-level edge case where the close is delayed.
/// Returns `None` if the thread didn't finish in time — abandon and let the
/// OS reap it when the daemon exits.
pub fn join_reader_bounded<T>(handle: JoinHandle<T>) -> Option<T> {
    let deadline = Instant::now() + READER_JOIN_GRACE;
    while !handle.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    if handle.is_finished() {
        handle.join().ok()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn detached_child_is_session_leader() {
        let mut command = Command::new("sh");
        command.arg("-c").arg("sleep 0.2");
        detach_into_new_session(&mut command);
        let mut child = command.spawn().expect("spawn");
        let pid = child.id() as i32;
        // Session leadership is what severs the controlling terminal, which
        // is the property the installer relies on to defeat /dev/tty prompts.
        // SAFETY: getsid on a pid we own performs no memory access.
        let sid = unsafe { libc::getsid(pid) };
        assert_eq!(sid, pid, "detached child must lead its own session");
        let _ = child.wait();
    }

    #[cfg(unix)]
    #[test]
    fn captured_run_reports_exit_status_and_both_streams() {
        let mut command = Command::new("sh");
        command.arg("-c").arg("echo out; echo err 1>&2; exit 3");
        let outcome =
            run_captured(&mut command, Duration::from_secs(30), 64 * 1024).expect("spawn");
        match outcome {
            CaptureOutcome::Exited {
                status,
                stdout,
                stderr,
                stderr_tail,
            } => {
                assert_eq!(status.code(), Some(3));
                assert_eq!(stdout.trim(), "out");
                assert_eq!(stderr.trim(), "err");
                assert_eq!(stderr_tail.trim(), "err");
            }
            _ => panic!("expected a clean exit"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn captured_run_hands_back_the_child_on_timeout() {
        let mut command = Command::new("sh");
        command.arg("-c").arg("sleep 30");
        let outcome =
            run_captured(&mut command, Duration::from_millis(100), 64 * 1024).expect("spawn");
        match outcome {
            CaptureOutcome::TimedOut { mut child, .. } => {
                // The helper must leave the child alive so the caller owns the
                // kill/reap policy.
                kill_process_group(&mut child);
                let reaped = wait_with_timeout(&mut child, Instant::now() + READER_JOIN_GRACE);
                assert!(matches!(reaped, Ok(Some(_))), "child must be reapable");
            }
            _ => panic!("expected a timeout"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn detached_child_leads_its_own_process_group() {
        let mut command = Command::new("sh");
        command.arg("-c").arg("sleep 0.2");
        detach_into_new_session(&mut command);
        let mut child = command.spawn().expect("spawn");
        let pid = child.id() as i32;
        // pgid == pid is the precondition `kill_process_group` documents.
        // SAFETY: getpgid on a pid we own performs no memory access.
        let pgid = unsafe { libc::getpgid(pid) };
        assert_eq!(pgid, pid, "detached child must lead its own process group");
        let _ = child.wait();
    }
}
