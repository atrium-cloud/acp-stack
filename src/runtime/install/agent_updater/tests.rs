use std::fs;
use std::os::unix::fs::PermissionsExt;

use super::{
    AgentUpdateOptions, AgentUpdateStepStatus, UpdateComponent, UpdateExecutionContext,
    UpdatePlanKind, choose_update_plan, help_output_contains_command, update_agent_for_config,
    update_component, update_components,
};
use crate::runtime::install::agent_registry::{RegistryCatalog, RegistryEntry};
use crate::state::{
    INSTALLER_METHOD_APT, INSTALLER_METHOD_GITHUB, INSTALLER_METHOD_NATIVE, INSTALLER_METHOD_NPM,
    INSTALLER_METHOD_SHELL, INSTALLER_OPERATION_INSTALL, INSTALLER_OPERATION_UPDATE, InstallerRun,
    StateStore,
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
    let agent = agent_config("fake", None);
    let component = harness_update_component(entry, &agent);
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

    let agent = agent_config("sdk-backed", None);
    let components = update_components(entry, &agent).expect("components");

    assert_eq!(components.len(), 1);
    assert_eq!(components[0].step, "adapter");
    assert_eq!(components[0].command_id, "sdk-backed-acp");
}

#[test]
fn update_plan_uses_explicit_apt_metadata_before_derived_sources() {
    let registry = registry_with_shell_npm_and_apt();
    let entry = registry.lookup_required("fake").expect("entry");
    let agent = agent_config("fake", None);
    let component = harness_update_component(entry, &agent);
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

#[test]
fn update_plan_honors_harness_version_pin() {
    let registry = registry_with_github_and_npm();
    let entry = registry.lookup_required("fake").expect("entry");
    let agent = agent_config("fake", Some("v1.2.3"));
    let component = harness_update_component(entry, &agent);
    // Recorded npm install would normally pick the npm plan; the pin must
    // win. A network fetch here would fail the test (no mock server).
    let installed = installer_run_with_method(Some(INSTALLER_METHOD_NPM));

    let plan = choose_update_plan(entry, &component, Some(&installed)).expect("plan");

    assert_eq!(plan.method, INSTALLER_METHOD_GITHUB);
    assert_eq!(plan.latest.as_deref(), Some("v1.2.3"));
}

#[test]
fn update_plan_reports_up_to_date_at_pin() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let state = StateStore::open(tempdir.path().join("state.sqlite")).expect("state");
    state.migrate().expect("migrate");
    let registry = registry_with_github_and_npm();
    let entry = registry.lookup_required("fake").expect("entry");
    let agent = agent_config("fake", Some("v1.2.3"));
    let component = harness_update_component(entry, &agent);
    let mut installed = installer_run_with_method(Some(INSTALLER_METHOD_GITHUB));
    installed.version = Some("1.2.3".to_owned());
    let context = UpdateExecutionContext {
        workspace_root: tempdir.path(),
        dest_dir: tempdir.path(),
        state: &state,
        log_base: None,
        force: false,
    };

    let report =
        update_component(&agent, entry, &component, Some(&installed), &context).expect("report");

    assert_eq!(report.status, AgentUpdateStepStatus::UpToDate);
    assert_eq!(report.latest.as_deref(), Some("v1.2.3"));
    assert_eq!(report.installed.as_deref(), Some("1.2.3"));
}

#[test]
fn update_components_pin_applies_to_harness_not_adapter() {
    let catalog = RegistryCatalog::from_toml(
        r#"
[[agents]]
id = "pinned"
name = "Pinned"
kind = "adapter"
headless_compatible = true
support_doc = "docs/agents/pinned.md"
github = "https://github.com/example/pinned"

[agents.adapter]
id = "pinned-acp"

[agents.adapter.install.npm]
package = "pinned-acp"
creates = "pinned-acp"

[agents.harness]
id = "pinned-agent"

[agents.harness.install.github]
asset_pattern = "pinned-{arch}.tar.gz"
archive = "tar.gz"
binary_name = "pinned-agent"

[agents.harness.install.github.arch]
x86_64 = "x64"
aarch64 = "arm64"
"#,
    )
    .expect("registry");
    let entry = catalog.lookup_required("pinned").expect("entry");
    let agent = agent_config("pinned", Some("v9.9.9"));

    let components = update_components(entry, &agent).expect("components");

    assert_eq!(components.len(), 2);
    assert_eq!(components[0].step, "harness");
    assert_eq!(components[0].version_pin, Some("v9.9.9"));
    assert_eq!(components[1].step, "adapter");
    assert_eq!(components[1].version_pin, None);
}

fn registry_with_github_and_npm() -> RegistryCatalog {
    RegistryCatalog::from_toml(
        r#"
[[agents]]
id = "fake"
name = "Fake"
kind = "native"
headless_compatible = true
support_doc = "docs/agents/fake.md"
github = "https://github.com/example/fake"

[agents.harness]
id = "fake-agent"

[agents.harness.install.npm]
package = "@example/fake-agent"
creates = "fake-agent"

[agents.harness.install.github]
asset_pattern = "fake-{arch}.tar.gz"
archive = "tar.gz"
binary_name = "fake-agent"

[agents.harness.install.github.arch]
x86_64 = "x64"
aarch64 = "arm64"
"#,
    )
    .expect("registry")
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

fn harness_update_component<'a>(
    entry: &'a RegistryEntry,
    agent: &'a crate::config::AgentConfig,
) -> UpdateComponent<'a> {
    let harness = entry.harness.as_ref().expect("harness");
    UpdateComponent {
        step: "install",
        field: "harness.update",
        command_id: &harness.id,
        install: &harness.install,
        apt: harness.update.apt.as_ref(),
        github_url: entry.github.as_deref(),
        version_pin: agent.harness_version.as_deref(),
    }
}

fn agent_config(id: &str, harness_version: Option<&str>) -> crate::config::AgentConfig {
    crate::config::AgentConfig {
        id: id.to_owned(),
        name: id.to_owned(),
        command: id.to_owned(),
        args: Vec::new(),
        cwd: None,
        env: Vec::new(),
        expected_sha256: None,
        restart: "never".to_owned(),
        mode: None,
        model: None,
        harness_version: harness_version.map(str::to_owned),
        adapter: None,
        provider: None,
        providers: None,
        subagent: None,
        auto_update: None,
        install: None,
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
