//! Agent installer.
//!
//! Runs the operator-declared `[agent.install]` recipe to bring a configured
//! ACP agent binary onto disk. The installer is a one-shot shell command with
//! a `creates` precheck/postcheck and an optional `expected_sha256` integrity
//! check.
//!
//! Hardening (see `docs/specs/security.md`):
//!
//! - Timeout (`INSTALLER_TIMEOUT`) so a runaway script cannot wedge
//!   `POST /v1/agent/install` indefinitely.
//! - Per-stream output cap (`MAX_INSTALLER_STREAM_BYTES`) so a chatty
//!   installer cannot bloat `installer_runs`. The state repo also re-truncates
//!   at INSERT time as defense-in-depth.
//! - Scrubbed environment: only `PATH`, `HOME`, `LANG`, and the
//!   `[agent].env`-listed secrets reach the subprocess. The installer must
//!   not be a wider door than the agent itself.
//! - Fresh process group so the timeout-induced SIGKILL reaches grandchildren
//!   the shell forked.
//!
//! `creates` is resolved against `PATH` using `std::env::split_paths`. This
//! mirrors the `which` semantics required by `docs/specs/runtime.md` without
//! a dependency on the `which` crate.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use chrono::{SecondsFormat, Utc};
use sha2::{Digest, Sha256};

use crate::config::AgentInstallConfig;
use crate::error::{Result, StackError};
use crate::state::{INSTALLER_OUTPUT_CAP_BYTES, InstallerRunInput, StateStore};

const INSTALLER_TIMEOUT: Duration = Duration::from_secs(10 * 60);
pub const MAX_INSTALLER_STREAM_BYTES: usize = INSTALLER_OUTPUT_CAP_BYTES;

const STDERR_TAIL_BYTES: usize = 2 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallerOutcome {
    Installed { path: PathBuf, sha256: String },
    AlreadyPresent { path: PathBuf, sha256: String },
}

impl InstallerOutcome {
    pub fn path(&self) -> &Path {
        match self {
            InstallerOutcome::Installed { path, .. }
            | InstallerOutcome::AlreadyPresent { path, .. } => path,
        }
    }

    pub fn sha256(&self) -> &str {
        match self {
            InstallerOutcome::Installed { sha256, .. }
            | InstallerOutcome::AlreadyPresent { sha256, .. } => sha256,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            InstallerOutcome::Installed { .. } => "installed",
            InstallerOutcome::AlreadyPresent { .. } => "already_present",
        }
    }
}

/// One installer execution paired with the row it should write. Returned by
/// [`run_installer_capture`] so the caller can persist the row under a brief
/// lock instead of holding `StateStore` for the entire installer run.
pub struct InstallerResult {
    pub outcome: Result<InstallerOutcome>,
    pub row: InstallerRowDraft,
}

/// Owned snapshot of the row to persist. The state-store call site converts
/// this into the borrowed `InstallerRunInput` at insert time.
pub struct InstallerRowDraft {
    pub started_at: String,
    pub finished_at: Option<String>,
    pub status: String,
    pub stdout: String,
    pub stderr: String,
    pub exit_status: Option<i32>,
}

/// Convenience wrapper used by call sites that already hold the state store
/// briefly: runs the installer and persists the row before returning. The
/// HTTP path uses [`run_installer_capture`] directly so it can drop the
/// state lock during the shell execution.
pub fn run_installer(
    install: &AgentInstallConfig,
    expected_sha256: Option<&str>,
    agent_env: HashMap<String, String>,
    workspace_root: &Path,
    state: &StateStore,
) -> Result<InstallerOutcome> {
    let result = run_installer_capture(install, expected_sha256, agent_env, workspace_root);
    state.append_installer_run(InstallerRunInput {
        started_at: &result.row.started_at,
        finished_at: result.row.finished_at.as_deref(),
        status: &result.row.status,
        stdout: &result.row.stdout,
        stderr: &result.row.stderr,
        exit_status: result.row.exit_status,
    })?;
    result.outcome
}

