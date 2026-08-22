use std::fs;

use crate::common::cli::*;

#[test]
fn agent_set_codex_accepts_effort_only() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), codex_config())
        .expect("config should be written");
    let options_path =
        write_acp_config_options_with_efforts(tempdir.path(), &[], &[], &["low", "medium", "high"]);

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args(["agent", "set", "--effort", "high"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent: codex"))
        .stdout(predicates::str::contains("effort: high"));

    let config =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    assert!(config.contains(r#"effort = "high""#));
}

#[test]
fn agent_set_codex_rejects_unadvertised_effort() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), codex_config())
        .expect("config should be written");
    let options_path =
        write_acp_config_options_with_efforts(tempdir.path(), &[], &[], &["low", "medium", "high"]);

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args(["agent", "set", "--effort", "bogus"])
        .assert()
        .failure();

    let config =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    assert!(!config.contains("effort = "));
}

#[test]
fn agent_set_amp_rejects_effort() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = VALID_CONFIG
        .replace(r#"id = "opencode""#, r#"id = "amp""#)
        .replace(r#"name = "OpenCode""#, r#"name = "Amp Code""#)
        .replace(r#"command = "opencode""#, r#"command = "amp-acp""#)
        .replace(r#"args = ["acp"]"#, r#"args = []"#)
        .replace(r#"env = ["OPENCODE_API_KEY"]"#, r#"env = ["AMP_API_KEY"]"#)
        .replace(
            r#"
[agent.install]
type = "shell"
shell = "curl -fsSL https://opencode.ai/install | bash"
creates = "opencode"
"#,
            "",
        );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "set", "--effort", "high"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Amp Code does not support reasoning-effort configuration",
        ));
}

#[test]
fn agent_set_rejects_mode_combined_with_effort() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), codex_config())
        .expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "set", "--mode", "auto", "--effort", "high"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "--mode cannot be combined with --provider, --model, --effort, or --api-key-ref",
        ));
}
