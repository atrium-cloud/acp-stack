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
//! drifting on security-relevant behavior. The runners keep their own
//! orchestration; only the leaf helpers live in this module.

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

/// Unix process-group kill for a synchronous child. The child MUST have been
/// spawned with `process_group(0)`; otherwise the negative pid won't reach
/// the grandchildren a shell forked.
#[cfg(unix)]
pub fn kill_process_group(child: &mut std::process::Child) {
    // SAFETY: libc::kill is async-signal-safe and we operate on a pid we own
    // (the process-group leader is the child itself because the caller used
    // `process_group(0)`). A negative pid addresses the whole process group,
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
