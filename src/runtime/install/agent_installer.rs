//! Agent installer.
//!
//! Two install paths share this module:
//!
//! - **Registry-resolved** (the default): the operator declares `[agent].id`
//!   matching an entry in the embedded `data/agents.toml`. Native entries
//!   produce one install step; adapter-backed entries produce two (harness
//!   first, adapter second).
//! - **Operator escape hatch**: `[agent.install] type = "shell"` runs a free-
//!   form shell recipe with a `creates` precheck/postcheck. Intended for
//!   private forks and unreleased agents that aren't in the curated catalog.
//!
//! Hardening (see `docs/specs/security.md`) applies to every shell-based step
//! (shell escape hatch, npx, uvx):
//!
//! - Timeout (`INSTALLER_TIMEOUT`) so a runaway script cannot wedge the
//!   install RPC indefinitely.
//! - Per-stream output cap (`MAX_INSTALLER_STREAM_BYTES`) so a chatty
//!   installer cannot bloat `installer_runs`. The state repo also
//!   re-truncates at INSERT time as defense-in-depth.
//! - Scrubbed environment: registry-resolved installer steps receive only
//!   `PATH`, `HOME`, and `LANG`. The operator escape-hatch installer also
//!   receives the explicitly resolved env passed to it by the caller.
//! - Fresh process group so the timeout-induced SIGKILL reaches grandchildren
//!   the shell forked.
//!
//! The `github_release` driver does not spawn a shell; it downloads,
//! optionally checksum-verifies, and extracts in-process. The HTTP timeout in
//! `github_release` and the in-process extraction APIs bound its worst case.
//!
//! `creates` is resolved against `PATH` using `std::env::split_paths`, which
//! mirrors the `which` semantics required by `docs/specs/runtime.md` without
//! a dependency on the `which` crate.

mod execute;
mod step_logs;
mod step_runners;

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

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

use self::step_runners::{finalize_shell_step, run_install_step, run_shell_install};

pub const MAX_INSTALLER_STREAM_BYTES: usize = INSTALLER_OUTPUT_CAP_BYTES;

// Step labels persisted to `installer_runs.step`. Centralized here so the
// state-side filter that the future operator UI will use stays consistent
// with what the installer writes.
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

/// One persisted row's worth of installer state. Owned so the HTTP path can
/// drop the state-store lock during the shell/HTTP work and write the row
/// briefly afterward.
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
    /// Resolved version the installer wrote. Populated for github_release
    /// (release tag) and npm installs. Shell-recipe installs leave this `None`;
    /// `acps agent check` then reports `unknown, manual check required`.
    pub version: Option<String>,
    /// On-disk directory the surrounding wrappers populated with the full
    /// stdout/stderr capture. The `*_capture` functions leave this `None`;
    /// the persisting wrappers (`run_installer`, `install_resolved`, and
    /// the HTTP route equivalents) set it after they write the files.
    pub log_dir: Option<String>,
    /// `installer_runs` id when the progress sink already persisted this step
    /// (its `running` row was finalized in place). Persisting wrappers skip
    /// re-appending such rows; `None` rows still need the end-of-run append.
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
//
// The `*_capture` functions hold no store handle, so the HTTP path never
// holds the state lock across a (potentially 10-minute) shell/HTTP step.
// Callers that want live progress instead pass an [`InstallProgress`] sink:
// the execution layer inserts a `running` row as each step starts and
// finalizes that same row when the step finishes, locking the store only
// for the duration of one statement.

/// Store access for step-boundary writes. `Sync` because adapter-backed
/// installs run harness and adapter steps on parallel scoped threads that
/// share one sink.
pub trait InstallerRunSink: Sync {
    /// Run `f` against a state store. Implementations serialize the call
    /// however their store handle requires; `f` must do one brief write and
    /// never outlive the call.
    fn with_store(&self, f: &mut dyn FnMut(&StateStore) -> Result<()>) -> Result<()>;
}

/// Sink over the daemon's shared store handle. Uses `blocking_lock`, so it is
/// only legal off the async executor — every install path that uses it runs
/// its steps inside `spawn_blocking` (same pattern as the deps-apply route).
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
/// (a rusqlite connection is `!Sync`). WAL plus the store busy-timeout make
/// the brief insert/update safe alongside the primary connection — the agent
/// updater already runs on a second connection for the same reason.
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

