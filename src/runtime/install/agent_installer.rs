//! Agent installer: registry-resolved install steps plus the
//! `[agent.install] type = "shell"` operator escape hatch.

mod execute;
mod step_logs;
mod step_runners;

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{SecondsFormat, Utc};
use sha2::{Digest, Sha256};

use crate::config::{AgentConfig, AgentInstallConfig};
use crate::error::{Result, StackError};
use crate::runtime::install::agent_registry::{
    ArchiveKind, InstallSet, RegistryEntry, RegistryKind,
};
use crate::state::{
    INSTALLER_OPERATION_INSTALL, INSTALLER_OUTPUT_CAP_BYTES, INSTALLER_STATUS_RUNNING,
    InstallerRunFinish, InstallerRunInput, StateStore,
};

pub(crate) use self::execute::install_one_with_fallback;
pub use self::execute::install_resolved_capture;
pub use self::step_logs::persist_step_logs_to_disk;

use self::step_runners::{
    DEFAULT_INSTALLER_TIMEOUT, finalize_shell_step, run_install_step, run_shell_install,
};

pub const MAX_INSTALLER_STREAM_BYTES: usize = INSTALLER_OUTPUT_CAP_BYTES;

// Step labels persisted to `installer_runs.step`.
pub(crate) const STEP_INSTALL: &str = "install";
pub(crate) const STEP_HARNESS: &str = "harness";
pub(crate) const STEP_ADAPTER: &str = "adapter";

pub(crate) use crate::state::{
    INSTALLER_METHOD_APT as INSTALL_METHOD_APT, INSTALLER_METHOD_GITHUB as INSTALL_METHOD_GITHUB,
    INSTALLER_METHOD_NATIVE as INSTALL_METHOD_NATIVE, INSTALLER_METHOD_NPM as INSTALL_METHOD_NPM,
    INSTALLER_METHOD_SHELL as INSTALL_METHOD_SHELL,
};

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

/// One persisted row's worth of installer state, owned so the caller can write
/// it without holding the state-store lock across the install work.
#[derive(Debug, Clone)]
pub struct InstallerRowDraft {
    pub started_at: String,
    pub finished_at: Option<String>,
    pub status: String,
    pub stdout: String,
    pub stderr: String,
    pub exit_status: Option<i32>,
    pub step: String,
    pub method: Option<String>,
    /// Resolved version the installer wrote; `None` for shell-recipe installs.
    pub version: Option<String>,
    /// Directory holding the full stdout/stderr capture, set by the persisting
    /// wrappers after they write the files.
    pub log_dir: Option<String>,
    /// `installer_runs` id when the progress sink already finalized this step's
    /// row in place; `None` rows still need the end-of-run append.
    pub persisted_run_id: Option<String>,
}

impl InstallerRowDraft {
    fn skipped(step: &str, started_at: &str) -> Self {
        Self {
            started_at: started_at.to_owned(),
            finished_at: Some(current_timestamp()),
            status: "skipped".into(),
            stdout: String::new(),
            stderr: String::new(),
            exit_status: Some(0),
            step: step.to_owned(),
            method: None,
            version: None,
            log_dir: None,
            persisted_run_id: None,
        }
    }

    fn config_error(step: &str) -> Self {
        Self {
            started_at: current_timestamp(),
            finished_at: None,
            status: "config_error".into(),
            stdout: String::new(),
            stderr: String::new(),
            exit_status: None,
            step: step.to_owned(),
            method: None,
            version: None,
            log_dir: None,
            persisted_run_id: None,
        }
    }
}

// =================================================================
// Step-boundary progress (in-flight visibility in `installer_runs`)
// =================================================================

/// Store access for step-boundary writes. `Sync` because adapter-backed
/// installs share one sink across parallel scoped threads.
pub trait InstallerRunSink: Sync {
    /// Run `f` against a state store; `f` must do one brief write and never
    /// outlive the call.
    fn with_store(&self, f: &mut dyn FnMut(&StateStore) -> Result<()>) -> Result<()>;
}

/// Sink over the daemon's shared store handle. Uses `blocking_lock`, so it is
/// only legal off the async executor — callers must run steps in
/// `spawn_blocking`.
pub struct SharedInstallerSink {
    state: std::sync::Arc<tokio::sync::Mutex<StateStore>>,
}

