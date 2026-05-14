use assert_cmd::Command;
use base64::Engine;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

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

#[test]
fn init_creates_config_and_state() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    let mut command = Command::cargo_bin("acps").expect("binary should build");

    command
        .env("HOME", tempdir.path())
        .arg("init")
        .assert()
        .success()
        .stdout(predicates::str::contains("initialized acp-stack"));

    let config_path = tempdir.path().join(".config/acp-stack/acp-stack.toml");
    let state_path = tempdir.path().join(".local/share/acp-stack/state.sqlite");
    assert!(config_path.is_file());
    assert!(state_path.is_file());

    let config = fs::read_to_string(config_path).expect("starter config should be readable");
    assert!(config.contains("[workspace.source]"));
    assert!(config.contains(r#"type = "none""#));
}

#[cfg(unix)]
#[test]
fn init_creates_owner_only_config_and_state_paths() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("init")
        .assert()
        .success();

    let config_dir = tempdir.path().join(".config/acp-stack");
    let state_dir = tempdir.path().join(".local/share/acp-stack");
    let config_path = config_dir.join("acp-stack.toml");
    let state_path = state_dir.join("state.sqlite");

    assert_eq!(mode(&config_dir), 0o700);
    assert_eq!(mode(&state_dir), 0o700);
    assert_eq!(mode(&config_path), 0o600);
    assert_eq!(mode(&state_path), 0o600);
}

#[test]
fn init_does_not_overwrite_existing_config() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config_path = config_dir.join("acp-stack.toml");
    fs::write(&config_path, VALID_CONFIG).expect("config should be written");

    let mut command = Command::cargo_bin("acps").expect("binary should build");

    command
        .env("HOME", tempdir.path())
        .arg("init")
        .assert()
        .success()
        .stdout(predicates::str::contains("validated existing config"));

    let config = fs::read_to_string(config_path).expect("config should be readable");
    assert_eq!(config, VALID_CONFIG);
}

#[test]
fn init_fails_when_existing_config_is_invalid() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(
        config_dir.join("acp-stack.toml"),
        VALID_CONFIG.replace(r#"bind = "127.0.0.1:7700""#, r#"bind = "bad""#),
    )
    .expect("invalid config should be written");

    let mut command = Command::cargo_bin("acps").expect("binary should build");

    command
        .env("HOME", tempdir.path())
        .arg("init")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "api.bind must be a socket address",
        ));
}

#[test]
fn status_reports_config_state_schema_and_latest_event() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("init")
        .assert()
        .success();

    let mut command = Command::cargo_bin("acps").expect("binary should build");
    command
        .env("HOME", tempdir.path())
        .arg("status")
        .assert()
        .success()
        .stdout(predicates::str::contains("config: ok"))
        .stdout(predicates::str::contains("state: ok"))
        .stdout(predicates::str::contains("schema_version: 1"))
        .stdout(predicates::str::contains("latest_event:"));
}

#[cfg(unix)]
#[test]
fn status_creates_owner_only_state_when_config_exists_without_state() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config_path = config_dir.join("acp-stack.toml");
    fs::write(&config_path, VALID_CONFIG).expect("config should be written");
    fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o755))
        .expect("config dir permissions should be set");
    fs::set_permissions(&config_path, fs::Permissions::from_mode(0o644))
        .expect("config file permissions should be set");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("status")
        .assert()
        .success();

    let state_dir = tempdir.path().join(".local/share/acp-stack");
    let state_path = state_dir.join("state.sqlite");
    assert_eq!(mode(&config_dir), 0o700);
    assert_eq!(mode(&config_path), 0o600);
    assert_eq!(mode(&state_dir), 0o700);
    assert_eq!(mode(&state_path), 0o600);
}

#[cfg(unix)]
#[test]
fn status_repairs_config_permissions_before_validation_failure() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config_path = config_dir.join("acp-stack.toml");
    fs::write(
        &config_path,
        VALID_CONFIG.replace(r#"bind = "127.0.0.1:7700""#, r#"bind = "bad""#),
    )
    .expect("invalid config should be written");
    fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o755))
        .expect("config dir permissions should be set");
    fs::set_permissions(&config_path, fs::Permissions::from_mode(0o644))
        .expect("config file permissions should be set");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("status")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "api.bind must be a socket address",
        ));

    assert_eq!(mode(&config_dir), 0o700);
    assert_eq!(mode(&config_path), 0o600);
}

