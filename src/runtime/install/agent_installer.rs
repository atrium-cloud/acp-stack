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

/// Run the resolved-registry installer WITHOUT touching the state store.
/// Returns all rows that should be persisted (in order) and the final outcome.
pub fn install_resolved_capture(
    agent: &AgentConfig,
    entry: &RegistryEntry,
    _agent_env: HashMap<String, String>,
    workspace_root: &Path,
    dest_dir: &Path,
) -> InstallerSequenceResult {
    let mut rows = Vec::new();
    let installer_env = HashMap::new();
    if let Err(err) = entry.ensure_supported() {
        return InstallerSequenceResult {
            outcome: Err(err),
            rows,
        };
    }

    // Step 1: install the upstream agent harness. Native entries speak ACP from
    // this binary; adapter-backed entries wrap it with an adapter in step 2.
    let harness = match entry.harness.as_ref() {
        Some(h) => h,
        None => {
            // The registry validator should have caught this; fail-fast with a
            // typed error if it didn't.
            return InstallerSequenceResult {
                outcome: Err(StackError::RegistryLoad {
                    reason: format!("registry entry `{}` has no harness block", entry.id),
                }),
                rows,
            };
        }
    };
    let harness_step_label = if entry.kind == RegistryKind::Adapter {
        STEP_HARNESS
    } else {
        STEP_INSTALL
    };
    if entry.kind == RegistryKind::Adapter {
        let adapter = match entry.adapter.as_ref() {
            Some(adapter) => adapter,
            None => {
                return InstallerSequenceResult {
                    outcome: Err(StackError::RegistryLoad {
                        reason: format!("registry entry `{}` has no adapter block", entry.id),
                    }),
                    rows,
                };
            }
        };

        // Harness + adapter install in parallel. Each side tries its
        // priority chain (shell → npm → github_release for floating,
        // github → npm for pinned) internally so a single broken path
        // doesn't abort the install when a sibling would have worked.
        let harness_workspace = workspace_root.to_path_buf();
        let harness_dest = dest_dir.to_path_buf();
        let harness_env = installer_env.clone();
        let harness_install = harness.install.clone();
        let harness_github = entry.github.clone();
        let harness_version = agent.harness_version.clone();
        let harness_id = entry.id.clone();
        let adapter_workspace = workspace_root.to_path_buf();
        let adapter_dest = dest_dir.to_path_buf();
        let adapter_env = installer_env.clone();
        let adapter_install = adapter.install.clone();
        let adapter_github = adapter.github.clone();
        let adapter_id = entry.id.clone();
        let harness_thread = std::thread::spawn(move || {
            install_one_with_fallback(
                &harness_id,
                "harness.install",
                STEP_HARNESS,
                &harness_install,
                harness_github.as_deref(),
                harness_version.as_deref(),
                &harness_env,
                &harness_workspace,
                &harness_dest,
            )
        });
        let adapter_thread = std::thread::spawn(move || {
            install_one_with_fallback(
                &adapter_id,
                "adapter.install",
                STEP_ADAPTER,
                &adapter_install,
                adapter_github.as_deref(),
                None,
                &adapter_env,
                &adapter_workspace,
                &adapter_dest,
            )
        });
        let harness_chain = harness_thread.join().unwrap_or_else(|_| FallbackChain {
            rows: vec![InstallerRowDraft::config_error(STEP_HARNESS)],
            terminal_error: Some(StackError::AgentInitializeFailed {
                reason: "harness installer thread panicked".to_owned(),
            }),
        });
        let adapter_chain = adapter_thread.join().unwrap_or_else(|_| FallbackChain {
            rows: vec![InstallerRowDraft::config_error(STEP_ADAPTER)],
            terminal_error: Some(StackError::AgentInitializeFailed {
                reason: "adapter installer thread panicked".to_owned(),
            }),
        });
        rows.extend(harness_chain.rows);
        rows.extend(adapter_chain.rows);
        if let Some(err) = harness_chain.terminal_error {
            return InstallerSequenceResult {
                outcome: Err(err),
                rows,
            };
        }
        if let Some(err) = adapter_chain.terminal_error {
            return InstallerSequenceResult {
                outcome: Err(err),
                rows,
            };
        }

        return final_verification(agent, workspace_root, dest_dir, rows);
    }

    let chain = install_one_with_fallback(
        &entry.id,
        "harness.install",
        harness_step_label,
        &harness.install,
        entry.github.as_deref(),
        agent.harness_version.as_deref(),
        &installer_env,
        workspace_root,
        dest_dir,
    );
    rows.extend(chain.rows);
    if let Some(err) = chain.terminal_error {
        return InstallerSequenceResult {
            outcome: Err(err),
            rows,
        };
    }

    final_verification(agent, workspace_root, dest_dir, rows)
}

/// Result of walking the `[shell, npm, github]` chain for one install
/// field. `rows` contains the per-attempt `installer_runs` draft (so
/// every attempt is preserved for audit, not just the winner);
/// `terminal_error` is `None` when any path succeeded, otherwise the
/// LAST path's error.
pub(crate) struct FallbackChain {
    pub(crate) rows: Vec<InstallerRowDraft>,
    pub(crate) terminal_error: Option<StackError>,
}

