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
use crate::state::{INSTALLER_OUTPUT_CAP_BYTES, InstallerRunInput, StateStore};

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
        }
    }
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
pub fn run_installer(
    agent_id: &str,
    install: &AgentInstallConfig,
    expected_sha256: Option<&str>,
    agent_env: HashMap<String, String>,
    workspace_root: &Path,
    state: &StateStore,
    log_base: Option<&Path>,
) -> Result<InstallerOutcome> {
    let mut result = run_installer_capture(install, expected_sha256, agent_env, workspace_root);
    persist_step_logs_to_disk(&mut result.row, agent_id, log_base)?;
    state.append_installer_run(InstallerRunInput {
        agent_id,
        started_at: &result.row.started_at,
        finished_at: result.row.finished_at.as_deref(),
        status: &result.row.status,
        stdout: &result.row.stdout,
        stderr: &result.row.stderr,
        exit_status: result.row.exit_status,
        step: &result.row.step,
        version: result.row.version.as_deref(),
        operation: crate::state::INSTALLER_OPERATION_INSTALL,
        method: result.row.method.as_deref(),
        log_dir: result.row.log_dir.as_deref(),
        apply_run_id: None,
    })?;
    result.outcome
}

/// Run the escape-hatch installer WITHOUT touching the state store. Returns
/// the outcome alongside the row draft the caller should persist.
pub fn run_installer_capture(
    install: &AgentInstallConfig,
    expected_sha256: Option<&str>,
    agent_env: HashMap<String, String>,
    workspace_root: &Path,
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

    if let Some(path) = resolve_creates(&install.creates, workspace_root, &[]) {
        let outcome = (|| {
            let sha256 = sha256_of_file(&path)?;
            verify_expected_sha256(expected_sha256, &sha256)?;
            Ok(InstallerOutcome::AlreadyPresent {
                path: path.clone(),
                sha256,
            })
        })();
        return InstallerResult {
            outcome,
            row: InstallerRowDraft::skipped(STEP_INSTALL, &started_at),
        };
    }

    let run_result = run_shell_install(shell, &agent_env, workspace_root, &[]);
    finalize_shell_step(
        STEP_INSTALL,
        started_at,
        run_result,
        &install.creates,
        expected_sha256,
        workspace_root,
    )
}

// =================================================================
// Registry-resolved path (one step for native, two for adapter-backed)
// =================================================================

/// Run the resolved-registry installer and persist every row under a brief
/// state-store lock per step. Used by the CLI which already holds the state
/// store. The HTTP path uses [`install_resolved_capture`] so it can drop the
/// state lock during each shell/HTTP step.
pub fn install_resolved(
    agent: &AgentConfig,
    entry: &RegistryEntry,
    agent_env: HashMap<String, String>,
    workspace_root: &Path,
    dest_dir: &Path,
    state: &StateStore,
    log_base: Option<&Path>,
) -> Result<InstallerOutcome> {
    let mut result = install_resolved_capture(agent, entry, agent_env, workspace_root, dest_dir);
    for row in result.rows.iter_mut() {
        persist_step_logs_to_disk(row, &agent.id, log_base)?;
    }
    for row in &result.rows {
        state.append_installer_run(InstallerRunInput {
            agent_id: &agent.id,
            started_at: &row.started_at,
            finished_at: row.finished_at.as_deref(),
            status: &row.status,
            stdout: &row.stdout,
            stderr: &row.stderr,
            exit_status: row.exit_status,
            step: &row.step,
            version: row.version.as_deref(),
            operation: crate::state::INSTALLER_OPERATION_INSTALL,
            method: row.method.as_deref(),
            log_dir: row.log_dir.as_deref(),
            apply_run_id: None,
        })?;
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
/// bin directory before falling back to the process PATH.
pub fn resolve_creates_for_init_resume(
    name: &str,
    workspace_root: &Path,
    extra_path_dirs: &[&Path],
) -> Option<PathBuf> {
    resolve_creates(name, workspace_root, extra_path_dirs)
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