/// Run the installer WITHOUT touching the state store. Returns the outcome
/// alongside the row draft the caller should persist.
///
/// Steps:
/// 1. Resolve `install.creates` against `PATH`. If found, optionally verify
///    `expected_sha256`, build a `skipped` row, and return `AlreadyPresent`.
/// 2. Otherwise spawn `sh -c <install.shell>` with the hardening described
///    in the module doc, capturing stdout/stderr.
/// 3. Build the row capturing the run.
/// 4. Postcheck: `install.creates` must resolve now. If not, return
///    `AgentInstallerCreatesMissing` even on exit-status 0.
/// 5. Verify `expected_sha256` if set.
pub fn run_installer_capture(
    install: &AgentInstallConfig,
    expected_sha256: Option<&str>,
    agent_env: HashMap<String, String>,
    workspace_root: &Path,
) -> InstallerResult {
    if install.install_type != "shell" {
        return InstallerResult {
            outcome: Err(StackError::AgentNotConfigured),
            row: InstallerRowDraft {
                started_at: current_timestamp(),
                finished_at: None,
                status: "config_error".into(),
                stdout: String::new(),
                stderr: String::new(),
                exit_status: None,
            },
        };
    }

    let started_at = current_timestamp();

    if let Some(path) = resolve_creates(&install.creates, workspace_root) {
        let outcome = (|| {
            let sha256 = sha256_of_file(&path)?;
            verify_expected_sha256(expected_sha256, &sha256)?;
            Ok(InstallerOutcome::AlreadyPresent {
                path: path.clone(),
                sha256,
            })
        })();
        let row = InstallerRowDraft {
            started_at: started_at.clone(),
            finished_at: Some(current_timestamp()),
            status: "skipped".into(),
            stdout: String::new(),
            stderr: String::new(),
            exit_status: Some(0),
        };
        return InstallerResult { outcome, row };
    }

    let run_result = run_shell_install(&install.shell, &agent_env, workspace_root);
    let finished_at = current_timestamp();

    match run_result {
        Ok(captured) => {
            let row_status = if captured.exit_status == Some(0) {
                "ran"
            } else {
                "failed"
            };
            let row = InstallerRowDraft {
                started_at: started_at.clone(),
                finished_at: Some(finished_at),
                status: row_status.into(),
                stdout: captured.stdout.clone(),
                stderr: captured.stderr.clone(),
                exit_status: captured.exit_status,
            };
            if captured.exit_status != Some(0) {
                return InstallerResult {
                    outcome: Err(StackError::AgentInstallerFailed {
                        exit: captured.exit_status,
                        stderr_tail: tail_bytes(&captured.stderr, STDERR_TAIL_BYTES),
                    }),
                    row,
                };
            }
            // Shell exited 0: verify postcheck and sha256, then build the
            // final outcome. Row is already captured.
            let outcome = (|| {
                let resolved =
                    resolve_creates(&install.creates, workspace_root).ok_or_else(|| {
                        StackError::AgentInstallerCreatesMissing {
                            name: install.creates.clone(),
                        }
                    })?;
                let sha256 = sha256_of_file(&resolved)?;
                verify_expected_sha256(expected_sha256, &sha256)?;
                Ok(InstallerOutcome::Installed {
                    path: resolved,
                    sha256,
                })
            })();
            InstallerResult { outcome, row }
        }
        Err(StackError::AgentInstallerTimeout) => InstallerResult {
            outcome: Err(StackError::AgentInstallerTimeout),
            row: InstallerRowDraft {
                started_at,
                finished_at: Some(finished_at),
                status: "timeout".into(),
                stdout: String::new(),
                stderr: "[installer timed out]".into(),
                exit_status: None,
            },
        },
        Err(err) => InstallerResult {
            outcome: Err(err),
            row: InstallerRowDraft {
                started_at,
                finished_at: Some(finished_at),
                status: "error".into(),
                stdout: String::new(),
                stderr: String::new(),
                exit_status: None,
            },
        },
    }
}

fn verify_expected_sha256(expected: Option<&str>, actual: &str) -> Result<()> {
    match expected {
        Some(expected) if expected != actual => Err(StackError::AgentSha256Mismatch {
            expected: expected.to_owned(),
            actual: actual.to_owned(),
        }),
        _ => Ok(()),
    }
}

struct CapturedOutput {
    stdout: String,
    stderr: String,
    exit_status: Option<i32>,
}

