//! Process-execution plumbing for install actions: spawning the shell
//! (optionally under `sudo -n`), bounded output capture, timeout kill and
//! reap, plus the PATH/env helpers those depend on.

use super::*;

/// Per-stream cap on captured output before we start dropping bytes.
/// Reuses the state-layer constant so a future bump in installer_runs
/// row size automatically applies to deps_apply too.
const STREAM_CAP_BYTES: usize = INSTALLER_OUTPUT_CAP_BYTES;

/// Return tuple: `(exit_code, stdout, stderr_prefix, timed_out,
/// stderr_tail)` — see `read_to_cap_with_tail` for why `stderr_tail`
/// is computed separately.
pub(crate) fn run_shell(
    shell_program: &str,
    script: &str,
    timeout: Duration,
    sudo: Option<&Path>,
) -> Result<(Option<i32>, String, String, bool, String)> {
    let mut command = match sudo {
        Some(sudo_path) => {
            let mut command = Command::new(sudo_path);
            command
                .arg(SUDO_NON_INTERACTIVE_FLAG)
                .arg(shell_program)
                .arg("-c")
                .arg(escalated_script(script));
            command
        }
        None => {
            let mut command = Command::new(shell_program);
            command.arg("-c").arg(script);
            command
        }
    };
    command.env_clear().envs(scrubbed_env());
    apply_non_interactive_env(&mut command);

    // `run_captured` owns the piped stdio, the session detachment (so a
    // timeout kill reaches every grandchild the shell forked and a dep script
    // probing /dev/tty cannot prompt), the capped reader threads and the
    // bounded wait; only the timeout reap policy below is specific to
    // deps apply.
    let outcome =
        crate::runtime::process_runner::run_captured(&mut command, timeout, STREAM_CAP_BYTES)
            .map_err(|source| StackError::AgentSpawnFailed { source })?;

    let (status, timed_out, stdout, stderr, stderr_tail) = match outcome {
        CaptureOutcome::Exited {
            status,
            stdout,
            stderr,
            stderr_tail,
        } => (status, false, stdout, stderr, stderr_tail),
        CaptureOutcome::TimedOut {
            mut child,
            stdout_reader,
            stderr_reader,
        } => {
            // On escalated runs the child is root-owned, so our SIGKILL is
            // refused with EPERM and an unbounded `wait()` would hang the
            // apply; the bounded reap (plus the bounded reader joins below)
            // keeps the outcome reported as a timeout Failed either way.
            kill_process_group(&mut child);
            if reap_with_grace(&mut child, KILL_REAP_GRACE).is_none() {
                // Root-owned (escalated) children ignore our SIGKILL, so the
                // unreaped child lingers as a zombie until the process exits.
                // Surface it rather than leak silently.
                tracing::warn!(
                    "dep install action outlived its timeout kill and was abandoned unreaped (pid={})",
                    child.id(),
                );
                // Still alive after the grace, so it may hold the pipes open;
                // signal the group once more before the bounded joins.
                kill_process_group(&mut child);
            }
            // Bounded join: a double-forked daemon that escaped the process
            // group could still hold our pipe descriptors open. We can't
            // SIGKILL it (we don't have a pid), so we wait `READER_JOIN_GRACE`
            // for the close to land and then abandon the thread if it didn't.
            // Abandoning is fine here — the OS reaps the orphaned thread when
            // `acps` exits, and dropping the captured output is preferable to
            // hanging the entire `deps apply` call.
            let stdout = stdout_reader
                .and_then(join_reader_bounded)
                .unwrap_or_default();
            let (stderr, stderr_tail) = stderr_reader
                .and_then(join_reader_bounded)
                .unwrap_or_default();
            (
                std::process::ExitStatus::default(),
                true,
                stdout,
                stderr,
                stderr_tail,
            )
        }
        CaptureOutcome::WaitFailed { source, .. } => {
            return Err(StackError::AgentSpawnFailed { source });
        }
    };
    Ok((status.code(), stdout, stderr, timed_out, stderr_tail))
}

/// Bounded reap after a group kill. A root-owned (escalated) child cannot be
/// signalled by a non-root parent, so a plain `wait()` would block forever.
/// Returns `None` when the child outlives the grace; callers already treat
/// that as a timeout and the pipe-reader joins are separately bounded.
pub(crate) fn reap_with_grace(
    child: &mut std::process::Child,
    grace: Duration,
) -> Option<std::process::ExitStatus> {
    wait_with_timeout(child, Instant::now() + grace)
        .ok()
        .flatten()
}

pub(crate) fn cap_stream(value: &str) -> String {
    if value.len() <= STREAM_CAP_BYTES {
        return value.to_owned();
    }
    let mut cutoff = STREAM_CAP_BYTES;
    while cutoff > 0 && !value.is_char_boundary(cutoff) {
        cutoff -= 1;
    }
    value[..cutoff].to_owned()
}

pub(crate) fn scrubbed_env() -> HashMap<String, String> {
    let mut env = HashMap::new();
    if let Ok(value) = std::env::var("PATH") {
        env.insert("PATH".to_owned(), value);
    }
    if let Ok(value) = std::env::var("HOME") {
        env.insert("HOME".to_owned(), value);
    }
    if let Ok(value) = std::env::var("LANG") {
        env.insert("LANG".to_owned(), value);
    }
    env
}

pub(crate) fn resolve_command(name: &str) -> Option<std::path::PathBuf> {
    if name.contains('/') {
        let path = Path::new(name).to_path_buf();
        return is_executable_file(&path).then_some(path);
    }
    let path_env = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_env) {
        let candidate = dir.join(name);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// True when `path` is a regular file that has at least one execute
/// bit set on Unix. A failed `chmod` after an `install` action would
/// otherwise let the postcheck report success against a non-executable
/// placeholder. On non-Unix targets, fall back to `is_file()` since
/// there's no mode bit semantic.
fn is_executable_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match std::fs::metadata(path) {
            Ok(meta) => (meta.mode() & 0o111) != 0,
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        true
    }
}
