use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::config::{AgentConfig, Config};
use crate::error::{Result, StackError};
#[cfg(unix)]
use std::os::unix::process::CommandExt;

use crate::runtime::install::agent_installer::{
    INSTALL_METHOD_APT, INSTALL_METHOD_NATIVE, STEP_ADAPTER, STEP_HARNESS, STEP_INSTALL,
    install_one_with_fallback, persist_step_logs_to_disk, resolve_creates,
};
use crate::runtime::install::agent_registry::{
    AdapterSpec, AptUpdate, HarnessSpec, InstallSet, RegistryEntry, RegistryKind,
    github_repo_from_url,
};
use crate::runtime::process_runner::{
    forward_host_env, join_reader_bounded, kill_process_group, path_env_with_extra_dirs,
    spawn_capped_reader, wait_with_timeout,
};
use crate::state::{
    INSTALLER_METHOD_APT, INSTALLER_METHOD_GITHUB, INSTALLER_METHOD_NATIVE, INSTALLER_METHOD_NPM,
    INSTALLER_METHOD_SHELL, INSTALLER_OPERATION_UPDATE, INSTALLER_OUTPUT_CAP_BYTES, InstallerRun,
    InstallerRunInput, StateStore,
};

const UPDATE_COMMAND_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const HELP_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const NATIVE_UPDATE_COMMANDS: &[&str] = &["update", "upgrade"];

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
        return Ok(vec![
            harness_component(entry, harness, STEP_HARNESS),
            adapter_component(entry, adapter),
        ]);
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
    let Some(subcommand) = probe_native_update_subcommand(&path, workspace_root, dest_dir) else {
        return command_error_row(
            step,
            INSTALL_METHOD_NATIVE,
            started_at,
            format!("native update command `{command}` did not advertise update or upgrade"),
        );
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

fn probe_native_update_subcommand(
    path: &Path,
    workspace_root: &Path,
    dest_dir: &Path,
) -> Option<String> {
    let context = CommandStepContext {
        workspace_root,
        dest_dir,
        timeout: HELP_PROBE_TIMEOUT,
    };
    for args in [&["--help"][..], &["help"][..]] {
        let row = run_command_step_with_started_at(
            STEP_INSTALL,
            INSTALL_METHOD_NATIVE,
            crate::runtime::install::agent_installer::current_timestamp(),
            path.to_path_buf(),
            args,
            &context,
        );
        let output = format!("{}\n{}", row.stdout, row.stderr);
        for candidate in NATIVE_UPDATE_COMMANDS {
            if help_output_contains_command(&output, candidate) {
                return Some((*candidate).to_owned());
            }
        }
    }
    None
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
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    #[cfg(unix)]
    {
        command.process_group(0);
    }
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
        help_output_contains_command, update_agent_for_config,
    };
    use crate::runtime::install::agent_registry::{RegistryCatalog, RegistryEntry};
    use crate::state::{
        INSTALLER_METHOD_APT, INSTALLER_METHOD_NATIVE, INSTALLER_METHOD_SHELL,
        INSTALLER_OPERATION_INSTALL, INSTALLER_OPERATION_UPDATE, InstallerRun, StateStore,
    };

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
