//! Per-kind installer step execution: shell, npm, and github_release runs
//! plus the helpers that build their persisted rows.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use crate::error::{Result, StackError};
use crate::runtime::install::agent_registry::{GithubInstall, InstallSet, github_repo_from_url};
use crate::runtime::install::github_release::{self, GithubReleaseInstall};
use crate::runtime::process_runner::{
    CaptureOutcome, apply_non_interactive_env, forward_host_env, join_reader_bounded,
    kill_process_group, path_env_with_extra_dirs, resolved_python_interpreter, run_captured,
};

use super::{
    INSTALL_METHOD_GITHUB, INSTALL_METHOD_NPM, INSTALL_METHOD_SHELL, InstallerOutcome,
    InstallerResult, InstallerRowDraft, MAX_INSTALLER_STREAM_BYTES, ResolvedInstallSpec,
    StepResult, current_timestamp, resolve_creates, sha256_of_file, verify_binary_spawns,
    verify_executable_header, verify_expected_sha256,
};

/// Whole-run budget for one install step when nothing declares its own.
pub(super) const DEFAULT_INSTALLER_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const STDERR_TAIL_BYTES: usize = 2 * 1024;
/// npm 12's lifecycle-script skip is soft (exit 0, bin linked), so agents that
/// fetch their real binary from a postinstall silently land a stub without
/// this. Not `--strict-allow-scripts`: that hard-fails on any unapproved
/// script in the tree, and npm-only agents have no fallback path.
const NPM_ALLOW_SCRIPTS_FLAG: &str = "--allow-scripts";

