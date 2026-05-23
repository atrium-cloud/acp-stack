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

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use chrono::{SecondsFormat, Utc};
use sha2::{Digest, Sha256};

use crate::config::{AgentConfig, AgentInstallConfig};
use crate::error::{Result, StackError};
use crate::runtime::agent_registry::{
    ArchiveKind, GithubInstall, InstallSet, RegistryEntry, RegistryKind, github_repo_from_url,
};
use crate::runtime::github_release::{self, GithubReleaseInstall};
use crate::state::{INSTALLER_OUTPUT_CAP_BYTES, InstallerRunInput, StateStore};

const INSTALLER_TIMEOUT: Duration = Duration::from_secs(10 * 60);
pub const MAX_INSTALLER_STREAM_BYTES: usize = INSTALLER_OUTPUT_CAP_BYTES;
const STDERR_TAIL_BYTES: usize = 2 * 1024;

// Step labels persisted to `installer_runs.step`. Centralized here so the
// state-side filter that the future operator UI will use stays consistent
// with what the installer writes.
pub(crate) const STEP_INSTALL: &str = "install";
pub(crate) const STEP_HARNESS: &str = "harness";
pub(crate) const STEP_ADAPTER: &str = "adapter";

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
        log_dir: result.row.log_dir.as_deref(),
    })?;
    result.outcome
}

/// Write the unbounded stdout/stderr for a single installer step to a
/// per-step directory under `log_base/<agent_id>/<sanitized started_at>/<step>/`
/// and stamp the path onto the row. Skipped step rows have empty streams,
/// so we don't bother creating a directory in that case. Persistence is
/// fail-fast because the full logs are the audit copy; the caller should not
/// append a history row claiming a completed run when that copy was lost.
pub fn persist_step_logs_to_disk(
    row: &mut InstallerRowDraft,
    agent_id: &str,
    log_base: Option<&Path>,
) -> Result<()> {
    let Some(base) = log_base else {
        return Ok(());
    };
    if row.stdout.is_empty() && row.stderr.is_empty() {
        return Ok(());
    }
    let sanitized_started = sanitize_for_path(&row.started_at);
    let log_dir = base
        .join(sanitize_for_path(agent_id))
        .join(sanitized_started)
        .join(sanitize_for_path(&row.step));
    create_dir_tree_synced(&log_dir)?;
    if !row.stdout.is_empty() {
        write_synced_log_file(&log_dir.join("stdout"), row.stdout.as_bytes())?;
    }
    if !row.stderr.is_empty() {
        write_synced_log_file(&log_dir.join("stderr"), row.stderr.as_bytes())?;
    }
    sync_directory(&log_dir)?;
    row.log_dir = Some(log_dir.to_string_lossy().into_owned());
    Ok(())
}

fn create_dir_tree_synced(path: &Path) -> Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        if current.as_os_str().is_empty() || current == Path::new("/") {
            continue;
        }
        match std::fs::metadata(&current) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                return Err(StackError::AgentInstallerLogPersist {
                    path: current,
                    source: std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        "path exists and is not a directory",
                    ),
                });
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&current).map_err(|source| {
                    StackError::AgentInstallerLogPersist {
                        path: current.clone(),
                        source,
                    }
                })?;
                sync_parent_directory(&current)?;
            }
            Err(source) => {
                return Err(StackError::AgentInstallerLogPersist {
                    path: current,
                    source,
                });
            }
        }
    }
    Ok(())
}

fn write_synced_log_file(path: &Path, body: &[u8]) -> Result<()> {
    let mut file =
        std::fs::File::create(path).map_err(|source| StackError::AgentInstallerLogPersist {
            path: path.to_path_buf(),
            source,
        })?;
    file.write_all(body)
        .map_err(|source| StackError::AgentInstallerLogPersist {
            path: path.to_path_buf(),
            source,
        })?;
    file.sync_all()
        .map_err(|source| StackError::AgentInstallerLogPersist {
            path: path.to_path_buf(),
            source,
        })
}

fn sync_parent_directory(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    sync_directory(parent)
}

fn sync_directory(path: &Path) -> Result<()> {
    let directory =
        std::fs::File::open(path).map_err(|source| StackError::AgentInstallerLogPersist {
            path: path.to_path_buf(),
            source,
        })?;
    directory
        .sync_all()
        .map_err(|source| StackError::AgentInstallerLogPersist {
            path: path.to_path_buf(),
            source,
        })
}