impl SharedInstallerSink {
    pub fn new(state: std::sync::Arc<tokio::sync::Mutex<StateStore>>) -> Self {
        Self { state }
    }
}

impl InstallerRunSink for SharedInstallerSink {
    fn with_store(&self, f: &mut dyn FnMut(&StateStore) -> Result<()>) -> Result<()> {
        let guard = self.state.blocking_lock();
        f(&guard)
    }
}

/// Sink that opens a short-lived second connection per boundary write, for
/// callers whose `&StateStore` cannot cross the installer's scoped threads
/// (a rusqlite connection is `!Sync`).
pub struct ReconnectingInstallerSink {
    state_path: PathBuf,
}

impl ReconnectingInstallerSink {
    pub fn new(state_path: PathBuf) -> Self {
        Self { state_path }
    }
}

impl InstallerRunSink for ReconnectingInstallerSink {
    fn with_store(&self, f: &mut dyn FnMut(&StateStore) -> Result<()>) -> Result<()> {
        let store = StateStore::open(&self.state_path)?;
        f(&store)
    }
}

/// Sink plus the provenance stamped onto each step-boundary `running` row.
pub struct InstallProgress<'a> {
    pub sink: &'a dyn InstallerRunSink,
    pub agent_id: &'a str,
    pub operation: &'static str,
    pub log_base: Option<&'a Path>,
}

/// Insert the `running` row for a step about to execute; a store failure is
/// warn-logged and the step runs untracked rather than aborting the install.
pub(crate) fn begin_tracked_step(
    progress: &InstallProgress<'_>,
    step_label: &'static str,
    method: Option<&str>,
) -> Option<String> {
    let started_at = current_timestamp();
    let mut inserted_id = None;
    let result = progress.sink.with_store(&mut |store| {
        let run = store.append_installer_run(InstallerRunInput {
            agent_id: progress.agent_id,
            started_at: &started_at,
            finished_at: None,
            status: INSTALLER_STATUS_RUNNING,
            stdout: "",
            stderr: "",
            exit_status: None,
            step: step_label,
            version: None,
            operation: progress.operation,
            method,
            log_dir: None,
            apply_run_id: None,
        })?;
        inserted_id = Some(run.id);
        Ok(())
    });
    match result {
        Ok(()) => inserted_id,
        Err(error) => {
            tracing::warn!(%error, step = step_label, "installer progress: running-row insert failed; step continues untracked");
            None
        }
    }
}

/// Finalize a step's `running` row with the finished draft. Logs are written to
/// disk before the row is updated, so a run without its audit log copy never
/// records success; a failed finalize marks the row `error` rather than leaving
/// it in-flight.
pub(crate) fn finalize_tracked_step(
    progress: &InstallProgress<'_>,
    run_id: Option<String>,
    row: &mut InstallerRowDraft,
) {
    let Some(run_id) = run_id else {
        return;
    };
    let result = (|| -> Result<()> {
        persist_step_logs_to_disk(row, progress.agent_id, progress.log_base)?;
        progress.sink.with_store(&mut |store| {
            store.finish_installer_run(
                &run_id,
                InstallerRunFinish {
                    started_at: &row.started_at,
                    finished_at: row.finished_at.as_deref(),
                    status: &row.status,
                    stdout: &row.stdout,
                    stderr: &row.stderr,
                    exit_status: row.exit_status,
                    version: row.version.as_deref(),
                    log_dir: row.log_dir.as_deref(),
                },
            )
        })
    })();
    match result {
        Ok(()) => row.persisted_run_id = Some(run_id),
        Err(error) => {
            tracing::warn!(%error, run_id, "installer progress: running-row finalize failed; row falls back to end-of-run append");
            let finished_now = current_timestamp();
            let reason = format!("installer progress finalize failed: {error}");
            let mark = progress.sink.with_store(&mut |store| {
                store.finish_installer_run(
                    &run_id,
                    InstallerRunFinish {
                        started_at: &row.started_at,
                        finished_at: Some(&finished_now),
                        status: "error",
                        stdout: &row.stdout,
                        stderr: &reason,
                        exit_status: row.exit_status,
                        version: row.version.as_deref(),
                        log_dir: row.log_dir.as_deref(),
                    },
                )
            });
            if let Err(mark_error) = mark {
                tracing::warn!(error = %mark_error, run_id, "installer progress: failed to mark unfinalizable row as error");
            }
        }
    }
}