/// Try each install path declared on the given field in priority order
/// (shell → npm → github_release for floating versions; github → npm for
/// pinned). Returns once one succeeds, or once all declared paths have
/// been exhausted. Each attempt is recorded so the operator can see the
/// fallback chain after the fact via `acps installer history`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn install_one_with_fallback(
    agent_id: &str,
    field: &str,
    step_label: &'static str,
    install: &InstallSet,
    github_url: Option<&str>,
    version_pin: Option<&str>,
    env: &HashMap<String, String>,
    workspace_root: &Path,
    dest_dir: &Path,
) -> FallbackChain {
    let mut remaining = install.clone();
    let mut rows = Vec::new();
    let mut last_error: Option<StackError> = None;
    let mut missing_tools = BTreeSet::new();
    loop {
        let spec = match step_runners::select_install_path(
            agent_id,
            field,
            &remaining,
            github_url,
            version_pin,
        ) {
            Ok(spec) => spec,
            Err(err) => {
                if rows.is_empty() {
                    // No path was ever runnable. Surface that as the
                    // single registry error with a placeholder row so
                    // the audit log records the attempt.
                    rows.push(InstallerRowDraft::config_error(step_label));
                    return FallbackChain {
                        rows,
                        terminal_error: Some(err),
                    };
                }
                return FallbackChain {
                    rows,
                    terminal_error: last_error.or(Some(err)),
                };
            }
        };
        let kind = path_kind_of(&spec);
        let missing_for_path = missing_required_tools(&spec, workspace_root, dest_dir);
        if !missing_for_path.is_empty() {
            for tool in missing_for_path {
                missing_tools.insert(tool);
            }
            match kind {
                InstallPathKind::Shell => remaining.shell = None,
                InstallPathKind::Npm => remaining.npm = None,
                InstallPathKind::Github => remaining.github = None,
            }
            if remaining.shell.is_none() && remaining.npm.is_none() && remaining.github.is_none() {
                return exhausted_after_missing_prerequisites(
                    agent_id,
                    field,
                    step_label,
                    rows,
                    last_error,
                    missing_tools,
                );
            }
            continue;
        }
        let step = run_install_step(step_label, spec, env, workspace_root, dest_dir);
        let ok = step.outcome.is_ok();
        rows.push(step.row);
        match step.outcome {
            Ok(_) => {
                return FallbackChain {
                    rows,
                    terminal_error: None,
                };
            }
            Err(err) => {
                last_error = Some(err);
                // Drop the path we just exhausted so the next select
                // resolves a different one.
                match kind {
                    InstallPathKind::Shell => remaining.shell = None,
                    InstallPathKind::Npm => remaining.npm = None,
                    InstallPathKind::Github => remaining.github = None,
                }
            }
        }
        let _ = ok;
        if remaining.shell.is_none() && remaining.npm.is_none() && remaining.github.is_none() {
            return FallbackChain {
                rows,
                terminal_error: last_error,
            };
        }
    }
}