#[test]
fn logs_query_shows_init_event() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("init")
        .assert()
        .success();

    let mut command = Command::cargo_bin("acps").expect("binary should build");
    command
        .env("HOME", tempdir.path())
        .args(["logs", "query"])
        .assert()
        .success()
        .stdout(predicates::str::contains("info init.completed initialized"));
}

#[cfg(unix)]
#[test]
fn logs_query_creates_owner_only_empty_state_when_missing() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args(["logs", "query"])
        .assert()
        .success()
        .stdout("");

    let state_dir = tempdir.path().join(".local/share/acp-stack");
    let state_path = state_dir.join("state.sqlite");
    assert_eq!(mode(&state_dir), 0o700);
    assert_eq!(mode(&state_path), 0o600);
}

#[test]
fn logs_query_supports_limit_and_level_filter() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("init")
        .assert()
        .success();
    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("status")
        .assert()
        .success();

    let mut limit_command = Command::cargo_bin("acps").expect("binary should build");
    limit_command
        .env("HOME", tempdir.path())
        .args(["logs", "query", "--limit", "1"])
        .assert()
        .success()
        .stdout(predicates::str::contains("status.checked").count(1));

    let mut level_command = Command::cargo_bin("acps").expect("binary should build");
    level_command
        .env("HOME", tempdir.path())
        .args(["logs", "query", "--level", "error"])
        .assert()
        .success()
        .stdout("");
}

#[test]
fn failed_cli_command_records_error_after_state_exists() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("init")
        .assert()
        .success();

    let config_path = tempdir.path().join(".config/acp-stack/acp-stack.toml");
    fs::write(
        config_path,
        VALID_CONFIG.replace(r#"bind = "127.0.0.1:7700""#, r#"bind = "bad""#),
    )
    .expect("invalid config should be written");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("status")
        .assert()
        .failure();

    let mut logs_command = Command::cargo_bin("acps").expect("binary should build");
    logs_command
        .env("HOME", tempdir.path())
        .args(["logs", "query", "--level", "error"])
        .assert()
        .success()
        .stdout(predicates::str::contains("error cli.error command failed"));
}

#[test]
fn parse_failure_records_error_after_state_exists() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("init")
        .assert()
        .success();

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("unknown-command")
        .assert()
        .failure();

    let mut logs_command = Command::cargo_bin("acps").expect("binary should build");
    logs_command
        .env("HOME", tempdir.path())
        .args(["logs", "query", "--level", "error"])
        .assert()
        .success()
        .stdout(predicates::str::contains("error cli.error command failed"));
}

#[test]
fn help_invocations_do_not_record_error_events() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("init")
        .assert()
        .success();

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("--help")
        .assert()
        .success();

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("--version")
        .assert()
        .success();

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args(["logs", "--help"])
        .assert()
        .success();

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args(["logs", "query", "--help"])
        .assert()
        .success();

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args(["logs", "query", "--level", "error"])
        .assert()
        .success()
        .stdout("");
}

#[cfg(unix)]
#[test]
fn cli_error_payload_handles_control_bytes_in_argument() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("init")
        .assert()
        .success();

    // Path that mixes a stray ANSI escape sequence and a bare control byte. The runtime
    // must strip ANSI, encode the remaining bytes via serde_json, and still produce a
    // valid JSON payload that survives json_valid() in SQLite.
    let bad_path = OsString::from_vec(b"/tmp/acp\x1b[31m-missing\x07\x08-file.toml".to_vec());

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args(["config", "validate"])
        .arg(&bad_path)
        .assert()
        .failure();

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args(["logs", "query", "--level", "error"])
        .assert()
        .success()
        .stdout(predicates::str::contains("error cli.error command failed"));
}

