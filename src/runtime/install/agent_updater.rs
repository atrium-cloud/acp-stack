use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::config::{AgentConfig, Config};
use crate::error::{Result, StackError};

use crate::runtime::install::agent_installer::{
    INSTALL_METHOD_APT, INSTALL_METHOD_NATIVE, STEP_ADAPTER, STEP_HARNESS, STEP_INSTALL,
    install_one_with_fallback, persist_step_logs_to_disk, resolve_creates,
};
use crate::runtime::install::agent_registry::{
    AdapterSpec, AptUpdate, HarnessSpec, InstallSet, RegistryEntry, RegistryKind,
    github_repo_from_url,
};
use crate::runtime::process_runner::{
    apply_non_interactive_env, detach_into_new_session, forward_host_env, join_reader_bounded,
    kill_process_group, path_env_with_extra_dirs, spawn_capped_reader, wait_with_timeout,
};
use crate::state::{
    INSTALLER_METHOD_APT, INSTALLER_METHOD_GITHUB, INSTALLER_METHOD_NATIVE, INSTALLER_METHOD_NPM,
    INSTALLER_METHOD_SHELL, INSTALLER_OPERATION_UPDATE, INSTALLER_OUTPUT_CAP_BYTES, InstallerRun,
    InstallerRunInput, StateStore,
};

