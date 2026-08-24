use crate::common::cli::*;
use acp_stack::config::{StackUpdatePolicy, load_config_from_str};
use std::fs;

#[test]
fn init_stack_update_off_sets_manual_policy() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--stack-update",
            "off",
            "--stack-update-frequency",
            "6m",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    let config = load_config_from_str(&written).expect("config should validate");
    assert_eq!(config.updates.acp_stack.policy, StackUpdatePolicy::Manual);
    assert_eq!(config.updates.acp_stack.frequency, "1d");
}

#[test]
fn init_stack_update_on_writes_compatible_policy_and_frequency() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--stack-update",
            "on",
            "--stack-update-frequency",
            "3w",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    let config = load_config_from_str(&written).expect("config should validate");
    assert_eq!(
        config.updates.acp_stack.policy,
        StackUpdatePolicy::Compatible
    );
    assert_eq!(config.updates.acp_stack.frequency, "3w");
}

#[test]
fn init_stack_update_rejects_sub_day_frequency() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--stack-update",
            "security",
            "--stack-update-frequency",
            "6m",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("day (d) or week (w)"));

    assert!(
        !tempdir
            .path()
            .join(".config/acp-stack/acps-config.toml")
            .exists(),
        "invalid stack-update frequency must fail before config creation"
    );
}

#[test]
fn init_stack_update_rejects_invalid_policy_before_config_creation() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--stack-update",
            "securty",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("on|security|off"));

    assert!(
        !tempdir
            .path()
            .join(".config/acp-stack/acps-config.toml")
            .exists(),
        "invalid stack-update policy must fail before config creation"
    );
}

#[test]
fn init_stack_update_existing_config_preserves_policy_without_flags() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--stack-update",
            "on",
            "--stack-update-frequency",
            "3w",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    acps_command()
        .env("HOME", tempdir.path())
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

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    let config = load_config_from_str(&written).expect("config should validate");
    assert_eq!(
        config.updates.acp_stack.policy,
        StackUpdatePolicy::Compatible
    );
    assert_eq!(config.updates.acp_stack.frequency, "3w");
}

#[test]
fn init_stack_update_default_preserved_non_interactive() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
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

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    let config = load_config_from_str(&written).expect("config should validate");
    // No --stack-update flag and non-interactive: the schema defaults are untouched.
    assert_eq!(
        config.updates.acp_stack.policy,
        StackUpdatePolicy::SecurityCritical
    );
    assert_eq!(config.updates.acp_stack.frequency, "1d");
}

#[test]
fn init_agent_update_off_disables_auto_update() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--agent-update",
            "off",
            // A frequency is accepted but ignored when auto-update is off.
            "--agent-update-frequency",
            "6m",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    let config = load_config_from_str(&written).expect("config should validate");
    let auto_update = config
        .agent
        .auto_update
        .as_ref()
        .expect("registry agent keeps an [agent.auto_update] block");
    assert!(!auto_update.enabled);
    assert_eq!(auto_update.frequency, "1d");
}

#[test]
fn init_agent_update_on_writes_enabled_and_frequency() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--agent-update",
            "on",
            "--agent-update-frequency",
            "3w",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    let config = load_config_from_str(&written).expect("config should validate");
    let auto_update = config
        .agent
        .auto_update
        .as_ref()
        .expect("registry agent keeps an [agent.auto_update] block");
    assert!(auto_update.enabled);
    assert_eq!(auto_update.frequency, "3w");
}

#[test]
fn init_agent_update_on_accepts_hourly_frequency() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    // Managed agent updates accept hour granularity, unlike the stack self-update's
    // day minimum.
    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--agent-update",
            "on",
            "--agent-update-frequency",
            "12h",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    let config = load_config_from_str(&written).expect("config should validate");
    let auto_update = config
        .agent
        .auto_update
        .as_ref()
        .expect("registry agent keeps an [agent.auto_update] block");
    assert!(auto_update.enabled);
    assert_eq!(auto_update.frequency, "12h");
}

#[test]
fn init_agent_update_rejects_sub_hour_frequency() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--agent-update",
            "on",
            "--agent-update-frequency",
            "6m",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("minimum granularity is an hour"));

    assert!(
        !tempdir
            .path()
            .join(".config/acp-stack/acps-config.toml")
            .exists(),
        "invalid agent-update frequency must fail before config creation"
    );
}