#[test]
fn empty_home_is_treated_as_unset() {
    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", "")
        .arg("init")
        .assert()
        .failure()
        .stderr(predicates::str::contains("HOME is not set"));
}

#[cfg(unix)]
#[test]
fn init_repairs_config_permissions_before_validation_failure() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config_path = config_dir.join("acp-stack.toml");
    fs::write(
        &config_path,
        VALID_CONFIG.replace(r#"bind = "127.0.0.1:7700""#, r#"bind = "bad""#),
    )
    .expect("invalid config should be written");
    fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o755))
        .expect("config dir perms should set");
    fs::set_permissions(&config_path, fs::Permissions::from_mode(0o644))
        .expect("config file perms should set");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("init")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "api.bind must be a socket address",
        ));

    assert_eq!(mode(&config_dir), 0o700);
    assert_eq!(mode(&config_path), 0o600);
}

#[cfg(unix)]
#[test]
fn init_repairs_existing_permissive_state_file() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let state_dir = tempdir.path().join(".local/share/acp-stack");
    fs::create_dir_all(&state_dir).expect("state dir should be created");
    let state_path = state_dir.join("state.sqlite");
    fs::write(&state_path, b"").expect("placeholder state file should be written");
    fs::set_permissions(&state_path, fs::Permissions::from_mode(0o644))
        .expect("permissive perms should set");
    assert_eq!(mode(&state_path), 0o644);

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("init")
        .assert()
        .success();

    assert_eq!(mode(&state_path), 0o600);
}

#[cfg(unix)]
#[test]
fn status_repairs_existing_permissive_state_file() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acp-stack.toml"), VALID_CONFIG)
        .expect("valid config should be written");

    let state_dir = tempdir.path().join(".local/share/acp-stack");
    fs::create_dir_all(&state_dir).expect("state dir should be created");
    let state_path = state_dir.join("state.sqlite");
    fs::write(&state_path, b"").expect("placeholder state file should be written");
    fs::set_permissions(&state_path, fs::Permissions::from_mode(0o644))
        .expect("permissive perms should set");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("status")
        .assert()
        .success();

    assert_eq!(mode(&state_path), 0o600);
}

#[cfg(unix)]
#[test]
fn logs_query_repairs_existing_permissive_state_file() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let state_dir = tempdir.path().join(".local/share/acp-stack");
    fs::create_dir_all(&state_dir).expect("state dir should be created");
    let state_path = state_dir.join("state.sqlite");
    fs::write(&state_path, b"").expect("placeholder state file should be written");
    fs::set_permissions(&state_path, fs::Permissions::from_mode(0o644))
        .expect("permissive perms should set");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args(["logs", "query"])
        .assert()
        .success();

    assert_eq!(mode(&state_path), 0o600);
}

#[cfg(unix)]
#[test]
fn error_recording_path_repairs_permissive_state_file() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("init")
        .assert()
        .success();

    let state_path = tempdir.path().join(".local/share/acp-stack/state.sqlite");
    fs::set_permissions(&state_path, fs::Permissions::from_mode(0o644))
        .expect("permissive perms should set");
    assert_eq!(mode(&state_path), 0o644);

    // Corrupt the config so the next invocation fails through the error-recording path.
    let config_path = tempdir.path().join(".config/acp-stack/acp-stack.toml");
    fs::write(
        &config_path,
        VALID_CONFIG.replace(r#"bind = "127.0.0.1:7700""#, r#"bind = "bad""#),
    )
    .expect("invalid config should be written");

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .arg("status")
        .assert()
        .failure();

    assert_eq!(
        mode(&state_path),
        0o600,
        "record_cli_error_message must repair permissive perms before writing the error row",
    );

    Command::cargo_bin("acps")
        .expect("binary should build")
        .env("HOME", tempdir.path())
        .args(["logs", "query", "--level", "error"])
        .assert()
        .success()
        .stdout(predicates::str::contains("error cli.error command failed"));
}

#[cfg(unix)]
fn mode(path: &std::path::Path) -> u32 {
    fs::metadata(path)
        .expect("metadata should be readable")
        .permissions()
        .mode()
        & 0o777
}