const UPDATE_COMMAND_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const HELP_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const NATIVE_UPDATE_COMMANDS: &[&str] = &["update", "upgrade"];
const PROBE_FAILURE_OUTPUT_TAIL_BYTES: usize = 200;
const HELP_PROBE_RETRY_DELAY: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, Copy, Default)]
pub struct AgentUpdateOptions {
    pub force: bool,
    pub agent_running: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentUpdateReport {
    pub agent: String,
    pub updated: bool,
    pub skipped: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub steps: Vec<AgentUpdateStepReport>,
}

impl AgentUpdateReport {
    pub fn has_failed_steps(&self) -> bool {
        self.steps
            .iter()
            .any(|step| step.status == AgentUpdateStepStatus::Failed)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentUpdateStepReport {
    pub step: String,
    pub status: AgentUpdateStepStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentUpdateStepStatus {
    Updated,
    UpToDate,
    Skipped,
    Failed,
}

pub fn update_agent_for_config(
    config: &Config,
    entry: &RegistryEntry,
    state: &StateStore,
    workspace_root: &Path,
    dest_dir: &Path,
    log_base: Option<&Path>,
    options: AgentUpdateOptions,
) -> Result<AgentUpdateReport> {
    if options.agent_running {
        return Ok(AgentUpdateReport {
            agent: config.agent.id.clone(),
            updated: false,
            skipped: true,
            reason: Some("agent is running".to_owned()),
            steps: Vec::new(),
        });
    }

    entry.ensure_supported()?;
    let installed_rows = state.latest_successful_installer_runs_for_agent(&config.agent.id)?;
    let context = UpdateExecutionContext {
        workspace_root,
        dest_dir,
        state,
        log_base,
        force: options.force,
    };
    let mut steps = Vec::new();
    for component in update_components(entry)? {
        steps.push(update_component(
            &config.agent,
            entry,
            &component,
            installed_rows.iter().find(|row| row.step == component.step),
            &context,
        )?);
    }
    let updated = steps
        .iter()
        .any(|step| step.status == AgentUpdateStepStatus::Updated);
    Ok(AgentUpdateReport {
        agent: config.agent.id.clone(),
        updated,
        skipped: false,
        reason: None,
        steps,
    })
}

struct UpdateExecutionContext<'a> {
    workspace_root: &'a Path,
    dest_dir: &'a Path,
    state: &'a StateStore,
    log_base: Option<&'a Path>,
    force: bool,
}

fn update_component(
    agent: &AgentConfig,
    entry: &RegistryEntry,
    component: &UpdateComponent<'_>,
    installed_row: Option<&InstallerRun>,
    context: &UpdateExecutionContext<'_>,
) -> Result<AgentUpdateStepReport> {
    let plan = choose_update_plan(entry, component, installed_row)?;
    let installed = installed_row.and_then(|row| row.version.clone());
    if let Some(latest) = plan.latest.as_deref()
        && installed_row
            .and_then(|row| row.version.as_deref())
            .is_some_and(|version| normalize_version(version) == normalize_version(latest))
        && !context.force
    {
        return Ok(AgentUpdateStepReport {
            step: component.step.to_owned(),
            status: AgentUpdateStepStatus::UpToDate,
            method: Some(plan.method.to_owned()),
            installed,
            latest: Some(latest.to_owned()),
            message: None,
        });
    }

    let mut rows = match plan.kind {
        UpdatePlanKind::InstallSet => {
            let version_pin = plan.latest.as_deref();
            let chain = install_one_with_fallback(
                &agent.id,
                component.field,
                component.step,
                &plan.install,
                component.github_url,
                version_pin,
                &HashMap::new(),
                context.workspace_root,
                context.dest_dir,
            );
            if let Some(err) = chain.terminal_error {
                let mut rows = chain.rows;
                persist_update_rows(&mut rows, agent, context.state, context.log_base)?;
                return Ok(AgentUpdateStepReport {
                    step: component.step.to_owned(),
                    status: AgentUpdateStepStatus::Failed,
                    method: Some(plan.method.to_owned()),
                    installed,
                    latest: plan.latest,
                    message: Some(err.to_string()),
                });
            }
            chain.rows
        }
        UpdatePlanKind::Apt(apt) => {
            vec![run_apt_update_step(
                component.step,
                apt,
                context.workspace_root,
                context.dest_dir,
            )]
        }
        UpdatePlanKind::Native { command } => {
            vec![run_native_update_step(
                component.step,
                &command,
                context.workspace_root,
                context.dest_dir,
            )]
        }
    };

    persist_update_rows(&mut rows, agent, context.state, context.log_base)?;
    let failed = rows.iter().find(|row| row.status != "ran");
    Ok(AgentUpdateStepReport {
        step: component.step.to_owned(),
        status: if failed.is_some() {
            AgentUpdateStepStatus::Failed
        } else {
            AgentUpdateStepStatus::Updated
        },
        method: Some(plan.method.to_owned()),
        installed,
        latest: plan.latest,
        message: failed
            .map(|row| row.stderr.clone())
            .filter(|value| !value.is_empty()),
    })
}

fn persist_update_rows(
    rows: &mut [crate::runtime::install::agent_installer::InstallerRowDraft],
    agent: &AgentConfig,
    state: &StateStore,
    log_base: Option<&Path>,
) -> Result<()> {
    for row in rows.iter_mut() {
        persist_step_logs_to_disk(row, &agent.id, log_base)?;
    }
    for row in rows {
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
            operation: INSTALLER_OPERATION_UPDATE,
            method: row.method.as_deref(),
            log_dir: row.log_dir.as_deref(),
            apply_run_id: None,
        })?;
    }
    Ok(())
}

struct UpdateComponent<'a> {
    step: &'static str,
    field: &'static str,
    command_id: &'a str,
    install: &'a InstallSet,
    apt: Option<&'a AptUpdate>,
    github_url: Option<&'a str>,
}

fn update_components(entry: &RegistryEntry) -> Result<Vec<UpdateComponent<'_>>> {
    let harness = entry
        .harness
        .as_ref()
        .ok_or_else(|| StackError::RegistryLoad {
            reason: format!("registry entry `{}` has no harness block", entry.id),
        })?;
    if entry.kind == RegistryKind::Adapter {
        let adapter = entry
            .adapter
            .as_ref()
            .ok_or_else(|| StackError::RegistryLoad {
                reason: format!("registry entry `{}` has no adapter block", entry.id),
            })?;
        let mut components = Vec::new();
        if !harness.install.is_provided_by_adapter() {
            components.push(harness_component(entry, harness, STEP_HARNESS));
        }
        components.push(adapter_component(entry, adapter));
        return Ok(components);
    }
    Ok(vec![harness_component(entry, harness, STEP_INSTALL)])
}

