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
use crate::runtime::agent::acp_bridge::resolve_command_path;
use crate::runtime::install::agent_registry::{
    ArchiveKind, InstallSet, RegistryEntry, RegistryKind,
};
use crate::runtime::install::install_ownership::install_components;
use crate::runtime::process_runner::HostExec;
use crate::state::{
    INSTALLER_OPERATION_INSTALL, INSTALLER_OUTPUT_CAP_BYTES, INSTALLER_STATUS_KEPT,
    INSTALLER_STATUS_RUNNING, InstallerRunFinish, InstallerRunInput, StateStore,
};

pub(crate) use self::execute::install_one_with_fallback;
pub use self::execute::install_resolved_capture;
pub use self::step_logs::persist_step_logs_to_disk;
pub(crate) use self::step_runners::npm_version_for_pin;

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

/// What an install run does with the agent CLI (the harness). The adapter always installs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HarnessInstall {
    /// Walk the harness install lanes, honoring a `harness_version` pin.
    Install,
    /// Leave the binary an operator chose to keep at this path, gate it, and record it `kept`.
    Keep(PathBuf),
}

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
    /// Resolved by the installer, or read from `--version` for shell recipes.
    pub version: Option<String>,
    /// Directory holding the full stdout/stderr capture, set by the persisting
    /// wrappers after they write the files.
    pub log_dir: Option<String>,
    /// `installer_runs` id when the progress sink already finalized this step's
    /// row in place; `None` rows still need the end-of-run append.
    pub persisted_run_id: Option<String>,
    /// The binary the step left in place, set on success so a later install can
    /// recognize it as one acp-stack put there.
    pub artifact: Option<InstalledArtifact>,
}

/// A step's resolved binary: the path the command resolver returns for it and
/// the sha256 of the file that path reaches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledArtifact {
    pub path: PathBuf,
    pub sha256: String,
}

impl InstalledArtifact {
    pub(crate) fn of(path: &Path) -> Result<Self> {
        Ok(Self {
            path: path.to_path_buf(),
            sha256: sha256_of_file(path)?,
        })
    }

