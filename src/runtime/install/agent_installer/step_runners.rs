//! Per-kind installer step execution.
//!
//! The orchestrator in the parent module picks a path from the fallback chain
//! and hands it to [`run_install_step`]; this module owns the actual shell,
//! npm, and github_release execution and the helpers that build their rows.

use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::error::{Result, StackError};
use crate::runtime::install::agent_registry::{GithubInstall, InstallSet, github_repo_from_url};
use crate::runtime::install::github_release::{self, GithubReleaseInstall};
use crate::runtime::process_runner::{
    forward_host_env, join_reader_bounded, kill_process_group, path_env_with_extra_dirs,
    spawn_capped_reader, wait_with_timeout,
};

use super::{
    INSTALL_METHOD_GITHUB, INSTALL_METHOD_NPM, INSTALL_METHOD_SHELL, InstallerOutcome,
    InstallerResult, InstallerRowDraft, MAX_INSTALLER_STREAM_BYTES, ResolvedInstallSpec,
    StepResult, current_timestamp, resolve_creates, sha256_of_file, verify_expected_sha256,
};

const INSTALLER_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const STDERR_TAIL_BYTES: usize = 2 * 1024;

/// Pick the install path to attempt for a given field. Honors a pinned
/// version (github > npm) when supplied, otherwise walks the floating
/// priority chain shell > npm > github_release.
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
) -> StepResult {
    let started_at = current_timestamp();
    match spec {
        ResolvedInstallSpec::Shell {
            script,
            creates,
            required_tools: _,
        } => {
            let result = run_shell_install(&script, agent_env, workspace_root, &[dest_dir]);
            shell_step_with_creates(
                step_label,
                started_at,
                result,
                CreatesCheck {
                    creates: &creates,
                    workspace_root,
                    extra_path_dirs: &[dest_dir],
                },
                Some(INSTALL_METHOD_SHELL.to_owned()),
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
                CreatesCheck {
                    creates: &creates,
                    workspace_root,
                    extra_path_dirs: &[dest_dir],
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
                dest_dir,
            )
        }
    }
}

pub(super) struct CreatesCheck<'a> {
    creates: &'a str,
    workspace_root: &'a Path,
    extra_path_dirs: &'a [&'a Path],
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
            .map(|_| ())
            .ok_or_else(|| StackError::AgentInstallerCreatesMissing {
                name: creates_check.creates.to_owned(),
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
                method: method.clone(),
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
                method,
                version,
                log_dir: None,
            },
        },
    }
}

pub(super) fn github_release_step(
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
                method: Some(INSTALL_METHOD_GITHUB.to_owned()),
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
                    method: Some(INSTALL_METHOD_GITHUB.to_owned()),
                    version: version_pin.map(str::to_owned),
                    log_dir: None,
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
                method: Some(INSTALL_METHOD_SHELL.to_owned()),
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
                method: Some(INSTALL_METHOD_SHELL.to_owned()),
                version: None,
                log_dir: None,
            },
        },
    }
}

pub(super) struct CapturedOutput {
    pub(super) stdout: String,
    pub(super) stderr: String,
    pub(super) exit_status: Option<i32>,
}

pub(super) fn run_shell_install(
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
                    method: Some(INSTALL_METHOD_NPM.to_owned()),
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
                method: Some(INSTALL_METHOD_NPM.to_owned()),
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
            method: Some(INSTALL_METHOD_NPM.to_owned()),
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
    if !workspace_root.is_dir() {
        return Err(StackError::AgentInstallerWorkingDirectoryMissing {
            path: workspace_root.to_path_buf(),
        });
    }

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
    let stdout_handle = child
        .stdout
        .take()
        .map(|r| spawn_capped_reader(r, MAX_INSTALLER_STREAM_BYTES));
    let stderr_handle = child
        .stderr
        .take()
        .map(|r| spawn_capped_reader(r, MAX_INSTALLER_STREAM_BYTES));

    let deadline = Instant::now() + INSTALLER_TIMEOUT;
    let exit = wait_with_timeout(&mut child, deadline);

    match exit {
        Ok(Some(status)) => {
            // Kill any grandchildren that inherited stdout/stderr from the
            // shell. Without this, a `(slow-process &)` in the installer
            // leaves the pipes open and the reader threads block forever
            // on EOF — exactly the bypass the spec hardening guards against.
            kill_process_group(&mut child);
            let stdout = stdout_handle
                .and_then(join_reader_bounded)
                .unwrap_or_default();
            let stderr = stderr_handle
                .and_then(join_reader_bounded)
                .unwrap_or_default();
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
            let _ = stdout_handle.and_then(join_reader_bounded);
            let _ = stderr_handle.and_then(join_reader_bounded);
            Err(StackError::AgentInstallerTimeout)
        }
        Err(source) => {
            let _ = child.kill();
            let _ = child.wait();
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