fn harness_component<'a>(
    entry: &'a RegistryEntry,
    harness: &'a HarnessSpec,
    step: &'static str,
) -> UpdateComponent<'a> {
    UpdateComponent {
        step,
        field: "harness.update",
        command_id: &harness.id,
        install: &harness.install,
        apt: harness.update.apt.as_ref(),
        github_url: entry.github.as_deref(),
    }
}

fn adapter_component<'a>(
    entry: &'a RegistryEntry,
    adapter: &'a AdapterSpec,
) -> UpdateComponent<'a> {
    UpdateComponent {
        step: STEP_ADAPTER,
        field: "adapter.update",
        command_id: &adapter.id,
        install: &adapter.install,
        apt: adapter.update.apt.as_ref(),
        github_url: adapter.github.as_deref().or(entry.github.as_deref()),
    }
}

struct UpdatePlan {
    method: &'static str,
    latest: Option<String>,
    install: InstallSet,
    kind: UpdatePlanKind,
}

enum UpdatePlanKind {
    InstallSet,
    Apt(AptUpdate),
    Native { command: String },
}

fn choose_update_plan(
    entry: &RegistryEntry,
    component: &UpdateComponent<'_>,
    installed_row: Option<&InstallerRun>,
) -> Result<UpdatePlan> {
    match installed_row.and_then(|row| row.method.as_deref()) {
        Some(INSTALLER_METHOD_GITHUB) if component.install.github.is_some() => {
            return github_plan(entry, component);
        }
        Some(INSTALLER_METHOD_NPM) if component.install.npm.is_some() => {
            return npm_plan(component);
        }
        Some(INSTALLER_METHOD_APT) => {
            if let Some(apt) = component.apt {
                return Ok(apt_plan(apt));
            }
        }
        Some(INSTALLER_METHOD_SHELL) => {
            return Ok(native_plan_with_command(
                component
                    .install
                    .shell
                    .as_ref()
                    .map(|shell| shell.creates.clone())
                    .unwrap_or_else(|| native_probe_target(component)),
            ));
        }
        Some(INSTALLER_METHOD_NATIVE) => return Ok(native_plan(component)),
        Some(_) | None => {}
    }
    if let Some(apt) = component.apt {
        return Ok(apt_plan(apt));
    }
    if component.install.npm.is_some() {
        return npm_plan(component);
    }
    if component.install.github.is_some() {
        return github_plan(entry, component);
    }
    Ok(native_plan(component))
}

fn apt_plan(apt: &AptUpdate) -> UpdatePlan {
    UpdatePlan {
        method: INSTALLER_METHOD_APT,
        latest: None,
        install: InstallSet::default(),
        kind: UpdatePlanKind::Apt(apt.clone()),
    }
}

fn native_plan(component: &UpdateComponent<'_>) -> UpdatePlan {
    native_plan_with_command(native_probe_target(component))
}

fn native_plan_with_command(command: String) -> UpdatePlan {
    UpdatePlan {
        method: INSTALLER_METHOD_NATIVE,
        latest: None,
        install: InstallSet::default(),
        kind: UpdatePlanKind::Native { command },
    }
}

fn npm_plan(component: &UpdateComponent<'_>) -> Result<UpdatePlan> {
    let npm = component.install.npm.as_ref().expect("checked by caller");
    let latest = crate::runtime::install::npm_registry::latest_version(&npm.package)?;
    Ok(UpdatePlan {
        method: INSTALLER_METHOD_NPM,
        latest: Some(latest),
        install: InstallSet {
            npm: Some(npm.clone()),
            ..InstallSet::default()
        },
        kind: UpdatePlanKind::InstallSet,
    })
}

fn github_plan(entry: &RegistryEntry, component: &UpdateComponent<'_>) -> Result<UpdatePlan> {
    let github = component
        .install
        .github
        .as_ref()
        .expect("checked by caller");
    let github_url = component
        .github_url
        .ok_or_else(|| StackError::RegistryLoad {
            reason: format!(
                "agent `{}` {}.github requires github URL",
                entry.id, component.field
            ),
        })?;
    let repo = github_repo_from_url(&entry.id, "github", github_url)?;
    let latest = crate::runtime::install::github_release::latest_release_tag(&repo)?;
    Ok(UpdatePlan {
        method: INSTALLER_METHOD_GITHUB,
        latest: Some(latest),
        install: InstallSet {
            github: Some(github.clone()),
            ..InstallSet::default()
        },
        kind: UpdatePlanKind::InstallSet,
    })
}

