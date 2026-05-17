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
use std::io::Read;
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
const STEP_INSTALL: &str = "install";
const STEP_HARNESS: &str = "harness";
const STEP_ADAPTER: &str = "adapter";

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
/// briefly: runs the escape-hatch installer and persists the row.
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
        step: &result.row.step,
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
) -> Result<InstallerOutcome> {
    let result = install_resolved_capture(agent, entry, agent_env, workspace_root, dest_dir);
    for row in &result.rows {
        state.append_installer_run(InstallerRunInput {
            started_at: &row.started_at,
            finished_at: row.finished_at.as_deref(),
            status: &row.status,
            stdout: &row.stdout,
            stderr: &row.stderr,
            exit_status: row.exit_status,
            step: &row.step,
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
    let harness_spec = match select_install_path(
        &entry.id,
        "harness.install",
        &harness.install,
        entry.github.as_deref(),
        agent.harness_version.as_deref(),
    ) {
        Ok(spec) => spec,
        Err(err) => {
            return InstallerSequenceResult {
                outcome: Err(err),
                rows,
            };
        }
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
        let adapter_spec = match select_install_path(
            &entry.id,
            "adapter.install",
            &adapter.install,
            adapter.github.as_deref(),
            None,
        ) {
            Ok(spec) => spec,
            Err(err) => {
                return InstallerSequenceResult {
                    outcome: Err(err),
                    rows,
                };
            }
        };

        let harness_workspace = workspace_root.to_path_buf();
        let harness_dest = dest_dir.to_path_buf();
        let harness_env = installer_env.clone();
        let adapter_workspace = workspace_root.to_path_buf();
        let adapter_dest = dest_dir.to_path_buf();
        let adapter_env = installer_env.clone();
        let harness_thread = std::thread::spawn(move || {
            run_install_step(
                STEP_HARNESS,
                harness_spec,
                &harness_env,
                &harness_workspace,
                &harness_dest,
            )
        });
        let adapter_thread = std::thread::spawn(move || {
            run_install_step(
                STEP_ADAPTER,
                adapter_spec,
                &adapter_env,
                &adapter_workspace,
                &adapter_dest,
            )
        });
        let harness_step = harness_thread.join().unwrap_or_else(|_| StepResult {
            row: InstallerRowDraft::config_error(STEP_HARNESS),
            outcome: Err(StackError::AgentInitializeFailed {
                reason: "harness installer thread panicked".to_owned(),
            }),
        });
        let adapter_step = adapter_thread.join().unwrap_or_else(|_| StepResult {
            row: InstallerRowDraft::config_error(STEP_ADAPTER),
            outcome: Err(StackError::AgentInitializeFailed {
                reason: "adapter installer thread panicked".to_owned(),
            }),
        });
        let harness_outcome = harness_step.outcome;
        let adapter_outcome = adapter_step.outcome;
        rows.push(harness_step.row);
        rows.push(adapter_step.row);
        if let Err(err) = harness_outcome {
            return InstallerSequenceResult {
                outcome: Err(err),
                rows,
            };
        }
        if let Err(err) = adapter_outcome {
            return InstallerSequenceResult {
                outcome: Err(err),
                rows,
            };
        }

        return final_verification(agent, workspace_root, dest_dir, rows);
    }

    let step = run_install_step(
        harness_step_label,
        harness_spec,
        &installer_env,
        workspace_root,
        dest_dir,
    );
    rows.push(step.row);
    if let Err(err) = step.outcome {
        return InstallerSequenceResult {
            outcome: Err(err),
            rows,
        };
    }

    final_verification(agent, workspace_root, dest_dir, rows)
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
    },
    GithubRelease {
        repo: String,
        asset_pattern: String,
        archive: ArchiveKind,
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
    let asset_pattern = if github.asset_pattern.contains("{arch}") {
        let token =
            github
                .arch
                .token_for_host()
                .ok_or_else(|| StackError::UnsupportedHostArch {
                    arch: std::env::consts::ARCH,
                })?;
        github.asset_pattern.replace("{arch}", token)
    } else {
        github.asset_pattern.clone()
    };
    Ok(ResolvedInstallSpec::GithubRelease {
        repo,
        asset_pattern,
        archive: github.archive,
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
            )
        }
        ResolvedInstallSpec::Npm { package, creates } => {
            let result = run_npm_install(&package, agent_env, workspace_root, dest_dir);
            shell_step_with_creates(
                step_label,
                started_at,
                result,
                &creates,
                workspace_root,
                &[dest_dir],
            )
        }
        ResolvedInstallSpec::GithubRelease {
            repo,
            asset_pattern,
            archive,
            binary_name,
            checksums_asset,
            version_pin,
        } => {
            let install = GithubReleaseInstall {
                repo: &repo,
                asset_pattern: &asset_pattern,
                archive,
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
) -> StepResult {
    let finished_at = current_timestamp();
    match run_result {
        Ok(captured) => {
            let exit_ok = captured.exit_status == Some(0);
            let row_status = if exit_ok { "ran" } else { "failed" };
            let row = InstallerRowDraft {
                started_at,
                finished_at: Some(finished_at),
                status: row_status.into(),
                stdout: captured.stdout.clone(),
                stderr: captured.stderr.clone(),
                exit_status: captured.exit_status,
                step: step_label.to_owned(),
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
            let row_status = if exit_ok { "ran" } else { "failed" };
            let row = InstallerRowDraft {
                started_at,
                finished_at: Some(finished_at),
                status: row_status.into(),
                stdout: captured.stdout.clone(),
                stderr: captured.stderr.clone(),
                exit_status: captured.exit_status,
                step: step_label.to_owned(),
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

/// Resolve `[agent.install].creates` to a real path. Matches the documented
/// behavior in `docs/specs/runtime.md`: absolute paths used as-is; paths
/// containing `/` resolved relative to `workspace_root` so an installer can
/// declare `creates = "bin/agent"` without depending on operator cwd; bare
/// names looked up on `PATH`.
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
            stdio_framing: Default::default(),
            website: None,
            github: None,
            support_doc: support_doc.map(str::to_owned),
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
            stdio_framing: Default::default(),
            website: None,
            github: None,
            support_doc: support_doc.map(str::to_owned),
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
        assert_eq!(runs[0].step, "install");
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
        assert_eq!(runs[0].step, "install");
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
        assert_eq!(runs[0].step, "install");
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