/// Convert an arbitrary string into a path-safe single segment. Replaces
/// `/`, `\`, and ASCII control chars with `_`. The `agent_id` and `step`
/// values are already safe (alphanumeric and `-`), so this is defense in
/// depth; `started_at` carries `:` which is fine on POSIX but worth keeping
/// readable.
fn sanitize_for_path(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\') {
                '_'
            } else {
                c
            }
        })
        .collect()
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
            log_dir: row.log_dir.as_deref(),
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
struct FallbackChain {
    rows: Vec<InstallerRowDraft>,
    terminal_error: Option<StackError>,
}

/// Try each install path declared on the given field in priority order
/// (shell → npm → github_release for floating versions; github → npm for
/// pinned). Returns once one succeeds, or once all declared paths have
/// been exhausted. Each attempt is recorded so the operator can see the
/// fallback chain after the fact via `acps installer history`.
#[allow(clippy::too_many_arguments)]
fn install_one_with_fallback(
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
    loop {
        let spec = match select_install_path(agent_id, field, &remaining, github_url, version_pin) {
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

#[derive(Debug, Clone, Copy)]
enum InstallPathKind {
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

fn final_verification(
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

struct StepResult {
    row: InstallerRowDraft,
    outcome: Result<()>,
}

#[derive(Debug, Clone)]
enum ResolvedInstallSpec {
    Shell {
        script: String,
        creates: String,
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

fn select_install_path(
    agent_id: &str,
    field: &str,
    install: &InstallSet,
    github_url: Option<&str>,
    version_pin: Option<&str>,
) -> Result<ResolvedInstallSpec> {
    if let Some(version) = version_pin {
        if let Some(github) = &install.github {
            return resolve_github_install(agent_id, field, github_url, github, Some(version));
        }
        if let Some(npm) = &install.npm {
            return Ok(ResolvedInstallSpec::Npm {
                package: format!("{}@{version}", npm.package),
                creates: npm.creates.clone(),
                version: Some(version.to_owned()),
            });
        }
        return Err(StackError::RegistryLoad {
            reason: format!(
                "agent `{agent_id}` {field} cannot honor pinned version `{version}` with shell-only install"
            ),
        });
    }

    if let Some(shell) = &install.shell {
        return Ok(ResolvedInstallSpec::Shell {
            script: shell.script.clone(),
            creates: shell.creates.clone(),
        });
    }
    if let Some(npm) = &install.npm {
        return Ok(ResolvedInstallSpec::Npm {
            package: npm.package.clone(),
            creates: npm.creates.clone(),
            version: None,
        });
    }
    if let Some(github) = &install.github {
        return resolve_github_install(agent_id, field, github_url, github, None);
    }

    Err(StackError::RegistryLoad {
        reason: format!("agent `{agent_id}` {field} has no install paths"),
    })
}

fn resolve_github_install(
    agent_id: &str,
    field: &str,
    github_url: Option<&str>,
    github: &GithubInstall,
    version_pin: Option<&str>,
) -> Result<ResolvedInstallSpec> {
    let github_url = github_url.ok_or_else(|| StackError::RegistryLoad {
        reason: format!("agent `{agent_id}` {field}.github requires github URL"),
    })?;
    let repo = github_repo_from_url(agent_id, "github", github_url)?;
    let arch_token = if github.asset_pattern.contains("{arch}")
        || github
            .archive_binary_name
            .as_deref()
            .is_some_and(|name| name.contains("{arch}"))
    {
        let token =
            github
                .arch
                .token_for_host()
                .ok_or_else(|| StackError::UnsupportedHostArch {
                    arch: std::env::consts::ARCH,
                })?;
        Some(token)
    } else {
        None
    };
    let asset_pattern = arch_token.map_or_else(
        || github.asset_pattern.clone(),
        |token| github.asset_pattern.replace("{arch}", token),
    );
    let archive_binary_name = github
        .archive_binary_name
        .as_ref()
        .map(|name| arch_token.map_or_else(|| name.clone(), |token| name.replace("{arch}", token)));
    Ok(ResolvedInstallSpec::GithubRelease {
        repo,
        asset_pattern,
        archive: github.archive,
        archive_binary_name,
        binary_name: github.binary_name.clone(),
        checksums_asset: github.checksums_asset.clone(),
        version_pin: version_pin.map(str::to_owned),
    })
}

fn run_install_step(
    step_label: &'static str,
    spec: ResolvedInstallSpec,
    agent_env: &HashMap<String, String>,
    workspace_root: &Path,
    dest_dir: &Path,
) -> StepResult {
    let started_at = current_timestamp();
    match spec {
        ResolvedInstallSpec::Shell { script, creates } => {
            let result = run_shell_install(&script, agent_env, workspace_root, &[dest_dir]);
            shell_step_with_creates(
                step_label,
                started_at,
                result,
                &creates,
                workspace_root,
                &[dest_dir],
                None,
            )
        }
        ResolvedInstallSpec::Npm {
            package,
            creates,
            version,
        } => {
            let (package, version) = match version {
                Some(version) => (package, version),
                None => match resolve_npm_package_version(
                    step_label,
                    started_at.clone(),
                    &package,
                    agent_env,
                    workspace_root,
                    dest_dir,
                ) {
                    Ok(version) => (npm_package_with_version(&package, &version), version),
                    Err(step) => return *step,
                },
            };
            let result = run_npm_install(&package, agent_env, workspace_root, dest_dir);
            shell_step_with_creates(
                step_label,
                started_at,
                result,
                &creates,
                workspace_root,
                &[dest_dir],
                Some(version),
            )
        }
        ResolvedInstallSpec::GithubRelease {
            repo,
            asset_pattern,
            archive,
            archive_binary_name,
            binary_name,
            checksums_asset,
            version_pin,
        } => {
            let install = GithubReleaseInstall {
                repo: &repo,
                asset_pattern: &asset_pattern,
                archive,
                archive_binary_name: archive_binary_name.as_deref(),
                binary_name: &binary_name,
                checksums_asset: checksums_asset.as_deref(),
            };
            github_release_step(
                step_label,
                started_at,
                install,
                version_pin.as_deref(),
                agent_env,
                dest_dir,
            )
        }
    }
}

fn shell_step_with_creates(
    step_label: &'static str,
    started_at: String,
    run_result: Result<CapturedOutput>,
    creates: &str,
    workspace_root: &Path,
    extra_path_dirs: &[&Path],
    version: Option<String>,
) -> StepResult {
    let finished_at = current_timestamp();
    match run_result {
        Ok(captured) => {
            let exit_ok = captured.exit_status == Some(0);
            let mut row = InstallerRowDraft {
                started_at,
                finished_at: Some(finished_at),
                status: if exit_ok { "ran" } else { "failed" }.into(),
                stdout: captured.stdout.clone(),
                stderr: captured.stderr.clone(),
                exit_status: captured.exit_status,
                step: step_label.to_owned(),
                version: version.clone(),
                log_dir: None,
            };
            if !exit_ok {
                return StepResult {
                    outcome: Err(StackError::AgentInstallerFailed {
                        exit: captured.exit_status,
                        stderr_tail: tail_bytes(&captured.stderr, STDERR_TAIL_BYTES),
                    }),
                    row,
                };
            }
            let outcome = resolve_creates(creates, workspace_root, extra_path_dirs)
                .map(|_| ())
                .ok_or_else(|| StackError::AgentInstallerCreatesMissing {
                    name: creates.to_owned(),
                });
            if let Err(err) = &outcome {
                row.status = "failed".to_owned();
                row.stderr = append_stderr_detail(&row.stderr, err);
            }
            StepResult { outcome, row }
        }
        Err(StackError::AgentInstallerTimeout) => StepResult {
            outcome: Err(StackError::AgentInstallerTimeout),
            row: InstallerRowDraft {
                started_at,
                finished_at: Some(finished_at),
                status: "timeout".into(),
                stdout: String::new(),
                stderr: "[installer timed out]".into(),
                exit_status: None,
                step: step_label.to_owned(),
                version: version.clone(),
                log_dir: None,
            },
        },
        Err(err) => StepResult {
            outcome: Err(err),
            row: InstallerRowDraft {
                started_at,
                finished_at: Some(finished_at),
                status: "error".into(),
                stdout: String::new(),
                stderr: String::new(),
                exit_status: None,
                step: step_label.to_owned(),
                version,
                log_dir: None,
            },
        },
    }
}

fn github_release_step(
    step_label: &'static str,
    started_at: String,
    install: GithubReleaseInstall<'_>,
    version_pin: Option<&str>,
    agent_env: &HashMap<String, String>,
    dest_dir: &Path,
) -> StepResult {
    let result = github_release::install(install, version_pin, dest_dir, agent_env);
    let finished_at = current_timestamp();
    match result {
        Ok(outcome) => StepResult {
            outcome: Ok(()),
            row: InstallerRowDraft {
                started_at,
                finished_at: Some(finished_at),
                status: "ran".into(),
                stdout: outcome.log,
                stderr: String::new(),
                exit_status: Some(0),
                step: step_label.to_owned(),
                version: Some(outcome.release_tag),
                log_dir: None,
            },
        },
        Err(err) => {
            let stderr = err.to_string();
            StepResult {
                outcome: Err(err),
                row: InstallerRowDraft {
                    started_at,
                    finished_at: Some(finished_at),
                    status: "error".into(),
                    stdout: String::new(),
                    stderr,
                    exit_status: None,
                    step: step_label.to_owned(),
                    version: version_pin.map(str::to_owned),
                    log_dir: None,
                },
            }
        }
    }
}

fn finalize_shell_step(
    step_label: &'static str,
    started_at: String,
    run_result: Result<CapturedOutput>,
    creates: &str,
    expected_sha256: Option<&str>,
    workspace_root: &Path,
) -> InstallerResult {
    let finished_at = current_timestamp();
    match run_result {
        Ok(captured) => {
            let exit_ok = captured.exit_status == Some(0);
            let mut row = InstallerRowDraft {
                started_at,
                finished_at: Some(finished_at),
                status: if exit_ok { "ran" } else { "failed" }.into(),
                stdout: captured.stdout.clone(),
                stderr: captured.stderr.clone(),
                exit_status: captured.exit_status,
                step: step_label.to_owned(),
                version: None,
                log_dir: None,
            };
            if !exit_ok {
                return InstallerResult {
                    outcome: Err(StackError::AgentInstallerFailed {
                        exit: captured.exit_status,
                        stderr_tail: tail_bytes(&captured.stderr, STDERR_TAIL_BYTES),
                    }),
                    row,
                };
            }
            let outcome = (|| {
                let resolved = resolve_creates(creates, workspace_root, &[]).ok_or_else(|| {
                    StackError::AgentInstallerCreatesMissing {
                        name: creates.to_owned(),
                    }
                })?;
                let sha256 = sha256_of_file(&resolved)?;
                verify_expected_sha256(expected_sha256, &sha256)?;
                Ok(InstallerOutcome::Installed {
                    path: resolved,
                    sha256,
                })
            })();
            if let Err(err) = &outcome {
                row.status = "failed".to_owned();
                row.stderr = append_stderr_detail(&row.stderr, err);
            }
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
                step: step_label.to_owned(),
                version: None,
                log_dir: None,
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
                step: step_label.to_owned(),
                version: None,
                log_dir: None,
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
    extra_path_dirs: &[&Path],
) -> Result<CapturedOutput> {
    run_program_install(
        "/bin/sh",
        &["-c".to_owned(), shell.to_owned()],
        agent_env,
        workspace_root,
        extra_path_dirs,
    )
}

fn run_npm_install(
    package: &str,
    agent_env: &HashMap<String, String>,
    workspace_root: &Path,
    dest_dir: &Path,
) -> Result<CapturedOutput> {
    let prefix = dest_dir.parent().ok_or_else(|| StackError::RegistryLoad {
        reason: format!(
            "managed bin directory {} has no parent for npm --prefix",
            dest_dir.display()
        ),
    })?;
    let args = vec![
        "install".to_owned(),
        "-g".to_owned(),
        "--prefix".to_owned(),
        prefix.to_string_lossy().into_owned(),
        package.to_owned(),
    ];
    run_program_install("npm", &args, agent_env, workspace_root, &[dest_dir])
}

fn resolve_npm_package_version(
    step_label: &'static str,
    started_at: String,
    package: &str,
    agent_env: &HashMap<String, String>,
    workspace_root: &Path,
    dest_dir: &Path,
) -> std::result::Result<String, Box<StepResult>> {
    let args = vec![
        "view".to_owned(),
        package.to_owned(),
        "version".to_owned(),
        "--json".to_owned(),
    ];
    let result = run_program_install("npm", &args, agent_env, workspace_root, &[dest_dir]);
    match result {
        Ok(captured) if captured.exit_status == Some(0) => {
            let parsed = serde_json::from_str::<String>(captured.stdout.trim()).map_err(|err| {
                format!("npm view {package} version --json returned invalid JSON string: {err}")
            });
            match parsed {
                Ok(version) if !version.trim().is_empty() => Ok(version),
                Ok(_) => Err(Box::new(npm_version_failure_step(
                    step_label,
                    started_at,
                    captured,
                    "npm view returned an empty version".to_owned(),
                ))),
                Err(reason) => Err(Box::new(npm_version_failure_step(
                    step_label, started_at, captured, reason,
                ))),
            }
        }
        Ok(captured) => {
            let exit = captured.exit_status;
            let stderr_tail = tail_bytes(&captured.stderr, STDERR_TAIL_BYTES);
            Err(Box::new(StepResult {
                outcome: Err(StackError::AgentInstallerFailed { exit, stderr_tail }),
                row: InstallerRowDraft {
                    started_at,
                    finished_at: Some(current_timestamp()),
                    status: "failed".into(),
                    stdout: captured.stdout,
                    stderr: captured.stderr,
                    exit_status: captured.exit_status,
                    step: step_label.to_owned(),
                    version: None,
                    log_dir: None,
                },
            }))
        }
        Err(err) => Err(Box::new(StepResult {
            outcome: Err(err),
            row: InstallerRowDraft {
                started_at,
                finished_at: Some(current_timestamp()),
                status: "failed".into(),
                stdout: String::new(),
                stderr: String::new(),
                exit_status: None,
                step: step_label.to_owned(),
                version: None,
                log_dir: None,
            },
        })),
    }
}

fn npm_version_failure_step(
    step_label: &'static str,
    started_at: String,
    captured: CapturedOutput,
    reason: String,
) -> StepResult {
    StepResult {
        outcome: Err(StackError::AgentInitializeFailed {
            reason: reason.clone(),
        }),
        row: InstallerRowDraft {
            started_at,
            finished_at: Some(current_timestamp()),
            status: "failed".into(),
            stdout: captured.stdout,
            stderr: append_stderr_detail(&captured.stderr, &reason),
            exit_status: captured.exit_status,
            step: step_label.to_owned(),
            version: None,
            log_dir: None,
        },
    }
}

fn npm_package_with_version(package: &str, version: &str) -> String {
    format!("{package}@{version}")
}

fn append_stderr_detail(stderr: &str, detail: impl std::fmt::Display) -> String {
    if stderr.is_empty() {
        detail.to_string()
    } else {
        format!("{stderr}\n{detail}")
    }
}

fn run_program_install(
    program: &str,
    args: &[String],
    agent_env: &HashMap<String, String>,
    workspace_root: &Path,
    extra_path_dirs: &[&Path],
) -> Result<CapturedOutput> {
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(workspace_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear();

    // Minimal env so the installer is no wider a door than the agent itself.
    // PATH is required so `creates` lookups resolve. HOME lets installers
    // place dotfiles in the operator's home if they need to. LANG keeps
    // tools that read locale from producing mojibake.
    if let Some(path) = path_env_with_extra_dirs(extra_path_dirs) {
        command.env("PATH", path);
    }
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

fn path_env_with_extra_dirs(extra_path_dirs: &[&Path]) -> Option<std::ffi::OsString> {
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
fn resolve_creates(
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
    use crate::agent_registry::{AdapterSpec, HarnessSpec, ShellInstall};
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
            install: None,
        }
    }

    fn shell_install_set(script: &str, creates: &str) -> InstallSet {
        InstallSet {
            shell: Some(ShellInstall {
                script: script.to_owned(),
                creates: creates.to_owned(),
            }),
            ..InstallSet::default()
        }
    }

    fn harness_spec(id: &str, install: InstallSet) -> HarnessSpec {
        HarnessSpec {
            id: id.to_owned(),
            install,
        }
    }

    fn adapter_spec(id: &str, install: InstallSet) -> AdapterSpec {
        AdapterSpec {
            id: id.to_owned(),
            github: None,
            install,
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
            npm: Some(crate::agent_registry::NpmInstall {
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
            npm: Some(crate::agent_registry::NpmInstall {
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
            npm: Some(crate::agent_registry::NpmInstall {
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
            npm: Some(crate::agent_registry::NpmInstall {
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
            npm: Some(crate::agent_registry::NpmInstall {
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

        let install = InstallSet {
            shell: Some(ShellInstall {
                script: "exit 1".to_owned(),
                creates: "fallback-agent".to_owned(),
            }),
            npm: Some(crate::runtime::agent_registry::NpmInstall {
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