fn native_probe_target(component: &UpdateComponent<'_>) -> String {
    component
        .install
        .npm
        .as_ref()
        .map(|npm| npm.creates.clone())
        .or_else(|| {
            component
                .install
                .github
                .as_ref()
                .map(|github| github.binary_name.clone())
        })
        .or_else(|| {
            component
                .install
                .shell
                .as_ref()
                .map(|shell| shell.creates.clone())
        })
        .unwrap_or_else(|| component.command_id.to_owned())
}

fn run_apt_update_step(
    step: &'static str,
    apt: AptUpdate,
    workspace_root: &Path,
    dest_dir: &Path,
) -> crate::runtime::install::agent_installer::InstallerRowDraft {
    let args = ["install", "--only-upgrade", "-y", apt.package.as_str()];
    run_command_step(
        step,
        INSTALL_METHOD_APT,
        "apt-get",
        &args,
        workspace_root,
        dest_dir,
        UPDATE_COMMAND_TIMEOUT,
    )
}

fn run_native_update_step(
    step: &'static str,
    command: &str,
    workspace_root: &Path,
    dest_dir: &Path,
) -> crate::runtime::install::agent_installer::InstallerRowDraft {
    let started_at = crate::runtime::install::agent_installer::current_timestamp();
    let Some(path) = resolve_creates(command, workspace_root, &[dest_dir]) else {
        return command_error_row(
            step,
            INSTALL_METHOD_NATIVE,
            started_at,
            format!("native update command `{command}` did not resolve"),
        );
    };
    let subcommand = match probe_native_update_subcommand(&path, workspace_root, dest_dir) {
        Ok(subcommand) => subcommand,
        Err(failure) => {
            let headline = if failure.command_ran {
                "did not advertise update or upgrade"
            } else {
                "could not be probed for update or upgrade"
            };
            return command_error_row(
                step,
                INSTALL_METHOD_NATIVE,
                started_at,
                format!(
                    "native update command `{command}` {headline} ({})",
                    failure.detail
                ),
            );
        }
    };
    let context = CommandStepContext {
        workspace_root,
        dest_dir,
        timeout: UPDATE_COMMAND_TIMEOUT,
    };
    run_command_step_with_started_at(
        step,
        INSTALL_METHOD_NATIVE,
        started_at,
        path,
        &[subcommand.as_str()],
        &context,
    )
}

// Distinguishes "the command ran and its help lacks the token" from "the
// probe never executed" (spawn error, timeout), so the report can avoid
// asserting anything about a help listing that was never seen.
struct NativeProbeFailure {
    command_ran: bool,
    detail: String,
}

fn probe_native_update_subcommand(
    path: &Path,
    workspace_root: &Path,
    dest_dir: &Path,
) -> std::result::Result<String, NativeProbeFailure> {
    let context = CommandStepContext {
        workspace_root,
        dest_dir,
        timeout: HELP_PROBE_TIMEOUT,
    };
    let mut command_ran = false;
    let mut failures = Vec::new();
    for args in [&["--help"][..], &["help"][..]] {
        let run_probe = || {
            run_command_step_with_started_at(
                STEP_INSTALL,
                INSTALL_METHOD_NATIVE,
                crate::runtime::install::agent_installer::current_timestamp(),
                path.to_path_buf(),
                args,
                &context,
            )
        };
        let mut row = run_probe();
        if let Some(subcommand) = advertised_subcommand(&row) {
            return Ok(subcommand);
        }
        // A spawn error or a timeout with no output means the child stalled
        // between fork and exec (a known hazard of pre_exec-based spawns in a
        // heavily threaded process) rather than the command lacking the
        // subcommand. That stall is transient, so one fresh spawn is retried.
        if row.status == "error" || row.status == "timeout" {
            std::thread::sleep(HELP_PROBE_RETRY_DELAY);
            row = run_probe();
            if let Some(subcommand) = advertised_subcommand(&row) {
                return Ok(subcommand);
            }
        }
        // "ran" and "failed" both mean the command itself executed; "error"
        // and "timeout" mean the probe never got a real help listing.
        command_ran |= row.status == "ran" || row.status == "failed";
        failures.push(probe_failure_detail(args[0], &row));
    }
    Err(NativeProbeFailure {
        command_ran,
        detail: failures.join("; "),
    })
}

