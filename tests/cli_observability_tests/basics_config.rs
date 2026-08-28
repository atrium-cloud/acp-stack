use base64::Engine;
use predicates::prelude::PredicateBooleanExt as _;
use std::fs;

use crate::common::cli::*;

#[test]
fn prints_version() {
    let home = tempfile::tempdir().expect("home tempdir");
    let mut command = acps_command(home.path());

    command
        .arg("--version")
        .assert()
        .success()
        .stdout(predicates::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn security_check_is_listed_in_help() {
    let home = tempfile::tempdir().expect("home tempdir");
    acps_command(home.path())
        .args(["security", "--help"])
        .assert()
        .success()
        .stdout(predicates::str::contains("check"))
        .stdout(predicates::str::contains("runtime security self-check"));
}

#[test]
fn top_level_help_describes_common_subcommands() {
    let home = tempfile::tempdir().expect("home tempdir");
    acps_command(home.path())
        .arg("--help")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "Initialize local config, secrets, workspace, and agent files",
        ))
        .stdout(predicates::str::contains(
            "Print daemon health and runtime status",
        ))
        .stdout(predicates::str::contains(
            "Rotate or inspect configured API key references",
        ))
        .stdout(predicates::str::contains(
            "Manage encrypted local secret values",
        ))
        .stdout(predicates::str::contains(
            "Validate, export, or import runtime config",
        ))
        .stdout(predicates::str::contains("Query durable runtime logs"))
        .stdout(predicates::str::contains(
            "Install, control, test, or configure the agent",
        ))
        .stdout(predicates::str::contains(
            "Configure OpenCode small-model behavior",
        ))
        .stdout(predicates::str::contains(
            "List, create, prompt, or close sessions",
        ))
        .stdout(predicates::str::contains("Run development-only workflows"))
        .stdout(predicates::str::contains(
            "acps config import acps-config.toml --dry-run",
        ))
        .stdout(predicates::str::contains("config import --path").not());
}

#[test]
fn config_help_uses_positional_import_path() {
    let home = tempfile::tempdir().expect("home tempdir");
    acps_command(home.path())
        .args(["config", "--help"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "acps config import acps-config.toml --dry-run",
        ))
        .stdout(predicates::str::contains("config import --path").not());
}

#[test]
fn validates_explicit_config_path() {
    let home = tempfile::tempdir().expect("home tempdir");
    let mut command = acps_command(home.path());

    command
        .args([
            "config",
            "validate",
            "tests/fixtures/valid-opencode-stack.toml",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("config is valid"));
}

#[test]
fn validate_failure_exits_nonzero_with_specific_error() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("invalid.toml");
    fs::write(
        &path,
        VALID_CONFIG.replace(r#"bind = "127.0.0.1:7700""#, r#"bind = "bad""#),
    )
    .expect("invalid config should be written");

    let mut command = acps_command(tempdir.path());

    command
        .args([
            "config",
            "validate",
            path.to_str().expect("path should be UTF-8"),
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "api.bind must be a socket address",
        ))
        .stderr(predicates::str::contains(
            "hint: run the command with `--help` and correct the invalid input",
        ));
}

#[test]
fn exports_default_home_config_to_stdout() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    let mut command = acps_command(tempdir.path());

    command
        .args(["config", "export"])
        .assert()
        .success()
        .stdout(predicates::str::contains("[api]"))
        .stdout(predicates::str::contains("[array.targets.agent.install]"))
        .stdout(predicates::str::contains(SESSION_KEY).not())
        .stdout(predicates::str::contains(ADMIN_KEY).not())
        .stdout(predicates::str::contains("sk-proj-exampleinlinevalue").not());
}

#[test]
fn exports_base64_default_home_config() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    let mut command = acps_command(tempdir.path());
    let output = command
        .args(["config", "export", "--base64"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let encoded = String::from_utf8(output).expect("stdout should be UTF-8");
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .expect("stdout should be base64 TOML");
    let toml = String::from_utf8(decoded).expect("decoded TOML should be UTF-8");

    assert!(toml.contains("[api]"));
    assert!(toml.contains("[array.targets.agent.install]"));
}

#[test]
fn exports_default_home_config_to_output_path() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");
    let output_path = tempdir.path().join("exported.toml");

    let mut command = acps_command(tempdir.path());

    command
        .args([
            "config",
            "export",
            "--output",
            output_path.to_str().expect("path should be UTF-8"),
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("progress: loading config"))
        .stdout(predicates::str::contains(
            "progress: rendering config export",
        ))
        .stdout(predicates::str::contains("progress: writing config export"));

    let exported = fs::read_to_string(output_path).expect("export should be readable");
    assert!(exported.contains("[api]"));
    assert!(exported.contains("[array.targets.agent.install]"));
}
