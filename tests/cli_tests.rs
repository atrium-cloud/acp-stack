use assert_cmd::Command;
use base64::Engine;
use std::fs;

const VALID_CONFIG: &str = include_str!("fixtures/valid-acp-stack.toml");

#[test]
fn prints_version() {
    let mut command = Command::cargo_bin("acps").expect("binary should build");

    command
        .arg("--version")
        .assert()
        .success()
        .stdout(predicates::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn validates_explicit_config_path() {
    let mut command = Command::cargo_bin("acps").expect("binary should build");

    command
        .args(["config", "validate", "tests/fixtures/valid-acp-stack.toml"])
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

    let mut command = Command::cargo_bin("acps").expect("binary should build");

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
        ));
}

#[test]
fn exports_default_home_config_to_stdout() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acp-stack.toml"), VALID_CONFIG).expect("config should be written");

    let mut command = Command::cargo_bin("acps").expect("binary should build");

    command
        .env("HOME", tempdir.path())
        .args(["config", "export"])
        .assert()
        .success()
        .stdout(predicates::str::contains("[api]"))
        .stdout(predicates::str::contains("[agent.install]"));
}

#[test]
fn exports_base64_default_home_config() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acp-stack.toml"), VALID_CONFIG).expect("config should be written");

    let mut command = Command::cargo_bin("acps").expect("binary should build");
    let output = command
        .env("HOME", tempdir.path())
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
    assert!(toml.contains("[agent.install]"));
}

#[test]
fn exports_default_home_config_to_output_path() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acp-stack.toml"), VALID_CONFIG).expect("config should be written");
    let output_path = tempdir.path().join("exported.toml");

    let mut command = Command::cargo_bin("acps").expect("binary should build");

    command
        .env("HOME", tempdir.path())
        .args([
            "config",
            "export",
            "--output",
            output_path.to_str().expect("path should be UTF-8"),
        ])
        .assert()
        .success()
        .stdout("");

    let exported = fs::read_to_string(output_path).expect("export should be readable");
    assert!(exported.contains("[api]"));
    assert!(exported.contains("[agent.install]"));
}