/// Persist a row the progress sink did not finalize, writing its log capture to
/// disk first.
pub fn persist_untracked_installer_row(
    state: &StateStore,
    row: &mut InstallerRowDraft,
    agent_id: &str,
    operation: &'static str,
    log_base: Option<&Path>,
) -> Result<()> {
    if row.persisted_run_id.is_some() {
        return Ok(());
    }
    persist_step_logs_to_disk(row, agent_id, log_base)?;
    state.append_installer_run(InstallerRunInput {
        agent_id,
        started_at: &row.started_at,
        finished_at: row.finished_at.as_deref(),
        status: &row.status,
        stdout: &row.stdout,
        stderr: &row.stderr,
        exit_status: row.exit_status,
        step: &row.step,
        version: row.version.as_deref(),
        operation,
        method: row.method.as_deref(),
        log_dir: row.log_dir.as_deref(),
        apply_run_id: None,
    })?;
    Ok(())
}

/// Operator escape-hatch single-step result.
pub struct InstallerResult {
    pub outcome: Result<InstallerOutcome>,
    pub row: InstallerRowDraft,
}

/// Registry-resolved sequence result; rows must be persisted in order.
pub struct InstallerSequenceResult {
    pub outcome: Result<InstallerOutcome>,
    pub rows: Vec<InstallerRowDraft>,
}

// =================================================================
// Operator escape-hatch (`[agent.install] type = "shell"`)
// =================================================================

/// Run the escape-hatch installer and persist its row, publishing progress
/// through a [`ReconnectingInstallerSink`].
pub fn run_installer(
    agent_id: &str,
    install: &AgentInstallConfig,
    expected_sha256: Option<&str>,
    agent_env: HashMap<String, String>,
    workspace_root: &Path,
    state: &StateStore,
    log_base: Option<&Path>,
) -> Result<InstallerOutcome> {
    let sink = ReconnectingInstallerSink::new(state.path().to_path_buf());
    let progress = InstallProgress {
        sink: &sink,
        agent_id,
        operation: INSTALLER_OPERATION_INSTALL,
        log_base,
    };
    let mut result = run_installer_capture(
        install,
        expected_sha256,
        agent_env,
        workspace_root,
        Some(&progress),
    );
    persist_untracked_installer_row(
        state,
        &mut result.row,
        agent_id,
        INSTALLER_OPERATION_INSTALL,
        log_base,
    )?;
    result.outcome
}

/// Run the escape-hatch installer without holding the state store across the
/// shell run, returning the row draft for the caller to persist.
pub fn run_installer_capture(
    install: &AgentInstallConfig,
    expected_sha256: Option<&str>,
    agent_env: HashMap<String, String>,
    workspace_root: &Path,
    progress: Option<&InstallProgress<'_>>,
) -> InstallerResult {
    if install.install_type.as_str() != "shell" {
        return InstallerResult {
            outcome: Err(StackError::AgentNotConfigured),
            row: InstallerRowDraft::config_error(STEP_INSTALL),
        };
    }
    let shell = match install.shell.as_deref() {
        Some(shell) => shell,
        None => {
            return InstallerResult {
                outcome: Err(StackError::AgentNotConfigured),
                row: InstallerRowDraft::config_error(STEP_INSTALL),
            };
        }
    };
    let started_at = current_timestamp();

    // Integrity first: the spawn gate executes the file, and a binary failing
    // the operator's sha256 pin must never run. A present binary that fails the
    // gate reads as absent so the recipe re-runs and replaces it.
    if let Some(path) = resolve_creates(&install.creates, workspace_root, &[]) {
        let integrity = (|| {
            let sha256 = sha256_of_file(&path)?;
            verify_expected_sha256(expected_sha256, &sha256)?;
            Ok(sha256)
        })();
        match integrity {
            Err(err) => {
                return InstallerResult {
                    outcome: Err(err),
                    row: InstallerRowDraft::skipped(STEP_INSTALL, &started_at),
                };
            }
            Ok(sha256) => match verify_binary_spawns(&path, workspace_root, &[]) {
                Ok(()) => {
                    return InstallerResult {
                        outcome: Ok(InstallerOutcome::AlreadyPresent {
                            path: path.clone(),
                            sha256,
                        }),
                        row: InstallerRowDraft::skipped(STEP_INSTALL, &started_at),
                    };
                }
                Err(error) => {
                    tracing::warn!(%error, "existing agent binary failed the spawn gate; re-running installer");
                }
            },
        }
    }

    let run_id = progress.and_then(|progress| {
        begin_tracked_step(progress, STEP_INSTALL, Some(INSTALL_METHOD_SHELL))
    });
    let run_result = run_shell_install(
        shell,
        &agent_env,
        workspace_root,
        &[],
        DEFAULT_INSTALLER_TIMEOUT,
    );
    let mut result = finalize_shell_step(
        STEP_INSTALL,
        started_at,
        run_result,
        &install.creates,
        expected_sha256,
        workspace_root,
    );
    if let Some(progress) = progress {
        finalize_tracked_step(progress, run_id, &mut result.row);
    }
    result
}