/// Pick the install path to attempt for a given field: a pinned version
/// (github > npm) when supplied, otherwise shell > npm > github_release.
pub(super) fn select_install_path(
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
                name: npm.package.clone(),
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
            required_tools: shell.required_tools.clone(),
            timeout: shell
                .timeout_secs
                .map_or(DEFAULT_INSTALLER_TIMEOUT, Duration::from_secs),
        });
    }
    if let Some(npm) = &install.npm {
        return Ok(ResolvedInstallSpec::Npm {
            package: npm.package.clone(),
            name: npm.package.clone(),
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

pub(super) fn resolve_github_install(
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

pub(super) fn run_install_step(
    step_label: &'static str,
    spec: ResolvedInstallSpec,
    agent_env: &HashMap<String, String>,
    workspace_root: &Path,
    dest_dir: &Path,
    pin_declared: bool,
) -> StepResult {
    let started_at = current_timestamp();
    match spec {
        ResolvedInstallSpec::Shell {
            script,
            creates,
            required_tools: _,
            timeout,
        } => {
            let result =
                run_shell_install(&script, agent_env, workspace_root, &[dest_dir], timeout);
            shell_step_with_creates(
                step_label,
                started_at,
                result,
                CreatesCheck {
                    creates: &creates,
                    workspace_root,
                    extra_path_dirs: &[dest_dir],
                    pin_declared,
                },
                Some(INSTALL_METHOD_SHELL.to_owned()),
                None,
            )
        }
        ResolvedInstallSpec::Npm {
            package,
            name,
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
            let result = run_npm_install(&package, &name, agent_env, workspace_root, dest_dir);
            shell_step_with_creates(
                step_label,
                started_at,
                result,
                CreatesCheck {
                    creates: &creates,
                    workspace_root,
                    extra_path_dirs: &[dest_dir],
                    pin_declared,
                },
                Some(INSTALL_METHOD_NPM.to_owned()),
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
                workspace_root,
                dest_dir,
                pin_declared,
            )
        }
    }
}

pub(super) struct CreatesCheck<'a> {
    creates: &'a str,
    workspace_root: &'a Path,
    extra_path_dirs: &'a [&'a Path],
    pin_declared: bool,
}

pub(super) fn shell_step_with_creates(
    step_label: &'static str,
    started_at: String,
    run_result: Result<CapturedOutput>,
    creates_check: CreatesCheck<'_>,
    method: Option<String>,
    version: Option<String>,
) -> StepResult {
    let finished_at = current_timestamp();
    match run_result {
        Ok(captured) => {
            if let Some(timeout) = captured.timed_out_after {
                return StepResult {
                    outcome: Err(StackError::AgentInstallerTimeout),
                    row: timed_out_row(
                        step_label,
                        started_at,
                        finished_at,
                        captured,
                        timeout,
                        method,
                        version,
                    ),
                };
            }
            let exit_ok = captured.exit_status == Some(0);
            let mut row = InstallerRowDraft {
                started_at,
                finished_at: Some(finished_at),
                status: if exit_ok { "ran" } else { "failed" }.into(),
                stdout: captured.stdout.clone(),
                stderr: captured.stderr.clone(),
                exit_status: captured.exit_status,
                step: step_label.to_owned(),
                method: method.clone(),
                version: version.clone(),
                log_dir: None,
                persisted_run_id: None,
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
            let outcome = resolve_creates(
                creates_check.creates,
                creates_check.workspace_root,
                creates_check.extra_path_dirs,
            )
            .ok_or_else(|| StackError::AgentInstallerCreatesMissing {
                name: creates_check.creates.to_owned(),
            })
            .and_then(|path| {
                // A declared sha256 pin is only checked in `final_verification`,
                // so spawning here would execute the binary before its
                // integrity is proven. The header check never executes it.
                if creates_check.pin_declared {
                    verify_executable_header(&path)
                } else {
                    verify_binary_spawns(
                        &path,
                        creates_check.workspace_root,
                        creates_check.extra_path_dirs,
                    )
                }
            });
            if let Err(err) = &outcome {
                row.status = "failed".to_owned();
                row.stderr = append_stderr_detail(&row.stderr, err);
            }
            StepResult { outcome, row }
        }
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
                method,
                version,
                log_dir: None,
                persisted_run_id: None,
            },
        },
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn github_release_step(
    step_label: &'static str,
    started_at: String,
    install: GithubReleaseInstall<'_>,
    version_pin: Option<&str>,
    agent_env: &HashMap<String, String>,
    workspace_root: &Path,
    dest_dir: &Path,
    pin_declared: bool,
) -> StepResult {
    let binary_path = dest_dir.join(install.binary_name);
    let result = github_release::install(install, version_pin, dest_dir, agent_env);
    let finished_at = current_timestamp();
    match result {
        Ok(outcome) => {
            // A wrong-arch asset extracts fine and only fails at first spawn,
            // so gate here. Safe to spawn because asset checksums were already
            // verified in the download — unless an `expected_sha256` pin is
            // declared, which only `final_verification` checks, so that lane
            // stays header-only.
            let gate = if pin_declared {
                verify_executable_header(&binary_path)
            } else {
                verify_binary_spawns(&binary_path, workspace_root, &[dest_dir])
            };
            let mut row = InstallerRowDraft {
                started_at,
                finished_at: Some(finished_at),
                status: "ran".into(),
                stdout: outcome.log,
                stderr: String::new(),
                exit_status: Some(0),
                step: step_label.to_owned(),
                method: Some(INSTALL_METHOD_GITHUB.to_owned()),
                version: Some(outcome.release_tag),
                log_dir: None,
                persisted_run_id: None,
            };
            if let Err(err) = &gate {
                row.status = "failed".to_owned();
                row.stderr = append_stderr_detail(&row.stderr, err);
            }
            StepResult { outcome: gate, row }
        }
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
                    method: Some(INSTALL_METHOD_GITHUB.to_owned()),
                    version: version_pin.map(str::to_owned),
                    log_dir: None,
                    persisted_run_id: None,
                },
            }
        }
    }
}

pub(super) fn finalize_shell_step(
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
            if let Some(timeout) = captured.timed_out_after {
                return InstallerResult {
                    outcome: Err(StackError::AgentInstallerTimeout),
                    row: timed_out_row(
                        step_label,
                        started_at,
                        finished_at,
                        captured,
                        timeout,
                        Some(INSTALL_METHOD_SHELL.to_owned()),
                        None,
                    ),
                };
            }
            let exit_ok = captured.exit_status == Some(0);
            let mut row = InstallerRowDraft {
                started_at,
                finished_at: Some(finished_at),
                status: if exit_ok { "ran" } else { "failed" }.into(),
                stdout: captured.stdout.clone(),
                stderr: captured.stderr.clone(),
                exit_status: captured.exit_status,
                step: step_label.to_owned(),
                method: Some(INSTALL_METHOD_SHELL.to_owned()),
                version: None,
                log_dir: None,
                persisted_run_id: None,
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
                verify_binary_spawns(&resolved, workspace_root, &[])?;
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
                method: Some(INSTALL_METHOD_SHELL.to_owned()),
                version: None,
                log_dir: None,
                persisted_run_id: None,
            },
        },
    }
}

