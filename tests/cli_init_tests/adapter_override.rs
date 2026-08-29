use crate::common::cli::*;
use acp_stack::config::load_config_from_str;
use acp_stack::state::{StateStore, default_state_path};
use std::fs;

const CONFIG_RELATIVE_PATH: &str = ".config/acp-stack/acps-config.toml";

fn init_placebo_with_override(tempdir: &tempfile::TempDir) {
    acps_command(tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--adapter-override-command",
            "placebo-acp",
            "--adapter-override-arg",
            "--stdio",
            "--adapter-override-github",
            "example/placebo-acp",
            "--adapter-override-install-npm",
            "@example/placebo-acp",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();
}

fn written_config(tempdir: &tempfile::TempDir) -> acp_stack::config::Config {
    let written = fs::read_to_string(tempdir.path().join(CONFIG_RELATIVE_PATH))
        .expect("config should be readable");
    load_config_from_str(&written).expect("config should validate")
}

#[test]
fn init_capability_probe_records_the_override_adapter_id() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let fixture = tempdir.path().join("agent-capabilities.json");
    fs::write(
        &fixture,
        serde_json::json!({
            "protocol_version": 1,
            "capabilities": {},
            "agent_name": "placebo",
            "agent_title": null,
            "agent_version": null,
        })
        .to_string(),
    )
    .expect("capabilities fixture written");
    acps_command(tempdir.path())
        .env(
            acp_stack::dev_gates::FIXTURE_AGENT_CAPABILITIES_ENV,
            &fixture,
        )
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--adapter-override-command",
            "placebo-acp",
            "--adapter-override-github",
            "example/placebo-acp",
            "--adapter-override-install-npm",
            "@example/placebo-acp",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    let store = StateStore::open(default_state_path(tempdir.path())).expect("state store");
    let row = store
        .latest_agent_capabilities("placebo")
        .expect("capabilities query")
        .expect("probe persisted a capabilities row");
    let snapshot: serde_json::Value =
        serde_json::from_str(&row.capabilities_json).expect("snapshot parses");
    assert_eq!(snapshot["agent_id"], "placebo");
    assert_eq!(snapshot["adapter_id"], "placebo-acp");
}

#[test]
fn init_adapter_override_writes_block_and_launch_command() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    init_placebo_with_override(&tempdir);

    let config = written_config(&tempdir);
    assert_eq!(config.agent.id, "placebo");
    assert_eq!(config.agent.command, "placebo-acp");
    assert_eq!(config.agent.args, vec!["--stdio".to_owned()]);
    let override_config = config
        .agent
        .adapter_override
        .as_ref()
        .expect("override block written");
    assert_eq!(override_config.command, "placebo-acp");
    assert_eq!(
        override_config.github.as_deref(),
        Some("example/placebo-acp")
    );
    let npm = override_config
        .install
        .npm
        .as_ref()
        .expect("npm install variant written");
    assert_eq!(npm.package, "@example/placebo-acp");
    assert_eq!(npm.creates, "placebo-acp");
    assert!(config.agent.install.is_none());
}

#[test]
fn init_rerun_same_agent_preserves_adapter_override() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    init_placebo_with_override(&tempdir);

    acps_command(tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    let config = written_config(&tempdir);
    assert!(config.agent.adapter_override.is_some());
    assert_eq!(config.agent.command, "placebo-acp");
}

#[test]
fn init_agent_change_clears_adapter_override() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    init_placebo_with_override(&tempdir);
    seed_init_secrets(tempdir.path(), &[("OPENAI_API_KEY", "test-openai-key")]);

    acps_command(tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "opencode",
            "--provider",
            "openai",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    let config = written_config(&tempdir);
    assert!(config.agent.adapter_override.is_none());
    // Under the dev placebo registry every harness command is the placebo path.
    assert_eq!(config.agent.command, env!("CARGO_BIN_EXE_placebo-agent"));
}

#[test]
fn init_adapter_override_clear_removes_block() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    init_placebo_with_override(&tempdir);

    acps_command(tempdir.path())
        .args([
            "dev",
            "init",
            "--adapter-override-clear",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    let config = written_config(&tempdir);
    assert!(config.agent.adapter_override.is_none());
    assert_eq!(config.agent.command, env!("CARGO_BIN_EXE_placebo-agent"));
}

#[test]
fn init_adapter_override_applies_without_agent_reselection() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    acps_command(tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    acps_command(tempdir.path())
        .args([
            "dev",
            "init",
            "--adapter-override-command",
            "placebo-acp",
            "--adapter-override-install-npm",
            "@example/placebo-acp",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    let config = written_config(&tempdir);
    assert!(config.agent.adapter_override.is_some());
    assert_eq!(config.agent.command, "placebo-acp");
}

#[test]
fn init_adapter_override_clear_works_after_agent_leaves_the_registry() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    init_placebo_with_override(&tempdir);

    // With `placebo` absent from the embedded catalog, clearing the stale
    // designation must still persist even though the run then fails at install.
    acps_command_without_placebo(tempdir.path())
        .args([
            "dev",
            "init",
            "--adapter-override-clear",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "ACP registry does not contain agent `placebo`",
        ));

    let config = written_config(&tempdir);
    assert!(config.agent.adapter_override.is_none());
}

#[test]
fn init_adapter_override_requires_an_install_source() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command(tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--adapter-override-command",
            "placebo-acp",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--adapter-override-install-npm"));
}

#[test]
fn init_adapter_override_conflicts_with_custom_agent_flags() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command(tempdir.path())
        .args([
            "dev",
            "init",
            "--custom-agent-id",
            "my-agent",
            "--custom-agent-command",
            "my-agent",
            "--custom-agent-install",
            "echo x",
            "--adapter-override-command",
            "my-acp",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("cannot be used with"));
}

#[test]
fn init_adapter_override_rejects_custom_agent_config() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    acps_command(tempdir.path())
        .args([
            "dev",
            "init",
            "--custom-agent-id",
            "my-agent",
            "--custom-agent-command",
            "my-agent-bin",
            "--custom-agent-install",
            "echo install",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    acps_command(tempdir.path())
        .args([
            "dev",
            "init",
            "--adapter-override-command",
            "my-acp",
            "--adapter-override-install-npm",
            "my-acp",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("not a registry agent"));
}

#[test]
fn init_adapter_override_shell_variant_writes_shell_install() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command(tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--adapter-override-command",
            "placebo-acp",
            "--adapter-override-install-shell",
            "echo install placebo-acp",
            "--adapter-override-install-creates",
            "placebo-acp",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    let config = written_config(&tempdir);
    let override_config = config
        .agent
        .adapter_override
        .as_ref()
        .expect("override block written");
    let shell = override_config
        .install
        .shell
        .as_ref()
        .expect("shell install variant written");
    assert_eq!(shell.script, "echo install placebo-acp");
    assert_eq!(shell.creates, "placebo-acp");
    assert!(override_config.install.npm.is_none());
}