/// Everything the execution layer needs to publish step-boundary progress:
/// the sink plus the provenance stamped onto the `running` row.
pub struct InstallProgress<'a> {
    pub sink: &'a dyn InstallerRunSink,
    pub agent_id: &'a str,
    pub operation: &'static str,
    pub log_base: Option<&'a Path>,
}

/// Insert the `running` row for a step that is about to execute; returns the
/// row id to finalize with. A store failure is warn-logged and the step runs
/// untracked — progress visibility must never abort the install itself.
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

/// Finalize a step's `running` row with the finished draft. The full log
/// capture is written to disk first so the same update can record `log_dir`.
/// On success the draft is stamped with `persisted_run_id` so the caller's
/// end-of-run persistence skips it; on failure (warn-logged) the draft stays
/// unstamped so the caller's legacy append still records the audit row. A
/// failed finalize additionally marks the row `error` (best-effort): the step
/// is over, so it must not keep reading as in-flight — and an `error` row
/// makes no completion claim, preserving the rule that a run without its
/// audit log copy never records success.
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

/// Persist a row the progress sink did not finalize (skipped/config_error
/// placeholders, or steps whose sink writes failed). The full log capture
/// goes to disk first; the state row is the index into it.
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

/// Operator escape-hatch single-step result. Returned by
/// [`run_installer_capture`] so the caller can persist the row under a brief
/// state-store lock instead of holding it for the entire installer run.
pub struct InstallerResult {
    pub outcome: Result<InstallerOutcome>,
    pub row: InstallerRowDraft,
}

/// Registry-resolved sequence result. May carry 1 row (native or escape hatch)
/// or 2 rows (adapter-backed). The caller persists rows in order before
/// reporting the outcome.
pub struct InstallerSequenceResult {
    pub outcome: Result<InstallerOutcome>,
    pub rows: Vec<InstallerRowDraft>,
}

// =================================================================
// Operator escape-hatch (`[agent.install] type = "shell"`)
// =================================================================

/// Convenience wrapper used by call sites that already hold the state store
/// briefly: runs the escape-hatch installer and persists the row. When
/// `log_base` is `Some`, the wrapper writes the full stdout/stderr capture
/// to a per-step subdirectory and records the path on the row; pass
/// `state::default_installer_log_base(&home)` to land logs under the
/// canonical `~/.local/share/acp-stack/installer-logs/` tree.
///
/// Progress is published through a [`ReconnectingInstallerSink`] (the
/// borrowed store cannot cross the installer's scoped threads): the step's
/// `running` row lands when the shell starts and is finalized in place when
/// it exits, so a concurrent reader sees the in-flight step.
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

/// Run the escape-hatch installer WITHOUT holding the state store across the
/// shell run. Returns the outcome alongside the row draft the caller should
/// persist. When `progress` is provided, the executed step's row is inserted
/// as `running` at start and finalized in place at finish; the returned
/// draft's `persisted_run_id` then tells the caller the row is already stored.
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

    // A present binary that fails the spawn gate is treated as absent so the
    // recipe re-runs and can replace it, instead of skipping past a file
    // nothing can execute. Integrity first: the gate executes the file, and a
    // binary failing the operator's sha256 pin must never run.
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
    let run_result = run_shell_install(shell, &agent_env, workspace_root, &[]);
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