pub(super) struct CapturedOutput {
    pub(super) stdout: String,
    pub(super) stderr: String,
    pub(super) exit_status: Option<i32>,
    /// Budget the run exceeded, when it was killed for hitting it. The
    /// accompanying `exit_status` is `None`, so a caller that ignores this
    /// field still treats the step as failed, never as success.
    pub(super) timed_out_after: Option<Duration>,
}

pub(super) fn run_shell_install(
    shell: &str,
    agent_env: &HashMap<String, String>,
    workspace_root: &Path,
    extra_path_dirs: &[&Path],
    timeout: Duration,
) -> Result<CapturedOutput> {
    run_program_install(
        "/bin/sh",
        &["-c".to_owned(), shell.to_owned()],
        agent_env,
        workspace_root,
        extra_path_dirs,
        timeout,
    )
}

/// Serializes `npm install -g --prefix` runs: npm has no cross-process lock
/// for global installs into one prefix, and the harness and adapter installers
/// run on parallel threads against the same managed prefix.
static NPM_INSTALL_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn run_npm_install(
    package: &str,
    name: &str,
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
        format!("{NPM_ALLOW_SCRIPTS_FLAG}={name}"),
        package.to_owned(),
    ];
    let _guard = NPM_INSTALL_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    run_program_install(
        "npm",
        &args,
        agent_env,
        workspace_root,
        &[dest_dir],
        DEFAULT_INSTALLER_TIMEOUT,
    )
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
    let result = run_program_install(
        "npm",
        &args,
        agent_env,
        workspace_root,
        &[dest_dir],
        DEFAULT_INSTALLER_TIMEOUT,
    );
    match result {
        Ok(captured) if captured.exit_status == Some(0) => {
            let parsed = parse_npm_view_version(captured.stdout.trim()).map_err(|err| {
                format!("npm view {package} version --json returned unexpected JSON: {err}")
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
            if let Some(timeout) = captured.timed_out_after {
                return Err(Box::new(StepResult {
                    outcome: Err(StackError::AgentInstallerTimeout),
                    row: timed_out_row(
                        step_label,
                        started_at,
                        current_timestamp(),
                        captured,
                        timeout,
                        Some(INSTALL_METHOD_NPM.to_owned()),
                        None,
                    ),
                }));
            }
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
                    method: Some(INSTALL_METHOD_NPM.to_owned()),
                    version: None,
                    log_dir: None,
                    persisted_run_id: None,
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
                method: Some(INSTALL_METHOD_NPM.to_owned()),
                version: None,
                log_dir: None,
                persisted_run_id: None,
            },
        })),
    }
}

/// `npm view <pkg> version --json` prints a bare string for one match but an
/// ascending JSON array when several match, so the last element is newest.
fn parse_npm_view_version(stdout: &str) -> std::result::Result<String, String> {
    let value: serde_json::Value = serde_json::from_str(stdout).map_err(|err| err.to_string())?;
    match value {
        serde_json::Value::String(version) => Ok(version),
        serde_json::Value::Array(items) => {
            let mut versions = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    serde_json::Value::String(version) => versions.push(version),
                    other => {
                        return Err(format!(
                            "invalid type: array element {other}, expected a string"
                        ));
                    }
                }
            }
            Ok(versions.pop().unwrap_or_default())
        }
        other => Err(format!(
            "invalid type: {other}, expected a string or array of strings"
        )),
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
            method: Some(INSTALL_METHOD_NPM.to_owned()),
            version: None,
            log_dir: None,
            persisted_run_id: None,
        },
    }
}

fn npm_package_with_version(package: &str, version: &str) -> String {
    format!("{package}@{version}")
}