fn exhausted_after_missing_prerequisites(
    agent_id: &str,
    field: &str,
    step_label: &'static str,
    mut rows: Vec<InstallerRowDraft>,
    last_error: Option<StackError>,
    missing_tools: BTreeSet<String>,
) -> FallbackChain {
    if !rows.is_empty() {
        return FallbackChain {
            rows,
            terminal_error: last_error,
        };
    }
    rows.push(InstallerRowDraft::config_error(step_label));
    FallbackChain {
        rows,
        terminal_error: Some(StackError::AgentInstallerPrerequisitesMissing {
            agent_id: agent_id.to_owned(),
            step: field.to_owned(),
            tools: missing_tools.into_iter().collect(),
        }),
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) enum InstallPathKind {
    Shell,
    Npm,
    Github,
}

fn path_kind_of(spec: &ResolvedInstallSpec) -> InstallPathKind {
    match spec {
        ResolvedInstallSpec::Shell { .. } => InstallPathKind::Shell,
        ResolvedInstallSpec::Npm { .. } => InstallPathKind::Npm,
        ResolvedInstallSpec::GithubRelease { .. } => InstallPathKind::Github,
    }
}

fn missing_required_tools(
    spec: &ResolvedInstallSpec,
    workspace_root: &Path,
    dest_dir: &Path,
) -> Vec<String> {
    let required_tools: Vec<&str> = match spec {
        ResolvedInstallSpec::Shell { required_tools, .. } => {
            required_tools.iter().map(String::as_str).collect()
        }
        ResolvedInstallSpec::Npm { .. } => vec!["npm"],
        ResolvedInstallSpec::GithubRelease { .. } => Vec::new(),
    };
    required_tools
        .into_iter()
        .filter(|tool| resolve_creates(tool, workspace_root, &[dest_dir]).is_none())
        .map(str::to_owned)
        .collect()
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
mod tests {
    use super::step_runners::select_install_path;
    use super::*;
    use crate::runtime::install::agent_registry::{
        AdapterSpec, ArchiveKind, HarnessSpec, ShellInstall,
    };
    use crate::state::StateStore;
    use std::os::unix::fs::PermissionsExt;
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
            creates: creates.into(),
            shell: Some(shell.into()),
        }
    }

    fn workspace_root() -> PathBuf {
        std::env::temp_dir()
    }

    #[test]
    fn init_resume_creates_resolver_checks_local_bin_and_workspace_relative_paths() {
        let tempdir = TempDir::new().expect("tempdir");
        let workspace_root = tempdir.path().join("workspace");
        let local_bin = tempdir.path().join(".local/bin");
        std::fs::create_dir_all(workspace_root.join("bin")).expect("workspace bin");
        std::fs::create_dir_all(&local_bin).expect("local bin");
        let workspace_agent = workspace_root.join("bin/agent");
        let local_agent = local_bin.join("managed-agent");
        std::fs::write(&workspace_agent, b"#!/bin/sh\n").expect("workspace agent");
        std::fs::write(&local_agent, b"#!/bin/sh\n").expect("local agent");

        assert_eq!(
            resolve_creates_for_init_resume("bin/agent", &workspace_root, &[&local_bin]),
            Some(workspace_agent),
        );
        assert_eq!(
            resolve_creates_for_init_resume("managed-agent", &workspace_root, &[&local_bin]),
            Some(local_agent),
        );
        assert_eq!(
            resolve_creates_for_init_resume("managed-agent", &workspace_root, &[]),
            None,
            "custom [agent.install] verifier must not search managed local bin unless it is on PATH",
        );
    }

    fn agent_config(command: &str) -> AgentConfig {
        AgentConfig {
            id: "test-agent".to_owned(),
            name: "Test Agent".to_owned(),
            command: command.to_owned(),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            expected_sha256: None,
            restart: "on-crash".to_owned(),
            mode: None,
            model: None,
            harness_version: None,
            adapter: None,
            provider: None,
            subagent: None,
            auto_update: None,
            install: None,
        }
    }

    fn shell_install_set(script: &str, creates: &str) -> InstallSet {
        InstallSet {
            shell: Some(ShellInstall {
                script: script.to_owned(),
                creates: creates.to_owned(),
                required_tools: Vec::new(),
            }),
            ..InstallSet::default()
        }
    }

    fn harness_spec(id: &str, install: InstallSet) -> HarnessSpec {
        HarnessSpec {
            id: id.to_owned(),
            install,
            update: Default::default(),
        }
    }

    fn adapter_spec(id: &str, install: InstallSet) -> AdapterSpec {
        AdapterSpec {
            id: id.to_owned(),
            sync_id: None,
            github: None,
            install,
            update: Default::default(),
        }
    }

    fn native_entry(
        id: &str,
        name: &str,
        support_doc: Option<&str>,
        harness: HarnessSpec,
    ) -> RegistryEntry {
        RegistryEntry {
            id: id.to_owned(),
            name: name.to_owned(),
            kind: RegistryKind::Native,
            headless_compatible: support_doc.is_some(),
            set_provider: false,
            set_model: false,
            set_mode: false,
            supports_mcp: false,
            supports_agent_skills: false,
            agent_skills_install_dir: None,
            subagents: false,
            subagent_alias: None,
            subagent_free_models: Vec::new(),
            allow_custom_provider: false,
            allow_custom_model: false,
            stdio_framing: Default::default(),
            website: None,
            github: None,
            support_doc: support_doc.map(str::to_owned),
            testflight_prompt: None,
            testflight_expect_fs: None,
            adapter: None,
            harness: Some(harness),
        }
    }

    fn adapter_entry(
        id: &str,
        name: &str,
        support_doc: Option<&str>,
        harness: HarnessSpec,
        adapter: AdapterSpec,
    ) -> RegistryEntry {
        RegistryEntry {
            id: id.to_owned(),
            name: name.to_owned(),
            kind: RegistryKind::Adapter,
            headless_compatible: support_doc.is_some(),
            set_provider: false,
            set_model: false,
            set_mode: false,
            supports_mcp: false,
            supports_agent_skills: false,
            agent_skills_install_dir: None,
            subagents: false,
            subagent_alias: None,
            subagent_free_models: Vec::new(),
            allow_custom_provider: false,
            allow_custom_model: false,
            stdio_framing: Default::default(),
            website: None,
            github: None,
            support_doc: support_doc.map(str::to_owned),
            testflight_prompt: None,
            testflight_expect_fs: None,
            adapter: Some(adapter),
            harness: Some(harness),
        }
    }

    fn shell_string_for_write(path: &Path, content: &str) -> String {
        format!(
            "mkdir -p {bin} && printf {content} > {binary} && chmod 755 {binary}",
            bin = shell_quote_path(path.parent().expect("binary has parent")),
            content = shell_quote_literal(content),
            binary = shell_quote_path(path),
        )
    }

    fn shell_quote_literal(text: &str) -> String {
        format!("'{}'", text.replace('\'', "'\\''"))
    }

    fn write_fake_npm(dest_dir: &Path, body: &str) {
        let npm_path = dest_dir.join("npm");
        std::fs::write(&npm_path, format!("#!/bin/sh\n{body}")).expect("write fake npm");
        let permissions = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(&npm_path, permissions).expect("chmod fake npm");
    }

    #[test]
    fn select_install_path_captures_pinned_npm_version() {
        let install = InstallSet {
            npm: Some(crate::runtime::install::agent_registry::NpmInstall {
                package: "@scope/agent".to_owned(),
                creates: "agent".to_owned(),
            }),
            ..InstallSet::default()
        };
        let resolved =
            select_install_path("test", "harness.install", &install, None, Some("1.2.3"))
                .expect("resolve");
        match resolved {
            ResolvedInstallSpec::Npm {
                package,
                version,
                creates,
            } => {
                assert_eq!(package, "@scope/agent@1.2.3");
                assert_eq!(version.as_deref(), Some("1.2.3"));
                assert_eq!(creates, "agent");
            }
            other => panic!("expected Npm variant, got {other:?}"),
        }
    }

    #[test]
    fn select_install_path_unpinned_npm_has_no_version() {
        let install = InstallSet {
            npm: Some(crate::runtime::install::agent_registry::NpmInstall {
                package: "@scope/agent".to_owned(),
                creates: "agent".to_owned(),
            }),
            ..InstallSet::default()
        };
        let resolved =
            select_install_path("test", "harness.install", &install, None, None).expect("resolve");
        match resolved {
            ResolvedInstallSpec::Npm {
                package,
                version,
                creates,
            } => {
                assert_eq!(package, "@scope/agent");
                assert!(version.is_none());
                assert_eq!(creates, "agent");
            }
            other => panic!("expected Npm variant, got {other:?}"),
        }
    }

    #[test]
    fn unpinned_npm_install_records_resolved_version() {
        let tempdir = TempDir::new().expect("tempdir");
        let dest_dir = tempdir.path().join("bin");
        std::fs::create_dir(&dest_dir).expect("create bin dir");
        write_fake_npm(
            &dest_dir,
            r#"
set -eu
if [ "$1" = "view" ]; then
  test "$2" = "@scope/agent"
  test "$3" = "version"
  test "$4" = "--json"
  printf '"1.2.3"\n'
  exit 0
fi
if [ "$1" = "install" ]; then
  test "$2" = "-g"
  test "$3" = "--prefix"
  test "$5" = "@scope/agent@1.2.3"
  mkdir -p "$4/bin"
  printf agent > "$4/bin/agent"
  chmod 755 "$4/bin/agent"
  exit 0
fi
exit 99
"#,
        );
        let install = InstallSet {
            npm: Some(crate::runtime::install::agent_registry::NpmInstall {
                package: "@scope/agent".to_owned(),
                creates: "agent".to_owned(),
            }),
            ..InstallSet::default()
        };
        let entry = native_entry(
            "npm-agent",
            "Npm Agent",
            Some("docs/agents/npm-agent.md"),
            harness_spec("agent", install),
        );

        let result = install_resolved_capture(
            &agent_config("agent"),
            &entry,
            HashMap::new(),
            tempdir.path(),
            &dest_dir,
        );

        result.outcome.expect("npm install should pass");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].status, "ran");
        assert_eq!(result.rows[0].version.as_deref(), Some("1.2.3"));
    }

    #[test]
    fn npm_version_lookup_failure_fails_step() {
        let tempdir = TempDir::new().expect("tempdir");
        let dest_dir = tempdir.path().join("bin");
        std::fs::create_dir(&dest_dir).expect("create bin dir");
        write_fake_npm(
            &dest_dir,
            r#"
set -eu
if [ "$1" = "view" ]; then
  printf 'registry down\n' >&2
  exit 7
fi
exit 99
"#,
        );
        let install = InstallSet {
            npm: Some(crate::runtime::install::agent_registry::NpmInstall {
                package: "@scope/agent".to_owned(),
                creates: "agent".to_owned(),
            }),
            ..InstallSet::default()
        };
        let entry = native_entry(
            "npm-agent",
            "Npm Agent",
            Some("docs/agents/npm-agent.md"),
            harness_spec("agent", install),
        );

        let result = install_resolved_capture(
            &agent_config("agent"),
            &entry,
            HashMap::new(),
            tempdir.path(),
            &dest_dir,
        );

        assert!(matches!(
            result.outcome.expect_err("npm view failure should fail"),
            StackError::AgentInstallerFailed { exit: Some(7), .. }
        ));
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].status, "failed");
        assert_eq!(result.rows[0].exit_status, Some(7));
        assert!(result.rows[0].version.is_none());
    }

    #[test]
    fn npm_version_lookup_invalid_json_fails_step() {
        let tempdir = TempDir::new().expect("tempdir");
        let dest_dir = tempdir.path().join("bin");
        std::fs::create_dir(&dest_dir).expect("create bin dir");
        write_fake_npm(
            &dest_dir,
            r#"
set -eu
if [ "$1" = "view" ]; then
  printf 'not-json\n'
  exit 0
fi
exit 99
"#,
        );
        let install = InstallSet {
            npm: Some(crate::runtime::install::agent_registry::NpmInstall {
                package: "@scope/agent".to_owned(),
                creates: "agent".to_owned(),
            }),
            ..InstallSet::default()
        };
        let entry = native_entry(
            "npm-agent",
            "Npm Agent",
            Some("docs/agents/npm-agent.md"),
            harness_spec("agent", install),
        );

        let result = install_resolved_capture(
            &agent_config("agent"),
            &entry,
            HashMap::new(),
            tempdir.path(),
            &dest_dir,
        );

        assert!(matches!(
            result
                .outcome
                .expect_err("invalid npm view JSON should fail"),
            StackError::AgentInitializeFailed { .. }
        ));
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].status, "failed");
        assert!(result.rows[0].stderr.contains("invalid JSON string"));
        assert!(result.rows[0].version.is_none());
    }

    #[test]
    fn install_records_every_fallback_attempt_when_first_path_fails() {
        // The first declared path is a shell recipe that exits 1
        // without producing `creates`. The second is an npm package that
        // npm/npx can't actually fetch in the test sandbox. The
        // important guarantee being asserted: BOTH attempts get recorded
        // as `installer_runs` rows (not just the first one). This proves
        // `install_one_with_fallback` walked past the failed shell path
        // and tried npm, rather than the pre-audit behavior of bailing
        // on the first failure.
        let tempdir = TempDir::new().expect("tempdir");
        write_fake_npm(
            tempdir.path(),
            r#"
set -eu
if [ "$1" = "view" ]; then
  printf '"1.2.3"\n'
  exit 0
fi
exit 9
"#,
        );

        let install = InstallSet {
            shell: Some(ShellInstall {
                script: "exit 1".to_owned(),
                creates: "fallback-agent".to_owned(),
                required_tools: Vec::new(),
            }),
            npm: Some(crate::runtime::install::agent_registry::NpmInstall {
                package: "@acp-stack/definitely-not-published".to_owned(),
                creates: "fallback-agent".to_owned(),
            }),
            ..InstallSet::default()
        };
        let entry = native_entry(
            "fallback-agent",
            "Fallback Agent",
            Some("docs/agents/fallback-agent.md"),
            harness_spec("fallback-agent", install),
        );

        let result = install_resolved_capture(
            &agent_config("fallback-agent"),
            &entry,
            HashMap::new(),
            tempdir.path(),
            tempdir.path(),
        );

        // The chain exhausted both paths, so the overall outcome is Err.
        // But the rows must include BOTH attempts — proof that the
        // fallback walk actually happened.
        assert!(
            result.outcome.is_err(),
            "every declared path is unreachable; expected Err",
        );
        assert!(
            result.rows.len() >= 2,
            "expected fallback chain to record both attempts, got {:?}",
            result
                .rows
                .iter()
                .map(|r| (r.status.as_str(), r.exit_status))
                .collect::<Vec<_>>(),
        );
        // Both rows must record the failure outcome — proves the runner
        // didn't skip the second attempt after the first failed.
        for (i, row) in result.rows.iter().enumerate() {
            assert_eq!(
                row.status, "failed",
                "attempt #{i} should be `failed`, got `{}`",
                row.status,
            );
        }
    }

    #[test]
    fn shell_install_records_no_version() {
        let tempdir = TempDir::new().expect("tempdir");
        let binary_path = tempdir.path().join("shell-agent");
        let script = shell_string_for_write(&binary_path, "agent");
        let entry = native_entry(
            "shell-agent",
            "Shell Agent",
            Some("docs/agents/shell-agent.md"),
            harness_spec("shell-agent", shell_install_set(&script, "shell-agent")),
        );
        let result = install_resolved_capture(
            &agent_config("shell-agent"),
            &entry,
            HashMap::new(),
            tempdir.path(),
            tempdir.path(),
        );
        result.outcome.expect("install ok");
        assert_eq!(result.rows.len(), 1);
        assert!(
            result.rows[0].version.is_none(),
            "shell installs must leave version unset; got {:?}",
            result.rows[0].version
        );
    }

    #[test]
    fn missing_shell_required_tool_fails_when_no_fallback_is_runnable() {
        let tempdir = TempDir::new().expect("tempdir");
        let install = InstallSet {
            shell: Some(ShellInstall {
                script: "missing-tool-command".to_owned(),
                creates: "agent".to_owned(),
                required_tools: vec!["definitely-missing-acp-stack-tool".to_owned()],
            }),
            ..InstallSet::default()
        };

        let chain = install_one_with_fallback(
            "preflight-agent",
            "harness.install",
            STEP_INSTALL,
            &install,
            None,
            None,
            &HashMap::new(),
            tempdir.path(),
            tempdir.path(),
        );

        match chain.terminal_error.expect("missing prerequisite") {
            StackError::AgentInstallerPrerequisitesMissing {
                agent_id,
                step,
                tools,
            } => {
                assert_eq!(agent_id, "preflight-agent");
                assert_eq!(step, "harness.install");
                assert_eq!(tools, vec!["definitely-missing-acp-stack-tool"]);
            }
            other => panic!("expected prerequisite error, got {other:?}"),
        }
    }

    #[test]
    fn missing_shell_required_tool_falls_back_to_runnable_npm_path() {
        let tempdir = TempDir::new().expect("tempdir");
        write_fake_npm(
            tempdir.path(),
            r#"
set -eu
if [ "$1" = "view" ]; then
  printf '"1.2.3"\n'
  exit 0
fi
exit 9
"#,
        );
        let install = InstallSet {
            shell: Some(ShellInstall {
                script: "missing-tool-command".to_owned(),
                creates: "agent".to_owned(),
                required_tools: vec!["definitely-missing-acp-stack-tool".to_owned()],
            }),
            npm: Some(crate::runtime::install::agent_registry::NpmInstall {
                package: "@scope/agent".to_owned(),
                creates: "agent".to_owned(),
            }),
            ..InstallSet::default()
        };

        let chain = install_one_with_fallback(
            "preflight-agent",
            "harness.install",
            STEP_INSTALL,
            &install,
            None,
            None,
            &HashMap::new(),
            tempdir.path(),
            tempdir.path(),
        );

        assert!(
            matches!(
                chain.terminal_error,
                Some(StackError::AgentInstallerFailed { exit: Some(9), .. })
            ),
            "npm fallback should run and fail as the terminal error, got {:?}",
            chain.terminal_error
        );
        assert_eq!(chain.rows.len(), 1);
    }

    #[test]
    fn missing_fallback_prerequisite_does_not_mask_runnable_path_failure() {
        let chain = exhausted_after_missing_prerequisites(
            "preflight-agent",
            "harness.install",
            STEP_INSTALL,
            vec![InstallerRowDraft::config_error(STEP_INSTALL)],
            Some(StackError::AgentInstallerFailed {
                exit: Some(7),
                stderr_tail: "failed".to_owned(),
            }),
            BTreeSet::from(["npm".to_owned()]),
        );
        assert!(
            matches!(
                chain.terminal_error,
                Some(StackError::AgentInstallerFailed { exit: Some(7), .. })
            ),
            "shell failure should remain terminal when npm is unavailable, got {:?}",
            chain.terminal_error
        );
        assert_eq!(chain.rows.len(), 1);
    }

    #[test]
    fn github_release_install_path_has_no_host_tool_prerequisites() {
        let spec = ResolvedInstallSpec::GithubRelease {
            repo: "owner/repo".to_owned(),
            asset_pattern: "agent-linux-x86_64.tar.gz".to_owned(),
            archive: ArchiveKind::TarGz,
            archive_binary_name: None,
            binary_name: "agent".to_owned(),
            checksums_asset: None,
            version_pin: None,
        };
        let tempdir = TempDir::new().expect("tempdir");
        assert!(missing_required_tools(&spec, tempdir.path(), tempdir.path()).is_empty());
    }

    #[test]
    fn persist_step_logs_writes_files_and_sets_log_dir() {
        let tempdir = TempDir::new().expect("tempdir");
        let mut row = InstallerRowDraft {
            started_at: "2026-05-22T00:00:00.123456789Z".to_owned(),
            finished_at: Some("2026-05-22T00:00:01.000000000Z".to_owned()),
            status: "ran".into(),
            stdout: "hello stdout\n".into(),
            stderr: "hello stderr\n".into(),
            exit_status: Some(0),
            step: "harness".into(),
            method: Some(INSTALL_METHOD_GITHUB.to_owned()),
            version: Some("v1.0.0".into()),
            log_dir: None,
        };
        persist_step_logs_to_disk(&mut row, "test-agent", Some(tempdir.path()))
            .expect("logs should persist");
        let log_dir = row.log_dir.as_deref().expect("log_dir set on success");
        let stdout_path = std::path::Path::new(log_dir).join("stdout");
        let stderr_path = std::path::Path::new(log_dir).join("stderr");
        let stdout_body = std::fs::read_to_string(&stdout_path).expect("stdout written");
        let stderr_body = std::fs::read_to_string(&stderr_path).expect("stderr written");
        assert_eq!(stdout_body, "hello stdout\n");
        assert_eq!(stderr_body, "hello stderr\n");
    }

    #[test]
    fn persist_step_logs_skips_when_streams_empty() {
        let tempdir = TempDir::new().expect("tempdir");
        let mut row = InstallerRowDraft {
            started_at: "2026-05-22T00:00:00.000000000Z".to_owned(),
            finished_at: Some("2026-05-22T00:00:00.000000000Z".to_owned()),
            status: "skipped".into(),
            stdout: String::new(),
            stderr: String::new(),
            exit_status: Some(0),
            step: "install".into(),
            method: Some(INSTALL_METHOD_SHELL.to_owned()),
            version: None,
            log_dir: None,
        };
        persist_step_logs_to_disk(&mut row, "test-agent", Some(tempdir.path()))
            .expect("empty streams should be a no-op");
        assert!(
            row.log_dir.is_none(),
            "log_dir must stay None when both streams are empty"
        );
    }

    #[test]
    fn persist_step_logs_is_a_no_op_when_log_base_is_none() {
        let mut row = InstallerRowDraft {
            started_at: "2026-05-22T00:00:00.000000000Z".to_owned(),
            finished_at: None,
            status: "ran".into(),
            stdout: "anything".into(),
            stderr: String::new(),
            exit_status: Some(0),
            step: "harness".into(),
            method: Some(INSTALL_METHOD_SHELL.to_owned()),
            version: None,
            log_dir: None,
        };
        persist_step_logs_to_disk(&mut row, "test-agent", None)
            .expect("missing log base should be a no-op");
        assert!(row.log_dir.is_none());
    }

    #[test]
    fn installer_log_persist_failure_prevents_history_row() {
        let tempdir = TempDir::new().expect("tempdir");
        let (_state_dir, store) = open_store();
        let log_base_file = tempdir.path().join("not-a-directory");
        std::fs::write(&log_base_file, b"file blocks log dir").expect("write blocker file");
        let install = install_config(
            "printf 'audit stdout\n'; mkdir -p bin; printf agent > bin/test-agent",
            "bin/test-agent",
        );

        let err = run_installer(
            "test-agent",
            &install,
            None,
            HashMap::new(),
            tempdir.path(),
            &store,
            Some(&log_base_file),
        )
        .expect_err("log persistence failure must fail install wrapper");

        assert!(matches!(err, StackError::AgentInstallerLogPersist { .. }));
        let runs = store.query_installer_runs(10).expect("query");
        assert!(
            runs.is_empty(),
            "installer history must not record a row without the audit log"
        );
    }

    #[test]
    fn precheck_short_circuits_when_creates_resolves() {
        // `true` ships on every POSIX system; the installer should skip.
        let (_tempdir, store) = open_store();
        let install = install_config("false", "true");
        let outcome = run_installer(
            "test-agent",
            &install,
            None,
            HashMap::new(),
            &workspace_root(),
            &store,
            None,
        )
        .expect("ok");
        assert_eq!(outcome.label(), "already_present");
        let runs = store.query_installer_runs(10).expect("query");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, "skipped");
        assert_eq!(runs[0].step, "install");
    }

    #[test]
    fn missing_creates_after_run_returns_creates_missing() {
        let (_tempdir, store) = open_store();
        // A successful shell that does NOT actually produce the named binary.
        let install = install_config("true", "definitely-not-a-real-binary-xyz123");
        let err = run_installer(
            "test-agent",
            &install,
            None,
            HashMap::new(),
            &workspace_root(),
            &store,
            None,
        )
        .expect_err("must fail");
        assert!(matches!(
            err,
            StackError::AgentInstallerCreatesMissing { .. }
        ));
        let runs = store.query_installer_runs(10).expect("query");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, "failed");
        assert_eq!(runs[0].step, "install");
    }

    #[test]
    fn nonzero_exit_returns_installer_failed() {
        let (_tempdir, store) = open_store();
        let install = install_config("false", "definitely-not-a-real-binary-xyz123");
        let err = run_installer(
            "test-agent",
            &install,
            None,
            HashMap::new(),
            &workspace_root(),
            &store,
            None,
        )
        .expect_err("must fail");
        assert!(matches!(
            err,
            StackError::AgentInstallerFailed { exit: Some(1), .. }
        ));
        let runs = store.query_installer_runs(10).expect("query");
        assert_eq!(runs[0].status, "failed");
        assert_eq!(runs[0].exit_status, Some(1));
        assert_eq!(runs[0].step, "install");
    }

    #[test]
    fn sha256_mismatch_returns_typed_error() {
        let (_tempdir, store) = open_store();
        let install = install_config("false", "true");
        let bogus = "0".repeat(64);
        let err = run_installer(
            "test-agent",
            &install,
            Some(&bogus),
            HashMap::new(),
            &workspace_root(),
            &store,
            None,
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
        let _ = run_installer(
            "test-agent",
            &install,
            None,
            HashMap::new(),
            &workspace_root(),
            &store,
            None,
        );
        let runs = store.query_installer_runs(10).expect("query");
        assert!(
            runs[0].stdout.len() <= MAX_INSTALLER_STREAM_BYTES + 128,
            "stdout grew to {} bytes",
            runs[0].stdout.len()
        );
    }

    #[test]
    fn unsupported_registry_entry_fails_before_running_steps() {
        let entry = native_entry(
            "unsupported",
            "Unsupported Agent",
            None,
            harness_spec(
                "unsupported",
                shell_install_set("false", "definitely-should-not-run"),
            ),
        );
        let tempdir = TempDir::new().expect("tempdir");
        let result = install_resolved_capture(
            &agent_config("unsupported-agent"),
            &entry,
            HashMap::new(),
            tempdir.path(),
            tempdir.path(),
        );
        assert!(result.rows.is_empty());
        let err = result.outcome.expect_err("must reject unsupported agent");
        assert_eq!(
            err.public_message(),
            "Unsupported Agent is not currently supported. Please try a different agent."
        );
    }

    #[test]
    fn final_verification_searches_managed_bin_dir() {
        let tempdir = TempDir::new().expect("tempdir");
        let dest_dir = tempdir.path().join("bin");
        std::fs::create_dir(&dest_dir).expect("create bin dir");
        let binary_path = dest_dir.join("managed-agent");
        std::fs::write(&binary_path, b"fake-agent").expect("write fake binary");

        let entry = native_entry(
            "managed-agent",
            "Managed Agent",
            Some("docs/agents/managed-agent.md"),
            harness_spec("managed-agent", shell_install_set("true", "managed-agent")),
        );

        let result = install_resolved_capture(
            &agent_config("managed-agent"),
            &entry,
            HashMap::new(),
            tempdir.path(),
            &dest_dir,
        );
        let outcome = result.outcome.expect("managed binary should resolve");
        assert_eq!(outcome.path(), binary_path.as_path());
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].status, "ran");
    }

    #[test]
    fn adapter_entry_installs_harness_then_adapter_and_verifies_adapter_command() {
        let tempdir = TempDir::new().expect("tempdir");
        let dest_dir = tempdir.path().join("bin");
        std::fs::create_dir(&dest_dir).expect("create bin dir");
        let adapter_binary = dest_dir.join("adapter-agent");
        let harness_binary = dest_dir.join("upstream-agent");
        let adapter_script = shell_string_for_write(&adapter_binary, "adapter");
        let harness_script = shell_string_for_write(&harness_binary, "harness");
        let entry = adapter_entry(
            "adapter-agent",
            "Adapter Agent",
            Some("docs/agents/adapter-agent.md"),
            harness_spec(
                "upstream-agent",
                shell_install_set(&harness_script, "upstream-agent"),
            ),
            adapter_spec(
                "adapter-agent",
                shell_install_set(&adapter_script, "adapter-agent"),
            ),
        );

        let result = install_resolved_capture(
            &agent_config("adapter-agent"),
            &entry,
            HashMap::new(),
            tempdir.path(),
            &dest_dir,
        );

        let outcome = result.outcome.expect("adapter should install");
        assert_eq!(outcome.path(), adapter_binary.as_path());
        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.rows[0].step, "harness");
        assert_eq!(result.rows[0].status, "ran");
        assert_eq!(result.rows[1].step, "adapter");
        assert_eq!(result.rows[1].status, "ran");
    }

    #[test]
    fn adapter_entry_runs_harness_and_adapter_install_steps_concurrently() {
        let tempdir = TempDir::new().expect("tempdir");
        let dest_dir = tempdir.path().join("bin");
        std::fs::create_dir(&dest_dir).expect("create bin dir");
        let harness_binary = dest_dir.join("upstream-agent");
        let adapter_binary = dest_dir.join("adapter-agent");
        let harness_script = format!(
            "sleep 0.6; mkdir -p {bin}; printf harness > {harness}; chmod 755 {harness}",
            bin = shell_quote_path(&dest_dir),
            harness = shell_quote_path(&harness_binary),
        );
        let adapter_script = format!(
            "sleep 0.6; mkdir -p {bin}; printf adapter > {adapter}; chmod 755 {adapter}",
            bin = shell_quote_path(&dest_dir),
            adapter = shell_quote_path(&adapter_binary),
        );
        let entry = adapter_entry(
            "adapter-agent",
            "Adapter Agent",
            Some("docs/agents/adapter-agent.md"),
            harness_spec(
                "upstream-agent",
                shell_install_set(&harness_script, "upstream-agent"),
            ),
            adapter_spec(
                "adapter-agent",
                shell_install_set(&adapter_script, "adapter-agent"),
            ),
        );

        let started = std::time::Instant::now();
        let result = install_resolved_capture(
            &agent_config("adapter-agent"),
            &entry,
            HashMap::new(),
            tempdir.path(),
            &dest_dir,
        );
        let elapsed = started.elapsed();

        result.outcome.expect("adapter should install");
        assert!(
            elapsed < std::time::Duration::from_millis(1100),
            "adapter install took {elapsed:?}, expected concurrent steps"
        );
        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.rows[0].step, "harness");
        assert_eq!(result.rows[1].step, "adapter");
    }

    #[test]
    fn adapter_entry_runs_adapter_even_when_harness_fails() {
        let tempdir = TempDir::new().expect("tempdir");
        let dest_dir = tempdir.path().join("bin");
        std::fs::create_dir(&dest_dir).expect("create bin dir");
        let adapter_binary = dest_dir.join("adapter-agent");
        let adapter_script = shell_string_for_write(&adapter_binary, "adapter");
        let entry = adapter_entry(
            "adapter-agent",
            "Adapter Agent",
            Some("docs/agents/adapter-agent.md"),
            harness_spec(
                "upstream-agent",
                shell_install_set("false", "upstream-agent"),
            ),
            adapter_spec(
                "adapter-agent",
                shell_install_set(&adapter_script, "adapter-agent"),
            ),
        );

        let result = install_resolved_capture(
            &agent_config("adapter-agent"),
            &entry,
            HashMap::new(),
            tempdir.path(),
            &dest_dir,
        );

        assert!(matches!(
            result
                .outcome
                .expect_err("harness failure must fail install"),
            StackError::AgentInstallerFailed { .. }
        ));
        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.rows[0].step, "harness");
        assert_eq!(result.rows[0].status, "failed");
        assert_eq!(result.rows[1].step, "adapter");
        assert_eq!(result.rows[1].status, "ran");
        assert!(adapter_binary.is_file());
    }

    #[test]
    fn registry_installs_do_not_receive_agent_runtime_secrets() {
        let tempdir = TempDir::new().expect("tempdir");
        let binary_path = tempdir.path().join("secret-check-agent");
        let script = format!(
            "test -z \"$OPENCODE_API_KEY\" && printf ok > {binary}",
            binary = shell_quote_path(&binary_path),
        );
        let entry = native_entry(
            "secret-check-agent",
            "Secret Check Agent",
            Some("docs/agents/secret-check-agent.md"),
            harness_spec(
                "secret-check-agent",
                shell_install_set(&script, "secret-check-agent"),
            ),
        );
        let mut agent_env = HashMap::new();
        agent_env.insert("OPENCODE_API_KEY".to_owned(), "secret-value".to_owned());

        let result = install_resolved_capture(
            &agent_config("secret-check-agent"),
            &entry,
            agent_env,
            tempdir.path(),
            tempdir.path(),
        );

        let outcome = result
            .outcome
            .expect("registry installer should not see runtime secret");
        assert_eq!(outcome.path(), binary_path.as_path());
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].status, "ran");
    }

    #[test]
    fn bootstrap_can_install_directly_into_managed_bin() {
        let tempdir = TempDir::new().expect("tempdir");
        let dest_dir = tempdir.path().join(".local").join("bin");
        let managed_opencode = dest_dir.join("opencode");
        let script = format!(
            "set -eu\n\
             managed_bin={dest_dir}\n\
             mkdir -p \"$managed_bin\"\n\
             printf bootstrap > \"$managed_bin/opencode\"\n\
             chmod 755 \"$managed_bin/opencode\"\n\
             test -x {managed_opencode}",
            dest_dir = shell_quote_path(&dest_dir),
            managed_opencode = shell_quote_path(&managed_opencode),
        );
        let entry = native_entry(
            "opencode",
            "OpenCode",
            Some("docs/agents/opencode.md"),
            harness_spec("opencode", shell_install_set(&script, "opencode")),
        );

        let result = install_resolved_capture(
            &agent_config("opencode"),
            &entry,
            HashMap::new(),
            tempdir.path(),
            &dest_dir,
        );

        let outcome = result.outcome.expect("managed opencode link should verify");
        assert_eq!(outcome.path(), managed_opencode.as_path());
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].status, "ran");
    }

    fn shell_quote_path(path: &Path) -> String {
        let text = path.to_string_lossy();
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}