fn advertised_subcommand(
    row: &crate::runtime::install::agent_installer::InstallerRowDraft,
) -> Option<String> {
    let output = format!("{}\n{}", row.stdout, row.stderr);
    NATIVE_UPDATE_COMMANDS
        .iter()
        .find(|candidate| help_output_contains_command(&output, candidate))
        .map(|candidate| (*candidate).to_owned())
}

// A probe can fail because the command genuinely lacks an update subcommand or
// because the probe itself could not run (spawn error, timeout). Keep the row's
// status/exit/output in the detail so the two cases stay distinguishable in
// the report.
fn probe_failure_detail(
    probe_arg: &str,
    row: &crate::runtime::install::agent_installer::InstallerRowDraft,
) -> String {
    let exit = match row.exit_status {
        Some(code) => code.to_string(),
        None => "none".to_owned(),
    };
    let combined = format!("{} {}", row.stdout, row.stderr);
    let flattened = combined.split_whitespace().collect::<Vec<_>>().join(" ");
    let output = if flattened.is_empty() {
        "no output".to_owned()
    } else {
        output_tail(&flattened, PROBE_FAILURE_OUTPUT_TAIL_BYTES).to_owned()
    };
    format!(
        "`{probe_arg}`: status {}, exit {exit}, output: {output}",
        row.status
    )
}

fn output_tail(text: &str, cap: usize) -> &str {
    if text.len() <= cap {
        return text;
    }
    let mut start = text.len() - cap;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

fn help_output_contains_command(output: &str, command: &str) -> bool {
    output
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'))
        .any(|token| token == command)
}

fn run_command_step(
    step: &'static str,
    method: &'static str,
    program: &str,
    args: &[&str],
    workspace_root: &Path,
    dest_dir: &Path,
    timeout: Duration,
) -> crate::runtime::install::agent_installer::InstallerRowDraft {
    let started_at = crate::runtime::install::agent_installer::current_timestamp();
    let context = CommandStepContext {
        workspace_root,
        dest_dir,
        timeout,
    };
    run_command_step_with_started_at(
        step,
        method,
        started_at,
        PathBuf::from(program),
        args,
        &context,
    )
}

struct CommandStepContext<'a> {
    workspace_root: &'a Path,
    dest_dir: &'a Path,
    timeout: Duration,
}

