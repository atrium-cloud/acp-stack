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
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear()
        .envs(scrubbed_env());
    apply_non_interactive_env(&mut command);
    // Detach into a fresh session so a timeout-induced kill reaches every
    // grandchild the shell forked (without this, `child.kill()` only stops
    // the shell — a `sleep 999` it spawned would keep the stdout/stderr
    // pipes open and the join threads would block forever), and so a dep
    // script probing /dev/tty cannot prompt. Same pattern as agent_installer.
    detach_into_new_session(&mut command);
    let mut child = command
        .spawn()
        .map_err(|source| StackError::AgentSpawnFailed { source })?;

    let stdout_handle = child.stdout.take().expect("piped stdout");
    let stderr_handle = child.stderr.take().expect("piped stderr");

    let stdout_thread = std::thread::spawn(move || read_to_cap(stdout_handle, STREAM_CAP_BYTES));
    let stderr_thread = std::thread::spawn(move || {
        read_to_cap_with_tail(stderr_handle, STREAM_CAP_BYTES, STDERR_TAIL_BYTES)
    });

    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    // On escalated runs the child is root-owned, so our
                    // SIGKILL is refused with EPERM and an unbounded
                    // `wait()` would hang the apply; the bounded reap
                    // (plus the bounded reader joins below) keeps the
                    // outcome reported as a timeout Failed either way.
                    kill_process_group(&mut child);
                    timed_out = true;
                    if reap_with_grace(&mut child, KILL_REAP_GRACE).is_none() {
                        // Root-owned (escalated) children ignore our SIGKILL,
                        // so the unreaped child lingers as a zombie until the
                        // process exits. Surface it rather than leak silently.
                        tracing::warn!(
                            "dep install action outlived its timeout kill and was abandoned unreaped (pid={})",
                            child.id(),
                        );
                    }
                    break std::process::ExitStatus::default();
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(err) => {
                return Err(StackError::AgentSpawnFailed { source: err });
            }
        }
    };
    // Always kill the process group, even on a clean shell exit. If
    // the shell forked a background grandchild that inherited
    // stdout/stderr, the reader threads would block forever waiting
    // for EOF on those pipes. Killing the group closes the pipes
    // (the child's std handles get released), so the readers see
    // EOF and the joins below return.
    kill_process_group(&mut child);
    // Bounded join: a double-forked daemon that escaped the process
    // group could still hold our pipe descriptors open. We can't
    // SIGKILL it (we don't have a pid), so we wait `READER_JOIN_GRACE`
    // for the close to land and then abandon the thread if it didn't.
    // Abandoning is fine here — the OS reaps the orphaned thread when
    // `acps` exits, and dropping the captured output is preferable to
    // hanging the entire `deps apply` call.
    let stdout = join_reader_bounded(stdout_thread).unwrap_or_default();
    let (stderr, stderr_tail) =
        join_reader_bounded(stderr_thread).unwrap_or((String::new(), String::new()));
    let exit_code = status.code();
    Ok((exit_code, stdout, stderr, timed_out, stderr_tail))
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