#[test]
fn init_agent_update_rejects_invalid_choice_before_config_creation() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--agent-update",
            "enabled",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("on|off"));

    assert!(
        !tempdir
            .path()
            .join(".config/acp-stack/acps-config.toml")
            .exists(),
        "invalid agent-update choice must fail before config creation"
    );
}

#[test]
fn init_agent_update_default_enabled_non_interactive() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
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

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    let config = load_config_from_str(&written).expect("config should validate");
    let auto_update = config
        .agent
        .auto_update
        .as_ref()
        .expect("registry agent defaults to an [agent.auto_update] block");
    assert!(auto_update.enabled);
    assert_eq!(auto_update.frequency, "1d");
}

#[test]
fn init_agent_update_on_rejected_for_custom_agent() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    // Custom agents carry no [agent.auto_update] block because the managed updater
    // cannot drive them.
    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--custom-agent-id",
            "my-agent",
            "--custom-agent-name",
            "My Agent",
            "--custom-agent-command",
            "my-agent-bin",
            "--custom-agent-arg",
            "acp",
            "--custom-agent-install",
            "echo install my-agent",
            "--custom-agent-creates",
            "my-agent-bin",
            "--agent-update",
            "on",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("managed registry agent"));
}

#[test]
fn init_agent_update_off_noop_for_custom_agent() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--custom-agent-id",
            "my-agent",
            "--custom-agent-command",
            "my-agent-bin",
            "--custom-agent-install",
            "echo install my-agent",
            "--custom-agent-creates",
            "my-agent-bin",
            "--agent-update",
            "off",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    let config = load_config_from_str(&written).expect("config should validate");
    assert!(
        config.agent.auto_update.is_none(),
        "custom agent must not get an [agent.auto_update] block"
    );
}

#[test]
fn init_agent_update_off_strips_stale_block_for_custom_agent() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_path = tempdir.path().join(".config/acp-stack/acps-config.toml");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--custom-agent-id",
            "my-agent",
            "--custom-agent-command",
            "my-agent-bin",
            "--custom-agent-install",
            "echo install my-agent",
            "--custom-agent-creates",
            "my-agent-bin",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    // A hand-edited config may carry an auto_update block even for a custom agent;
    // `--agent-update off` must strip it rather than leave a recurring skip.
    let written = fs::read_to_string(&config_path).expect("config should be readable");
    fs::write(
        &config_path,
        format!(
            "{written}\n[array.targets.agent.auto_update]\nenabled = true\nfrequency = \"1d\"\n"
        ),
    )
    .expect("config should be writable");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent-update",
            "off",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    let written = fs::read_to_string(&config_path).expect("config should be readable");
    let config = load_config_from_str(&written).expect("config should validate");
    assert!(
        config.agent.auto_update.is_none(),
        "--agent-update off must strip a stale [agent.auto_update] block"
    );
}

#[test]
fn init_agent_update_seeds_block_for_registry_config_missing_it() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_path = tempdir.path().join(".config/acp-stack/acps-config.toml");

    acps_command()
        .env("HOME", tempdir.path())
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

    // A registry agent with no [agent.auto_update] block: a bare re-run never re-seeds.
    let written = fs::read_to_string(&config_path).expect("config should be readable");
    let mut skipping = false;
    let mut stripped = String::new();
    for line in written.lines() {
        if line.trim() == "[agent.auto_update]" {
            skipping = true;
            continue;
        }
        if skipping {
            if line.trim_start().starts_with('[') {
                skipping = false;
            } else {
                continue;
            }
        }
        stripped.push_str(line);
        stripped.push('\n');
    }
    assert!(
        !stripped.contains("[agent.auto_update]"),
        "the block should have been stripped"
    );
    load_config_from_str(&stripped).expect("stripped config should still validate");
    fs::write(&config_path, &stripped).expect("stripped config should be writable");

    // `--agent-update on` must seed the block, not reject it as an escape-hatch install.
    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent-update",
            "on",
            "--agent-update-frequency",
            "2w",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    let written = fs::read_to_string(&config_path).expect("config should be readable");
    let config = load_config_from_str(&written).expect("config should validate");
    let auto_update = config
        .agent
        .auto_update
        .as_ref()
        .expect("managed registry agent must get an [agent.auto_update] block");
    assert!(auto_update.enabled);
    assert_eq!(auto_update.frequency, "2w");
}