/// Row for a step killed for exceeding its budget; the drained output is kept
/// and the marker appended rather than substituted.
fn timed_out_row(
    step_label: &'static str,
    started_at: String,
    finished_at: String,
    captured: CapturedOutput,
    timeout: Duration,
    method: Option<String>,
    version: Option<String>,
) -> InstallerRowDraft {
    let marker = format!("[installer timed out after {}s]", timeout.as_secs());
    InstallerRowDraft {
        started_at,
        finished_at: Some(finished_at),
        status: "timeout".into(),
        stdout: captured.stdout,
        stderr: append_stderr_detail(&captured.stderr, marker),
        exit_status: None,
        step: step_label.to_owned(),
        method,
        version,
        log_dir: None,
        persisted_run_id: None,
    }
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
    timeout: Duration,
) -> Result<CapturedOutput> {
    if !workspace_root.is_dir() {
        return Err(StackError::AgentInstallerWorkingDirectoryMissing {
            path: workspace_root.to_path_buf(),
        });
    }

    let mut command = Command::new(program);
    command.args(args).current_dir(workspace_root).env_clear();

    // Minimal env so the installer is no wider a door than the agent itself.
    let path_env = path_env_with_extra_dirs(extra_path_dirs);
    if let Some(path) = &path_env {
        command.env("PATH", path);
    }
    forward_host_env(&mut command, "HOME");
    forward_host_env(&mut command, "LANG");
    apply_non_interactive_env(&mut command);
    // Point node-gyp at the interpreter binary rather than whatever wrapper
    // `python3` happens to be. Set before `[agent].env` so an entry that knows
    // better can override it, unlike the reserved names below.
    if let Some(interpreter) = resolved_python_interpreter(path_env.as_ref()) {
        command.env("npm_config_python", interpreter);
    }
    // `[agent].env` must not override PATH/HOME/LANG (the daemon owns where
    // binaries and the operator's home live) or the non-interactive hints (an
    // agent entry must not re-enable prompting in a headless install).
    for (name, value) in agent_env {
        if matches!(
            name.as_str(),
            "PATH"
                | "HOME"
                | "LANG"
                | "CI"
                | "NONINTERACTIVE"
                | "DEBIAN_FRONTEND"
                | "GIT_TERMINAL_PROMPT"
                | "TERM"
        ) {
            tracing::warn!(
                name = %name,
                "refusing to inject `{name}` from `[agent].env` into installer: reserved",
            );
            continue;
        }
        command.env(name, value);
    }

    // `run_captured` detaches into a fresh session so the timeout SIGKILL
    // reaches grandchildren and an installer probing /dev/tty cannot
    // prompt-and-stop; it also owns the capped reader threads that keep a
    // chatty installer from wedging on a full pipe buffer.
    let outcome = run_captured(&mut command, timeout, MAX_INSTALLER_STREAM_BYTES)
        .map_err(|source| StackError::AgentSpawnFailed { source })?;

    match outcome {
        CaptureOutcome::Exited {
            status,
            stdout,
            stderr,
            ..
        } => Ok(CapturedOutput {
            stdout,
            stderr,
            exit_status: status.code(),
            timed_out_after: None,
        }),
        CaptureOutcome::TimedOut {
            mut child,
            stdout_reader,
            stderr_reader,
        } => {
            kill_process_group(&mut child);
            if let Err(error) = child.wait() {
                tracing::debug!(%error, program, "timed-out installer child reap failed");
            }
            // Threads exit once the pipes close from the kill.
            let stdout = stdout_reader
                .and_then(join_reader_bounded)
                .unwrap_or_default();
            let (stderr, _) = stderr_reader
                .and_then(join_reader_bounded)
                .unwrap_or_default();
            Ok(CapturedOutput {
                stdout,
                stderr,
                exit_status: None,
                timed_out_after: Some(timeout),
            })
        }
        CaptureOutcome::WaitFailed {
            source, mut child, ..
        } => {
            kill_process_group(&mut child);
            if let Err(error) = child.wait() {
                tracing::debug!(%error, program, "unwaitable installer child reap failed");
            }
            Err(StackError::AgentSpawnFailed { source })
        }
    }
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