fn run_command_step_with_started_at(
    step: &'static str,
    method: &'static str,
    started_at: String,
    program: PathBuf,
    args: &[&str],
    context: &CommandStepContext<'_>,
) -> crate::runtime::install::agent_installer::InstallerRowDraft {
    let mut command = Command::new(program);
    command.args(args);
    command.current_dir(context.workspace_root);
    command.env_clear();
    forward_host_env(&mut command, "HOME");
    forward_host_env(&mut command, "LANG");
    if let Some(path) = path_env_with_extra_dirs(&[context.dest_dir]) {
        command.env("PATH", path);
    }
    apply_non_interactive_env(&mut command);
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    // Detach so a native updater (e.g. `pi update`) probing the terminal
    // cannot prompt-and-block the daemon's uncancellable update task.
    detach_into_new_session(&mut command);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            return command_error_row(step, method, started_at, err.to_string());
        }
    };
    let stdout = child
        .stdout
        .take()
        .map(|stream| spawn_capped_reader(stream, INSTALLER_OUTPUT_CAP_BYTES));
    let stderr = child
        .stderr
        .take()
        .map(|stream| spawn_capped_reader(stream, INSTALLER_OUTPUT_CAP_BYTES));
    let deadline = Instant::now() + context.timeout;
    let status = match wait_with_timeout(&mut child, deadline) {
        Ok(Some(status)) => status,
        Ok(None) => {
            kill_process_group(&mut child);
            let stdout = stdout.and_then(join_reader_bounded).unwrap_or_default();
            let stderr = stderr.and_then(join_reader_bounded).unwrap_or_default();
            return crate::runtime::install::agent_installer::InstallerRowDraft {
                started_at,
                finished_at: Some(crate::runtime::install::agent_installer::current_timestamp()),
                status: "timeout".to_owned(),
                stdout,
                stderr,
                exit_status: None,
                step: step.to_owned(),
                method: Some(method.to_owned()),
                version: None,
                log_dir: None,
            };
        }
        Err(err) => {
            kill_process_group(&mut child);
            return command_error_row(step, method, started_at, err.to_string());
        }
    };
    // Reap any grandchildren that inherited the pipes before joining the reader
    // threads, so a command that backgrounds a child can't leave the readers
    // blocked on EOF — the same hardening the installer applies on success.
    kill_process_group(&mut child);
    let stdout = stdout.and_then(join_reader_bounded).unwrap_or_default();
    let stderr = stderr.and_then(join_reader_bounded).unwrap_or_default();
    crate::runtime::install::agent_installer::InstallerRowDraft {
        started_at,
        finished_at: Some(crate::runtime::install::agent_installer::current_timestamp()),
        status: if status.success() { "ran" } else { "failed" }.to_owned(),
        stdout,
        stderr,
        exit_status: status.code(),
        step: step.to_owned(),
        method: Some(method.to_owned()),
        version: None,
        log_dir: None,
    }
}

fn command_error_row(
    step: &'static str,
    method: &'static str,
    started_at: String,
    stderr: String,
) -> crate::runtime::install::agent_installer::InstallerRowDraft {
    crate::runtime::install::agent_installer::InstallerRowDraft {
        started_at,
        finished_at: Some(crate::runtime::install::agent_installer::current_timestamp()),
        status: "error".to_owned(),
        stdout: String::new(),
        stderr,
        exit_status: None,
        step: step.to_owned(),
        method: Some(method.to_owned()),
        version: None,
        log_dir: None,
    }
}

