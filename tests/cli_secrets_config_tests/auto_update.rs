use crate::common::cli::*;
use acp_stack::config::load_config_from_str;
use std::fs;

#[test]
fn init_agent_flag_updates_config_non_interactively() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    seed_init_secrets(tempdir.path(), &[("KIMI_API_KEY", "test-kimi-key")]);

    acps_command(tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "kimi",
            "--provider",
            "kimi-code",
            "--skip-workspace-init",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent: Kimi Code (kimi)"));

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config readable");
    assert!(written.contains(r#"id = "kimi""#));
    assert!(written.contains(&format!(
        r#"command = "{}""#,
        env!("CARGO_BIN_EXE_placebo-agent")
    )));
    assert!(written.contains(r#""acp""#));
    assert!(written.contains(r#""--model-config-option""#));
    assert!(written.contains(r#""placebo-model""#));
    assert!(written.contains(r#"env = ["KIMI_API_KEY"]"#));
    assert!(written.contains("[array.targets.agent.auto_update]"));
    assert!(written.contains("enabled = true"));
    assert!(written.contains(r#"frequency = "1d""#));
    assert!(!written.contains("[array.targets.agent.install]"));
}

#[test]
fn agent_update_set_edits_auto_update_config() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    acps_command(tempdir.path())
        .args(["agent", "update", "set", "--auto-on", "--frequency", "3d"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent update auto: enabled"))
        .stdout(predicates::str::contains("frequency: 3d"));

    let config_text =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config readable");
    let config = load_config_from_str(&config_text).expect("config parses after update set");
    let auto_update = config.agent.auto_update.expect("auto-update written");
    assert!(auto_update.enabled);
    assert_eq!(auto_update.frequency, "3d");

    acps_command(tempdir.path())
        .args(["agent", "update", "set", "--auto-off"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent update auto: disabled"));

    let config_text =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config readable");
    let config = load_config_from_str(&config_text).expect("config parses after auto-off");
    let auto_update = config.agent.auto_update.expect("auto-update retained");
    assert!(!auto_update.enabled);
    assert_eq!(auto_update.frequency, "3d");
}

#[test]
fn agent_update_set_rejects_invalid_frequency() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    acps_command(tempdir.path())
        .args(["agent", "update", "set", "--frequency", "0d"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("agent.auto_update.frequency"));
}

#[test]
fn agent_update_set_accepts_sub_day_frequency() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    // Unlike the stack self-update's day minimum, agent updates accept hours.
    acps_command(tempdir.path())
        .args(["agent", "update", "set", "--frequency", "12h"])
        .assert()
        .success()
        .stdout(predicates::str::contains("frequency: 12h"));

    let config_text =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config readable");
    let config = load_config_from_str(&config_text).expect("config parses after update set");
    let auto_update = config.agent.auto_update.expect("auto-update written");
    assert_eq!(auto_update.frequency, "12h");
}

#[test]
fn agent_update_set_rejects_sub_hour_frequency() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    // Minutes are finer than the agent updater's smallest unit (an hour), so
    // they are rejected and the config is left untouched.
    acps_command(tempdir.path())
        .args(["agent", "update", "set", "--frequency", "30m"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("agent.auto_update.frequency"));

    let config_text =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config readable");
    assert!(
        !config_text.contains("[agent.auto_update]"),
        "a rejected set must not write an auto-update block"
    );
}

#[test]
fn stack_update_set_edits_update_config() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    acps_command(tempdir.path())
        .args([
            "update",
            "set",
            "--policy",
            "compatible",
            "--frequency",
            "3d",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "acp-stack update policy: compatible",
        ))
        .stdout(predicates::str::contains("frequency: 3d"));

    let config_text =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config readable");
    let config = load_config_from_str(&config_text).expect("config parses after update set");
    assert_eq!(
        config.updates.acp_stack.policy,
        acp_stack::config::StackUpdatePolicy::Compatible
    );
    assert_eq!(config.updates.acp_stack.frequency, "3d");
}

#[test]
fn stack_update_set_rejects_sub_day_frequency() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    acps_command(tempdir.path())
        .args(["update", "set", "--frequency", "12h"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("updates.acp_stack.frequency"));

    let config_text =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config readable");
    let config = load_config_from_str(&config_text).expect("config still parses after failed set");
    assert_eq!(config.updates.acp_stack.frequency, "1d");
}

#[test]
fn agent_update_set_rejects_non_registry_agent() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    // An unresolvable agent id has nothing to auto-update, so every `set` is
    // rejected; a written block would leave the daemon loop skipping forever.
    let escape_hatch = VALID_CONFIG.replace(r#"id = "opencode""#, r#"id = "custom-private-agent""#);
    fs::write(config_dir.join("acps-config.toml"), escape_hatch).expect("config should be written");

    for extra in [
        &["--auto-on"][..],
        &["--auto-off"][..],
        &["--frequency", "3d"][..],
    ] {
        acps_command(tempdir.path())
            .args(["agent", "update", "set"])
            .args(extra)
            .assert()
            .failure()
            .stderr(predicates::str::contains("not a managed registry agent"));
    }

    // The rejected run must not have written an [agent.auto_update] block.
    let config_text =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config readable");
    assert!(
        !config_text.contains("[agent.auto_update]"),
        "a rejected set must not write an auto-update block"
    );
}

#[test]
fn agent_check_and_update_still_error_for_placeholder_agent() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    // The legacy `placeholder` sentinel is NOT a custom agent: it keeps its own
    // "select a real agent" signal rather than degrading to the skip path.
    let placeholder = VALID_CONFIG.replace(r#"id = "opencode""#, r#"id = "placeholder""#);
    fs::write(config_dir.join("acps-config.toml"), placeholder).expect("config should be written");

    for command in [
        &["agent", "check"][..],
        &["agent", "update"][..],
        &["agent", "update", "set", "--auto-on"][..],
    ] {
        acps_command(tempdir.path())
            .args(command)
            .assert()
            .failure()
            .stderr(predicates::str::contains("select a real agent"));
    }
}

#[test]
fn agent_check_skips_non_registry_agent() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let escape_hatch = VALID_CONFIG.replace(r#"id = "opencode""#, r#"id = "custom-private-agent""#);
    fs::write(config_dir.join("acps-config.toml"), escape_hatch).expect("config should be written");

    // `acps agent check` has no managed steps to inspect for an escape-hatch
    // agent, so it reports a skip and exits 0 rather than erroring.
    acps_command(tempdir.path())
        .args(["agent", "check"])
        .assert()
        .success()
        .stdout(predicates::str::contains("skipped"))
        .stdout(predicates::str::contains("not a managed registry agent"));
}

#[test]
fn agent_update_execute_skips_non_registry_agent() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let escape_hatch = VALID_CONFIG.replace(r#"id = "opencode""#, r#"id = "custom-private-agent""#);
    fs::write(config_dir.join("acps-config.toml"), escape_hatch).expect("config should be written");

    // A one-shot update for an escape-hatch agent reports the skip and exits 0
    // rather than erroring on a missing entry.
    acps_command(tempdir.path())
        .args(["agent", "update"])
        .assert()
        .success()
        .stdout(predicates::str::contains("skipped"))
        .stdout(predicates::str::contains("not a managed registry agent"));
}