    fn path_str(artifact: Option<&Self>) -> Option<String> {
        artifact.map(|artifact| artifact.path.display().to_string())
    }
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
            artifact: None,
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
            artifact: None,
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
/// only legal off the async executor. Callers must run steps in
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
            path: None,
            sha256: None,
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
        let artifact_path = InstalledArtifact::path_str(row.artifact.as_ref());
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
                    path: artifact_path.as_deref(),
                    sha256: row
                        .artifact
                        .as_ref()
                        .map(|artifact| artifact.sha256.as_str()),
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
                        path: None,
                        sha256: None,
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
    let artifact_path = InstalledArtifact::path_str(row.artifact.as_ref());
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
        path: artifact_path.as_deref(),
        sha256: row
            .artifact
            .as_ref()
            .map(|artifact| artifact.sha256.as_str()),
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
#[allow(clippy::too_many_arguments)]
pub fn run_installer(
    agent_id: &str,
    agent_command: &str,
    install: &AgentInstallConfig,
    expected_sha256: Option<&str>,
    agent_env: HashMap<String, String>,
    host: &HostExec,
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
        agent_command,
        expected_sha256,
        agent_env,
        host,
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
/// shell run, returning the row draft for the caller to persist. `agent_command`
/// is what the runtime spawns once the recipe has run.
pub fn run_installer_capture(
    install: &AgentInstallConfig,
    agent_command: &str,
    expected_sha256: Option<&str>,
    agent_env: HashMap<String, String>,
    host: &HostExec,
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
    crate::runtime::node_runtime::ensure_before_install(host.home());

    // Integrity first: the spawn gate executes the file, and a binary failing
    // the operator's sha256 pin must never run. A present binary that fails the
    // gate reads as absent so the recipe re-runs and replaces it.
    if let Some(path) = resolve_creates(&install.creates, host, &[]) {
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
            Ok(sha256) => match verify_binary_spawns(&path, host, &[]) {
                Ok(()) => {
                    let mut row = InstallerRowDraft::skipped(STEP_INSTALL, &started_at);
                    let outcome = match verify_workload_reachable(host, &[agent_command]) {
                        Ok(()) => Ok(InstallerOutcome::AlreadyPresent {
                            path: path.clone(),
                            sha256,
                        }),
                        Err(error) => {
                            row.status = "failed".to_owned();
                            row.stderr = step_runners::append_stderr_detail(&row.stderr, &error);
                            Err(error)
                        }
                    };
                    return InstallerResult { outcome, row };
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
    let run_result = run_shell_install(shell, &agent_env, host, &[], DEFAULT_INSTALLER_TIMEOUT);
    let mut result = finalize_shell_step(
        STEP_INSTALL,
        started_at,
        run_result,
        &install.creates,
        expected_sha256,
        agent_command,
        host,
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
#[allow(clippy::too_many_arguments)]
pub fn install_resolved(
    agent: &AgentConfig,
    entry: &RegistryEntry,
    harness: &HarnessInstall,
    agent_env: HashMap<String, String>,
    host: &HostExec,
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
        harness,
        agent_env,
        host,
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
    entry: &RegistryEntry,
    host: &HostExec,
    dest_dir: &Path,
    rows: Vec<InstallerRowDraft>,
) -> InstallerSequenceResult {
    let outcome = (|| {
        let path = resolve_creates(&agent.command, host, &[dest_dir]).ok_or_else(|| {
            StackError::AgentInstallerCreatesMissing {
                name: agent.command.clone(),
            }
        })?;
        let sha256 = sha256_of_file(&path)?;
        verify_expected_sha256(agent.expected_sha256.as_deref(), &sha256)?;
        verify_binary_spawns(&path, host, &[dest_dir])?;
        let components = install_components(agent, entry)?;
        let commands: Vec<&str> = components
            .iter()
            .map(|component| component.command.as_str())
            .collect();
        verify_workload_reachable(host, &commands)?;
        Ok(InstallerOutcome::Installed { path, sha256 })
    })();

    InstallerSequenceResult { outcome, rows }
}

/// Refuse an install whose binaries the workload identity could not run or could swap. Each
/// command resolves the way spawning resolves it, then its exec chain must be readable and
/// executable by the workload with no workload-writable hop. A no-op without an identity.
pub(crate) fn verify_workload_reachable(host: &HostExec, commands: &[&str]) -> Result<()> {
    if host.sandbox().identity.is_none() {
        return Ok(());
    }
    let executor = host.sandbox().executor();
    for command in commands {
        let subject = format!("agent binary `{command}`");
        let path = resolve_command_path(command, host.home()).ok_or_else(|| {
            StackError::WorkloadUnreachable {
                subject: subject.clone(),
                path: PathBuf::from(command),
                reason: "it does not resolve on the managed PATH".to_owned(),
            }
        })?;
        if let Err(error) = crate::workload_fs::check_exec_chain(&executor, &path) {
            return Err(StackError::WorkloadUnreachable {
                subject,
                path: workload_fs_error_path(&error)
                    .unwrap_or(&path)
                    .to_path_buf(),
                reason: error.to_string(),
            });
        }
    }
    Ok(())
}

fn workload_fs_error_path(error: &StackError) -> Option<&Path> {
    match error {
        StackError::WorkloadFsWorkloadUnreadable { path }
        | StackError::WorkloadFsWorkloadWritable { path }
        | StackError::WorkloadFsNotFound { path }
        | StackError::WorkloadFsSymlinkLoop { path, .. }
        | StackError::WorkloadFsInvalidPath { path, .. }
        | StackError::WorkloadFsIo { path, .. } => Some(path),
        _ => None,
    }
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
        bundle_binary_path: Option<String>,
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
    host: &HostExec,
    extra_path_dirs: &[&Path],
    expected_sha256: Option<&str>,
) -> Option<PathBuf> {
    let path = resolve_creates(name, host, extra_path_dirs)?;
    if expected_sha256.is_some() {
        let pinned = sha256_of_file(&path)
            .and_then(|sha256| verify_expected_sha256(expected_sha256, &sha256));
        if let Err(error) = pinned {
            tracing::warn!(%error, "installed agent binary failed the integrity pin; re-running installer");
            return None;
        }
    }
    if let Err(error) = verify_binary_spawns(&path, host, extra_path_dirs) {
        tracing::warn!(%error, "installed agent binary failed the spawn gate; re-running installer");
        return None;
    }
    Some(path)
}

/// Spawn gate for installed binaries. Callers MUST run integrity checks
/// (`expected_sha256`) before this gate: the probe executes the file, so a
/// binary that fails the operator's pin must never reach it.
pub(crate) fn verify_binary_spawns(
    path: &Path,
    host: &HostExec,
    extra_path_dirs: &[&Path],
) -> Result<()> {
    use crate::runtime::process_runner::kill_process_group;
    verify_executable_header(path)?;
    let mut command = version_probe_command(path, host, extra_path_dirs)?;
    command
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
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

/// `<path> --version` under the same narrow environment the install steps use, refused when the
/// workload identity could swap the binary.
fn version_probe_command(
    path: &Path,
    host: &HostExec,
    extra_path_dirs: &[&Path],
) -> Result<std::process::Command> {
    use crate::runtime::process_runner::detach_into_new_session;
    // The probe runs in the host-exec dir, so a relative `path` would resolve
    // differently here than it did in `resolve_creates`.
    let exec_path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    host.verify_executable(&exec_path)?;
    let mut command = host.command(&exec_path, extra_path_dirs);
    command.arg("--version").stdin(std::process::Stdio::null());
    detach_into_new_session(&mut command);
    Ok(command)
}

const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(15);
const VERSION_PROBE_CAP_BYTES: usize = 4 * 1024;

/// `<path> --version`, the only version source for shell recipes; `None` on
/// any failure since the spawn gate already proved the binary runs.
pub(crate) fn probe_binary_version(
    path: &Path,
    host: &HostExec,
    extra_path_dirs: &[&Path],
) -> Option<String> {
    use crate::runtime::process_runner::{
        kill_process_group, spawn_capped_reader, wait_with_timeout,
    };
    let mut command = match version_probe_command(path, host, extra_path_dirs) {
        Ok(command) => command,
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "version probe refused");
            return None;
        }
    };
    command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            tracing::debug!(%error, path = %path.display(), "version probe spawn failed");
            return None;
        }
    };
    let stdout = child.stdout.take()?;
    let reader = spawn_capped_reader(stdout, VERSION_PROBE_CAP_BYTES);
    let deadline = std::time::Instant::now() + VERSION_PROBE_TIMEOUT;
    let exited_cleanly = match wait_with_timeout(&mut child, deadline) {
        Ok(Some(status)) => status.success(),
        Ok(None) | Err(_) => false,
    };
    if !exited_cleanly {
        kill_process_group(&mut child);
        if let Err(error) = child.wait() {
            tracing::debug!(%error, path = %path.display(), "version probe child reap failed");
        }
        return None;
    }
    let output = reader.join().ok()?;
    version_token_from_output(&output)
}

/// First version-like token (`pi-acp 0.1.0` → `0.1.0`), leading `v` stripped.
pub(crate) fn version_token_from_output(output: &str) -> Option<String> {
    output
        .split_whitespace()
        .map(|token| token.strip_prefix('v').unwrap_or(token))
        .find(|token| {
            token.contains('.')
                && token.starts_with(|c: char| c.is_ascii_digit())
                && token
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'))
        })
        .map(str::to_owned)
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
/// `docs/specs/runtime.md`. A bare name searches the managed Node `bin` first,
/// so `node`/`npm` prerequisites resolve to it, then `extra_path_dirs`, then PATH,
/// skipping directories the workload identity can write. A relative path with a
/// separator resolves to nothing.
pub(crate) fn resolve_creates(
    name: &str,
    host: &HostExec,
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
        return None;
    }
    let dirs: Vec<PathBuf> =
        std::iter::once(crate::runtime::node_runtime::managed_bin_dir(host.home()))
            .chain(extra_path_dirs.iter().map(|dir| dir.to_path_buf()))
            .chain(std::env::split_paths(
                &std::env::var_os("PATH").unwrap_or_default(),
            ))
            .collect();
    host.sandbox()
        .without_workload_writable(dirs)
        .into_iter()
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

pub(super) fn sha256_of_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).map_err(|source| StackError::AgentBinaryInspect {
        path: path.to_path_buf(),
        source,
    })?;
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