// =================================================================
// Registry-resolved path (one step for native, two for adapter-backed)
// =================================================================

/// Run the resolved-registry installer and persist every row; the HTTP path
/// uses [`install_resolved_capture`] with its own sink instead.
pub fn install_resolved(
    agent: &AgentConfig,
    entry: &RegistryEntry,
    agent_env: HashMap<String, String>,
    workspace_root: &Path,
    dest_dir: &Path,
    state: &StateStore,
    log_base: Option<&Path>,
) -> Result<InstallerOutcome> {
    let sink = ReconnectingInstallerSink::new(state.path().to_path_buf());
    let progress = InstallProgress {
        sink: &sink,
        agent_id: &agent.id,
        operation: INSTALLER_OPERATION_INSTALL,
        log_base,
    };
    let mut result = install_resolved_capture(
        agent,
        entry,
        agent_env,
        workspace_root,
        dest_dir,
        Some(&progress),
    );
    for row in result.rows.iter_mut() {
        persist_untracked_installer_row(
            state,
            row,
            &agent.id,
            INSTALLER_OPERATION_INSTALL,
            log_base,
        )?;
    }
    result.outcome
}

pub(super) fn final_verification(
    agent: &AgentConfig,
    workspace_root: &Path,
    dest_dir: &Path,
    rows: Vec<InstallerRowDraft>,
) -> InstallerSequenceResult {
    let outcome = (|| {
        let path =
            resolve_creates(&agent.command, workspace_root, &[dest_dir]).ok_or_else(|| {
                StackError::AgentInstallerCreatesMissing {
                    name: agent.command.clone(),
                }
            })?;
        let sha256 = sha256_of_file(&path)?;
        verify_expected_sha256(agent.expected_sha256.as_deref(), &sha256)?;
        verify_binary_spawns(&path, workspace_root, &[dest_dir])?;
        Ok(InstallerOutcome::Installed { path, sha256 })
    })();

    InstallerSequenceResult { outcome, rows }
}

pub(super) struct StepResult {
    pub(super) row: InstallerRowDraft,
    pub(super) outcome: Result<()>,
}

#[derive(Debug, Clone)]
pub(super) enum ResolvedInstallSpec {
    Shell {
        script: String,
        creates: String,
        required_tools: Vec<String>,
        timeout: Duration,
    },
    Npm {
        package: String,
        /// Name passed to npm's `--allow-scripts`; kept separate because
        /// `package` may carry a resolved `@version` suffix that scoped names
        /// make ambiguous to strip back off.
        name: String,
        creates: String,
        version: Option<String>,
    },
    GithubRelease {
        repo: String,
        asset_pattern: String,
        archive: ArchiveKind,
        archive_binary_name: Option<String>,
        binary_name: String,
        checksums_asset: Option<String>,
        version_pin: Option<String>,
    },
}

/// Verifier used by `acps init --resume` for `agent_install`. The integrity pin
/// is checked before the spawn probe, so a binary failing `expected_sha256` is
/// never executed and simply reads as absent.
pub fn resolve_creates_for_init_resume(
    name: &str,
    workspace_root: &Path,
    extra_path_dirs: &[&Path],
    expected_sha256: Option<&str>,
) -> Option<PathBuf> {
    let path = resolve_creates(name, workspace_root, extra_path_dirs)?;
    if expected_sha256.is_some() {
        let pinned = sha256_of_file(&path)
            .and_then(|sha256| verify_expected_sha256(expected_sha256, &sha256));
        if let Err(error) = pinned {
            tracing::warn!(%error, "installed agent binary failed the integrity pin; re-running installer");
            return None;
        }
    }
    if let Err(error) = verify_binary_spawns(&path, workspace_root, extra_path_dirs) {
        tracing::warn!(%error, "installed agent binary failed the spawn gate; re-running installer");
        return None;
    }
    Some(path)
}