fn run_shell_install(
    shell: &str,
    agent_env: &HashMap<String, String>,
    workspace_root: &Path,
) -> Result<CapturedOutput> {
    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg(shell)
        .current_dir(workspace_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear();

    // Minimal env so the installer is no wider a door than the agent itself.
    // PATH is required so `creates` lookups resolve. HOME lets installers
    // place dotfiles in the operator's home if they need to. LANG keeps
    // tools that read locale from producing mojibake.
    forward_host_env(&mut command, "PATH");
    forward_host_env(&mut command, "HOME");
    forward_host_env(&mut command, "LANG");
    // Inject `[agent].env` values, but refuse to let them override
    // PATH/HOME/LANG. The same security argument applies as in the bridge:
    // the daemon's environment is the source of truth for where to find
    // binaries and the operator's home, not values reachable through the
    // secret store.
    for (name, value) in agent_env {
        if matches!(name.as_str(), "PATH" | "HOME" | "LANG") {
            tracing::warn!(
                name = %name,
                "refusing to inject `{name}` from `[agent].env` into installer: reserved",
            );
            continue;
        }
        command.env(name, value);
    }

    // Detach into a fresh process group so the timeout-induced SIGKILL also
    // reaches whatever grandchildren the shell forks.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    let mut child = command
        .spawn()
        .map_err(|source| StackError::AgentSpawnFailed { source })?;

    // Read stdout/stderr concurrently in dedicated threads with a hard cap
    // per stream. Without dedicated drainers, a chatty installer can fill
    // the pipe buffer and wedge the shell once it tries to write more.
    let stdout_handle = child.stdout.take().map(spawn_capped_reader);
    let stderr_handle = child.stderr.take().map(spawn_capped_reader);

    let deadline = Instant::now() + INSTALLER_TIMEOUT;
    let exit = wait_with_timeout(&mut child, deadline);

    match exit {
        Ok(Some(status)) => {
            // Kill any grandchildren that inherited stdout/stderr from the
            // shell. Without this, a `(slow-process &)` in the installer
            // leaves the pipes open and the reader threads block forever
            // on EOF — exactly the bypass the spec hardening guards against.
            kill_process_group(&mut child);
            let stdout = stdout_handle.map(join_reader_bounded).unwrap_or_default();
            let stderr = stderr_handle.map(join_reader_bounded).unwrap_or_default();
            Ok(CapturedOutput {
                stdout,
                stderr,
                exit_status: status.code(),
            })
        }
        Ok(None) => {
            // Timeout: kill the whole process group, then drain readers.
            kill_process_group(&mut child);
            // Best-effort: ensure the kernel reaps the child.
            let _ = child.wait();
            // Threads will exit once the pipes close from the kill.
            let _ = stdout_handle.map(join_reader_bounded);
            let _ = stderr_handle.map(join_reader_bounded);
            Err(StackError::AgentInstallerTimeout)
        }
        Err(source) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(StackError::AgentSpawnFailed { source })
        }
    }
}

/// Reader-thread cleanup window. After we've killed the process group, the
/// pipes should close almost immediately; this just bounds the worst case
/// so we never wedge `POST /v1/agent/install` on a stuck reader thread.
const READER_JOIN_GRACE: Duration = Duration::from_secs(2);

fn join_reader_bounded(handle: std::thread::JoinHandle<String>) -> String {
    // Poll for the join to complete. Threads can't be cancelled, but after
    // kill_process_group the pipes are closed so the reader returns
    // immediately in practice. The poll guards against a kernel-level edge
    // case where the close is delayed.
    let deadline = Instant::now() + READER_JOIN_GRACE;
    while !handle.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    if handle.is_finished() {
        handle.join().unwrap_or_default()
    } else {
        // Abandon the thread. The OS will reap it when the process exits.
        String::new()
    }
}

fn spawn_capped_reader<R>(mut reader: R) -> std::thread::JoinHandle<String>
where
    R: Read + Send + 'static,
{
    std::thread::spawn(move || {
        let mut buf = Vec::with_capacity(8 * 1024);
        let mut chunk = [0u8; 4 * 1024];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    if buf.len() >= MAX_INSTALLER_STREAM_BYTES {
                        // Drain the rest to keep the pipe from blocking the
                        // child, but discard. Without this the shell hangs on
                        // a chatty installer once the OS pipe buffer fills.
                        continue;
                    }
                    let remaining = MAX_INSTALLER_STREAM_BYTES - buf.len();
                    let take = n.min(remaining);
                    buf.extend_from_slice(&chunk[..take]);
                }
                Err(_) => break,
            }
        }
        // Lossy decode: an installer that writes non-UTF-8 bytes still gets a
        // readable row in installer_runs; we'd rather show replacement chars
        // than reject the run.
        String::from_utf8_lossy(&buf).into_owned()
    })
}