fn normalize_version(value: &str) -> &str {
    value
        .trim()
        .strip_prefix('v')
        .unwrap_or_else(|| value.trim())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::{
        AgentUpdateOptions, UpdateComponent, UpdatePlanKind, choose_update_plan,
        help_output_contains_command, update_agent_for_config, update_components,
    };
    use crate::runtime::install::agent_registry::{RegistryCatalog, RegistryEntry};
    use crate::state::{
        INSTALLER_METHOD_APT, INSTALLER_METHOD_NATIVE, INSTALLER_METHOD_SHELL,
        INSTALLER_OPERATION_INSTALL, INSTALLER_OPERATION_UPDATE, InstallerRun, StateStore,
    };

    #[cfg(unix)]
    #[test]
    fn command_step_runs_with_null_stdin() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        // `cat` with inherited stdin would block until the daemon's stdin
        // closes (the pre-fix behavior for `pi update`); with a null stdin it
        // sees immediate EOF and exits 0 within the timeout.
        let row = super::run_command_step_with_started_at(
            "harness",
            "native",
            crate::runtime::install::agent_installer::current_timestamp(),
            std::path::PathBuf::from("sh"),
            &["-c", "cat"],
            &super::CommandStepContext {
                workspace_root: tempdir.path(),
                dest_dir: tempdir.path(),
                timeout: std::time::Duration::from_secs(5),
            },
        );
        assert_eq!(row.status, "ran");
        assert_eq!(row.exit_status, Some(0));
    }

    #[test]
    fn native_help_probe_matches_exact_subcommand_tokens() {
        assert!(help_output_contains_command(
            "Commands:\n  update\n",
            "update"
        ));
        assert!(help_output_contains_command("upgrade agent", "upgrade"));
        assert!(!help_output_contains_command("self-update", "update"));
        assert!(!help_output_contains_command("updated", "update"));
    }

    #[test]
    fn update_plan_preserves_shell_install_as_native_update() {
        let registry = registry_with_shell_npm_and_apt();
        let entry = registry.lookup_required("fake").expect("entry");
        let component = harness_update_component(entry);
        let installed = installer_run_with_method(Some(INSTALLER_METHOD_SHELL));

        let plan = choose_update_plan(entry, &component, Some(&installed)).expect("plan");

        assert_eq!(plan.method, INSTALLER_METHOD_NATIVE);
        match plan.kind {
            UpdatePlanKind::Native { command } => assert_eq!(command, "shell-agent"),
            _ => panic!("expected native update plan"),
        }
    }

    #[test]
    fn update_components_skip_adapter_provided_harness() {
        let catalog = RegistryCatalog::from_toml(
            r#"
[[agents]]
id = "sdk-backed"
name = "SDK Backed"
kind = "adapter"
headless_compatible = true
support_doc = "docs/agents/sdk-backed.md"

[agents.adapter]
id = "sdk-backed-acp"

[agents.adapter.install.npm]
package = "sdk-backed-acp"
creates = "sdk-backed-acp"

[agents.harness]
id = "sdk-agent-sdk"

[agents.harness.install]
provided_by = "adapter"
"#,
        )
        .expect("registry");
        let entry = catalog.lookup_required("sdk-backed").expect("entry");

        let components = update_components(entry).expect("components");

        assert_eq!(components.len(), 1);
        assert_eq!(components[0].step, "adapter");
        assert_eq!(components[0].command_id, "sdk-backed-acp");
    }

    #[test]
    fn update_plan_uses_explicit_apt_metadata_before_derived_sources() {
        let registry = registry_with_shell_npm_and_apt();
        let entry = registry.lookup_required("fake").expect("entry");
        let component = harness_update_component(entry);
        let installed = installer_run_with_method(None);

        let plan = choose_update_plan(entry, &component, Some(&installed)).expect("plan");

        assert_eq!(plan.method, INSTALLER_METHOD_APT);
        match plan.kind {
            UpdatePlanKind::Apt(apt) => assert_eq!(apt.package, "fake-agent"),
            _ => panic!("expected apt update plan"),
        }
    }

    #[test]
    fn native_update_runs_detected_update_subcommand() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let workspace = tempdir.path().join("workspace");
        let dest = tempdir.path().join("bin");
        fs::create_dir_all(&workspace).expect("workspace");
        fs::create_dir_all(&dest).expect("dest");
        let marker = workspace.join("updated.txt");
        let command_path = dest.join("fake-agent");
        fs::write(
            &command_path,
            format!(
                "#!/bin/sh\nif [ \"$1\" = \"--help\" ]; then echo 'Commands: update'; exit 0; fi\nif [ \"$1\" = \"update\" ]; then touch {}; exit 0; fi\nexit 2\n",
                marker.display()
            ),
        )
        .expect("fake command");
        let mut permissions = fs::metadata(&command_path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&command_path, permissions).expect("chmod");

        let config_text = format!(
            r#"
config_version = 1

[api]
bind = "127.0.0.1:7700"
max_request_bytes = 1048576

[security.http]
max_request_bytes = 1048576
rate_limit_per_minute = 60
burst = 10
auth_failures_per_minute = 5
auth_block_duration = "5m"
trust_proxy_headers = false

[workspace]
root = "{}"
uploads = "{}/uploads"
default_shell = "/bin/sh"
runtime_user = "acp"
max_file_bytes = 1048576

[logging]
level = "info"
local_retention_days = 7

[agent]
id = "fake"
name = "Fake"
command = "fake-agent"
args = []
restart = "never"
"#,
            workspace.display(),
            workspace.display()
        );
        let config = crate::config::load_config_from_str(&config_text).expect("config");
        let registry = RegistryCatalog::from_toml(
            r#"
[[agents]]
id = "fake"
name = "Fake"
kind = "native"
headless_compatible = true
support_doc = "docs/agents/fake.md"

[agents.harness]
id = "fake-agent"

[agents.harness.install.shell]
script = "true"
creates = "fake-agent"
"#,
        )
        .expect("registry");
        let entry = registry.lookup_required("fake").expect("entry");
        let state = StateStore::open(tempdir.path().join("state.sqlite")).expect("state");
        state.migrate().expect("migrate");

        let report = update_agent_for_config(
            &config,
            entry,
            &state,
            &workspace,
            &dest,
            None,
            AgentUpdateOptions::default(),
        )
        .expect("update");
        assert!(report.updated, "{report:?}");
        assert!(marker.exists());
        let rows = state
            .latest_successful_installer_runs_for_agent("fake")
            .expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].operation, INSTALLER_OPERATION_UPDATE);
        assert_eq!(rows[0].method.as_deref(), Some(INSTALLER_METHOD_NATIVE));
    }

    #[test]
    fn native_probe_failure_detail_includes_status_exit_and_output() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let command_path = tempdir.path().join("fake-agent");
        fs::write(
            &command_path,
            "#!/bin/sh\necho 'Commands: doctor'\nexit 3\n",
        )
        .expect("fake command");
        let mut permissions = fs::metadata(&command_path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&command_path, permissions).expect("chmod");

        let failure =
            super::probe_native_update_subcommand(&command_path, tempdir.path(), tempdir.path())
                .expect_err("probe should fail");

        assert!(failure.command_ran);
        let detail = failure.detail;
        assert!(detail.contains("`--help`"), "{detail}");
        assert!(detail.contains("`help`"), "{detail}");
        assert!(detail.contains("status failed"), "{detail}");
        assert!(detail.contains("exit 3"), "{detail}");
        assert!(detail.contains("Commands: doctor"), "{detail}");
    }

    #[test]
    fn native_probe_spawn_error_is_visible_in_detail() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let command_path = tempdir.path().join("fake-agent");
        // Not executable, so the spawn itself fails rather than the command.
        fs::write(&command_path, "#!/bin/sh\nexit 0\n").expect("fake command");

        let failure =
            super::probe_native_update_subcommand(&command_path, tempdir.path(), tempdir.path())
                .expect_err("probe should fail");

        assert!(!failure.command_ran);
        let detail = failure.detail;
        assert!(detail.contains("status error"), "{detail}");
        assert!(detail.contains("exit none"), "{detail}");
    }

    fn registry_with_shell_npm_and_apt() -> RegistryCatalog {
        RegistryCatalog::from_toml(
            r#"
[[agents]]
id = "fake"
name = "Fake"
kind = "native"
headless_compatible = true
support_doc = "docs/agents/fake.md"

[agents.harness]
id = "fake-agent"

[agents.harness.install.shell]
script = "true"
creates = "shell-agent"

[agents.harness.install.npm]
package = "@example/fake-agent"
creates = "npm-agent"

[agents.harness.update.apt]
package = "fake-agent"
"#,
        )
        .expect("registry")
    }

    fn harness_update_component(entry: &RegistryEntry) -> UpdateComponent<'_> {
        let harness = entry.harness.as_ref().expect("harness");
        UpdateComponent {
            step: "install",
            field: "harness.update",
            command_id: &harness.id,
            install: &harness.install,
            apt: harness.update.apt.as_ref(),
            github_url: entry.github.as_deref(),
        }
    }

    fn installer_run_with_method(method: Option<&str>) -> InstallerRun {
        InstallerRun {
            id: "run".to_owned(),
            agent_id: Some("fake".to_owned()),
            started_at: "2026-01-01T00:00:00Z".to_owned(),
            finished_at: Some("2026-01-01T00:00:01Z".to_owned()),
            status: "ran".to_owned(),
            stdout: String::new(),
            stderr: String::new(),
            exit_status: Some(0),
            step: "install".to_owned(),
            version: None,
            operation: INSTALLER_OPERATION_INSTALL.to_owned(),
            method: method.map(str::to_owned),
            log_dir: None,
            apply_run_id: None,
        }
    }
}