/// Spawn gate for installed binaries. Callers MUST run integrity checks
/// (`expected_sha256`) before this gate — the probe executes the file, so a
/// binary that fails the operator's pin must never reach it.
pub(crate) fn verify_binary_spawns(
    path: &Path,
    workspace_root: &Path,
    extra_path_dirs: &[&Path],
) -> Result<()> {
    use crate::runtime::process_runner::{
        apply_non_interactive_env, detach_into_new_session, forward_host_env, kill_process_group,
        path_env_with_extra_dirs,
    };
    verify_executable_header(path)?;
    // The probe changes cwd to `workspace_root`, so a relative `path` would
    // resolve differently here than it did in `resolve_creates`.
    let exec_path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut command = std::process::Command::new(&exec_path);
    command
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .env_clear();
    if workspace_root.is_dir() {
        command.current_dir(workspace_root);
    }
    if let Some(path_env) = path_env_with_extra_dirs(extra_path_dirs) {
        command.env("PATH", path_env);
    }
    forward_host_env(&mut command, "HOME");
    forward_host_env(&mut command, "LANG");
    apply_non_interactive_env(&mut command);
    detach_into_new_session(&mut command);
    match command.spawn() {
        Ok(mut child) => {
            kill_process_group(&mut child);
            if let Err(error) = child.wait() {
                tracing::debug!(%error, path = %path.display(), "spawn-gate child reap failed");
            }
            Ok(())
        }
        Err(source) => Err(StackError::AgentInstallerBinaryUnrunnable {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Executable formats the runtime can spawn: shebang scripts, ELF, and Mach-O
/// including fat binaries.
const EXECUTABLE_MAGICS: &[&[u8]] = &[
    b"#!",
    b"\x7fELF",
    &[0xfe, 0xed, 0xfa, 0xce],
    &[0xfe, 0xed, 0xfa, 0xcf],
    &[0xce, 0xfa, 0xed, 0xfe],
    &[0xcf, 0xfa, 0xed, 0xfe],
    &[0xca, 0xfe, 0xba, 0xbe],
    &[0xbe, 0xba, 0xfe, 0xca],
    &[0xca, 0xfe, 0xba, 0xbf],
    &[0xbf, 0xba, 0xfe, 0xca],
];

pub(crate) fn verify_executable_header(path: &Path) -> Result<()> {
    use std::io::Read;
    // An unreadable file (exec-only mode, transient IO error) is not evidence of
    // a bad format, so defer to the spawn probe rather than hard-failing.
    let mut header = [0u8; 4];
    let mut read_total = 0;
    match std::fs::File::open(path) {
        Ok(mut file) => {
            while read_total < header.len() {
                match file.read(&mut header[read_total..]) {
                    Ok(0) => break,
                    Ok(n) => read_total += n,
                    Err(error) => {
                        tracing::debug!(%error, path = %path.display(), "skipping executable header check: read failed");
                        return Ok(());
                    }
                }
            }
        }
        Err(error) => {
            tracing::debug!(%error, path = %path.display(), "skipping executable header check: open failed");
            return Ok(());
        }
    }
    let header = &header[..read_total];
    if EXECUTABLE_MAGICS
        .iter()
        .any(|magic| header.starts_with(magic))
    {
        return Ok(());
    }
    Err(StackError::AgentInstallerBinaryUnrunnable {
        path: path.to_path_buf(),
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "no executable header (ELF, Mach-O, or `#!` interpreter line)",
        ),
    })
}

/// Resolve `[agent.install].creates` to a real path, per the lookup order in
/// `docs/specs/runtime.md`.
pub(crate) fn resolve_creates(
    name: &str,
    workspace_root: &Path,
    extra_path_dirs: &[&Path],
) -> Option<PathBuf> {
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
    for dir in extra_path_dirs {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
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

pub(super) fn sha256_of_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).map_err(|source| StackError::AgentSpawnFailed { source })?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

pub(crate) fn current_timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
}

pub(super) fn verify_expected_sha256(expected: Option<&str>, actual: &str) -> Result<()> {
    match expected {
        Some(expected) if expected != actual => Err(StackError::AgentSha256Mismatch {
            expected: expected.to_owned(),
            actual: actual.to_owned(),
        }),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests;