fn wait_with_timeout(
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

#[cfg(unix)]
fn kill_process_group(child: &mut std::process::Child) {
    // SAFETY: libc::kill is async-signal-safe and we're operating on a pid
    // we own (the process group leader is the child itself because we used
    // process_group(0)). Negative pid addresses the whole process group, so
    // grandchildren forked by the shell also receive the signal.
    unsafe {
        let pid = child.id() as i32;
        libc::kill(-pid, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_process_group(child: &mut std::process::Child) {
    let _ = child.kill();
}

fn forward_host_env(command: &mut Command, name: &str) {
    if let Some(value) = std::env::var_os(name) {
        command.env(name, value);
    }
}

/// Resolve `[agent.install].creates` to a real path. Matches the documented
/// behavior in `docs/specs/runtime.md`: absolute paths used as-is; paths
/// containing `/` resolved relative to `workspace_root` so an installer can
/// declare `creates = "bin/agent"` without depending on operator cwd; bare
/// names looked up on `PATH`.
fn resolve_creates(name: &str, workspace_root: &Path) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }
    let as_path = Path::new(name);
    if as_path.is_absolute() {
        return if as_path.is_file() {
            Some(as_path.to_path_buf())
        } else {
            None
        };
    }
    if name.contains('/') {
        let candidate = workspace_root.join(name);
        return if candidate.is_file() {
            Some(candidate)
        } else {
            None
        };
    }
    let path_env = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path_env) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn sha256_of_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).map_err(|source| StackError::AgentSpawnFailed { source })?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

fn current_timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn tail_bytes(input: &str, max_bytes: usize) -> String {
    if input.len() <= max_bytes {
        return input.to_owned();
    }
    let start = input.len() - max_bytes;
    let mut cutoff = start;
    while cutoff < input.len() && !input.is_char_boundary(cutoff) {
        cutoff += 1;
    }
    input[cutoff..].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::StateStore;
    use tempfile::TempDir;

    fn open_store() -> (TempDir, StateStore) {
        let tempdir = TempDir::new().expect("tempdir");
        let path = tempdir.path().join("state.sqlite");
        let store = StateStore::open(&path).expect("open");
        store.migrate().expect("migrate");
        (tempdir, store)
    }

    fn install_config(shell: &str, creates: &str) -> AgentInstallConfig {
        AgentInstallConfig {
            install_type: "shell".into(),
            shell: shell.into(),
            creates: creates.into(),
        }
    }

    fn workspace_root() -> PathBuf {
        std::env::temp_dir()
    }

    #[test]
    fn precheck_short_circuits_when_creates_resolves() {
        // `true` ships on every POSIX system; the installer should skip.
        let (_tempdir, store) = open_store();
        let install = install_config("false", "true");
        let outcome =
            run_installer(&install, None, HashMap::new(), &workspace_root(), &store).expect("ok");
        assert_eq!(outcome.label(), "already_present");
        let runs = store.query_installer_runs(10).expect("query");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, "skipped");
    }

    #[test]
    fn missing_creates_after_run_returns_creates_missing() {
        let (_tempdir, store) = open_store();
        // A successful shell that does NOT actually produce the named binary.
        let install = install_config("true", "definitely-not-a-real-binary-xyz123");
        let err = run_installer(&install, None, HashMap::new(), &workspace_root(), &store)
            .expect_err("must fail");
        assert!(matches!(
            err,
            StackError::AgentInstallerCreatesMissing { .. }
        ));
        let runs = store.query_installer_runs(10).expect("query");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, "ran");
    }

    #[test]
    fn nonzero_exit_returns_installer_failed() {
        let (_tempdir, store) = open_store();
        let install = install_config("false", "definitely-not-a-real-binary-xyz123");
        let err = run_installer(&install, None, HashMap::new(), &workspace_root(), &store)
            .expect_err("must fail");
        assert!(matches!(
            err,
            StackError::AgentInstallerFailed { exit: Some(1), .. }
        ));
        let runs = store.query_installer_runs(10).expect("query");
        assert_eq!(runs[0].status, "failed");
        assert_eq!(runs[0].exit_status, Some(1));
    }

    #[test]
    fn sha256_mismatch_returns_typed_error() {
        let (_tempdir, store) = open_store();
        let install = install_config("false", "true");
        let bogus = "0".repeat(64);
        let err = run_installer(
            &install,
            Some(&bogus),
            HashMap::new(),
            &workspace_root(),
            &store,
        )
        .expect_err("must fail");
        assert!(matches!(err, StackError::AgentSha256Mismatch { .. }));
    }

    #[test]
    fn output_truncation_keeps_rows_bounded() {
        let (_tempdir, store) = open_store();
        // Emit ~200 KiB to stdout via printf inside the shell; the cap should
        // hold the resulting row well below twice the cap. `head -c` is
        // POSIX-portable enough for our test environments.
        let shell = format!(
            "head -c {} /dev/urandom | base64 | head -c {}",
            MAX_INSTALLER_STREAM_BYTES * 4,
            MAX_INSTALLER_STREAM_BYTES * 4
        );
        // Use a creates path that won't exist so we go through the "ran" path
        // and capture stdout. We don't care that this returns an error after
        // running; we only check the truncation guarantee on what was stored.
        let install = install_config(&shell, "definitely-not-a-real-binary-xyz123");
        let _ = run_installer(&install, None, HashMap::new(), &workspace_root(), &store);
        let runs = store.query_installer_runs(10).expect("query");
        assert!(
            runs[0].stdout.len() <= MAX_INSTALLER_STREAM_BYTES + 128,
            "stdout grew to {} bytes",
            runs[0].stdout.len()
        );
    }
}