/// Run the resolved-registry installer and persist every row. Steps publish
/// `running` rows at their boundaries through a [`ReconnectingInstallerSink`]
/// (the borrowed store cannot cross the scoped harness/adapter threads);
/// rows the sink did not finalize are appended at the end as before. Used by
/// the CLI which already holds the state store. The HTTP path uses
/// [`install_resolved_capture`] with its own sink so it can drop the state
/// lock during each shell/HTTP step.
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
    // The operator's declared `[agent].command` must now resolve on PATH (or in
    // workspace, per `resolve_creates` semantics). Hash the resulting binary so
    // the existing `expected_sha256` integrity gate still runs.
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
    },
    Npm {
        package: String,
        /// Package name as declared in the install spec, passed to npm's
        /// `--allow-scripts`. Captured separately because `package` may carry
        /// a resolved `@version` suffix and scoped names make stripping it
        /// back off ambiguous. May itself carry a version when the spec
        /// declares one; npm's `allowScripts` accepts pinned `pkg@version`
        /// entries, so either form matches.
        name: String,
        creates: String,
        /// Pinned version when the registry/`acps init` resolved one.
        /// Unpinned npm installs resolve their version with `npm view` before
        /// running `npm install`.
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

/// Public verifier used by `acps init --resume` for `agent_install`.
/// It intentionally delegates to the same resolver used by the installer:
/// absolute paths are checked directly, slash-containing paths are resolved
/// under `workspace_root`, and bare names are checked in the managed local
/// bin directory before falling back to the process PATH. A resolved binary
/// that fails the spawn gate reads as absent so the resumed run re-installs
/// instead of skipping past a file nothing can execute.
///
/// Integrity keeps its usual priority over the probe: when
/// `expected_sha256` is declared, a binary that fails the pin (or cannot be
/// hashed) also reads as absent — never executed — and the resumed install's
/// final verification is where a still-mismatching pin surfaces as an error.
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

/// Spawn gate for installed binaries: `creates` resolving to an executable
/// file is not proof the file can run. A package manager that blocks
/// postinstall scripts leaves a shebang-less stub behind, and a wrong-arch
/// download is a valid file too; both otherwise surface only as ENOEXEC at
/// the first real agent spawn.
///
/// Two layers, both cheap: a header check that rejects files with no
/// executable format at all (the deployment target is Linux, whose exec has
/// no shell fallback for shebang-less text — macOS dev hosts do fall back,
/// so a spawn probe alone would pass the stub there), then a spawn probe
/// that catches host-specific failures like a wrong-arch ELF or a missing
/// interpreter. Only spawnability is judged — the child is killed
/// immediately, so a binary that ignores `--version` or hangs still passes.
///
/// Callers must run integrity checks (`expected_sha256`) BEFORE this gate:
/// the probe executes the file, and a binary that fails the operator's pin
/// must never run. Step-level gates therefore run only
/// [`verify_executable_header`] (which never executes the file) when a pin is
/// declared, and `final_verification` owns the pin check followed by this
/// probe. The probe gets the same scrubbed environment as installer
/// steps (no provider keys, non-interactive hints) and a PATH built from
/// `extra_path_dirs` so a `#!/usr/bin/env`-style interpreter in the managed
/// bin dir resolves the way the real agent spawn would resolve it.
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
    // `resolve_creates` judges with cwd-relative `is_file()` semantics, but
    // the probe changes cwd to `workspace_root`, so a relative `path` would
    // resolve differently between the check and the spawn.
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
            // The probe proved spawnability; the group kill reaches whatever
            // the binary already forked, and the wait reaps the leader.
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

/// Executable formats the runtime can actually spawn: shebang scripts, ELF
/// (Linux deployment), and Mach-O incl. fat binaries (macOS dev hosts).
const EXECUTABLE_MAGICS: &[&[u8]] = &[
    b"#!",
    b"\x7fELF",
    &[0xfe, 0xed, 0xfa, 0xce],
    &[0xfe, 0xed, 0xfa, 0xcf],
    &[0xce, 0xfa, 0xed, 0xfe],
    &[0xcf, 0xfa, 0xed, 0xfe],
    &[0xca, 0xfe, 0xba, 0xbe],
    &[0xbe, 0xba, 0xfe, 0xca],
    // 64-bit fat (universal) Mach-O variants of the two magics above.
    &[0xca, 0xfe, 0xba, 0xbf],
    &[0xbf, 0xba, 0xfe, 0xca],
];

pub(crate) fn verify_executable_header(path: &Path) -> Result<()> {
    use std::io::Read;
    // An unreadable file (exec-only mode, transient IO error) is not evidence
    // of a bad format — let the spawn probe be the judge instead of hard-
    // failing a binary the kernel might exec fine without read access here.
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

/// Resolve `[agent.install].creates` to a real path. Matches the documented
/// behavior in `docs/specs/runtime.md`: absolute paths used as-is; paths
/// containing `/` resolved relative to `workspace_root` so an installer can
/// declare `creates = "bin/agent"` without depending on operator cwd; bare
/// names looked up in caller-provided extra directories and then `PATH`.
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
