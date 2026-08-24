use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use crate::common::cli::*;

#[cfg(unix)]
#[test]
fn status_creates_owner_only_state_when_config_exists_without_state() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config_path = config_dir.join("acps-config.toml");
    fs::write(&config_path, VALID_CONFIG).expect("config should be written");
    fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o755))
        .expect("config dir permissions should be set");
    fs::set_permissions(&config_path, fs::Permissions::from_mode(0o644))
        .expect("config file permissions should be set");

    acps_command()
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
    let config_path = config_dir.join("acps-config.toml");
    fs::write(
        &config_path,
        VALID_CONFIG.replace(r#"bind = "127.0.0.1:7700""#, r#"bind = "bad""#),
    )
    .expect("invalid config should be written");
    fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o755))
        .expect("config dir permissions should be set");
    fs::set_permissions(&config_path, fs::Permissions::from_mode(0o644))
        .expect("config file permissions should be set");

    acps_command()
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
fn empty_home_is_treated_as_unset() {
    acps_command()
        .env("HOME", "")
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
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
    let config_path = config_dir.join("acps-config.toml");
    fs::write(
        &config_path,
        VALID_CONFIG.replace(r#"bind = "127.0.0.1:7700""#, r#"bind = "bad""#),
    )
    .expect("invalid config should be written");
    fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o755))
        .expect("config dir perms should set");
    fs::set_permissions(&config_path, fs::Permissions::from_mode(0o644))
        .expect("config file perms should set");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
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

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
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
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG)
        .expect("valid config should be written");

    let state_dir = tempdir.path().join(".local/share/acp-stack");
    fs::create_dir_all(&state_dir).expect("state dir should be created");
    let state_path = state_dir.join("state.sqlite");
    fs::write(&state_path, b"").expect("placeholder state file should be written");
    fs::set_permissions(&state_path, fs::Permissions::from_mode(0o644))
        .expect("permissive perms should set");

    acps_command()
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

    acps_command()
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

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success();

    let state_path = tempdir.path().join(".local/share/acp-stack/state.sqlite");
    fs::set_permissions(&state_path, fs::Permissions::from_mode(0o644))
        .expect("permissive perms should set");
    assert_eq!(mode(&state_path), 0o644);

    // Corrupt the config so the next invocation takes the error-recording path.
    let config_path = tempdir.path().join(".config/acp-stack/acps-config.toml");
    fs::write(
        &config_path,
        VALID_CONFIG.replace(r#"bind = "127.0.0.1:7700""#, r#"bind = "bad""#),
    )
    .expect("invalid config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .arg("status")
        .assert()
        .failure();

    assert_eq!(
        mode(&state_path),
        0o600,
        "record_cli_error_message must repair permissive perms before writing the error row",
    );

    acps_command()
        .env("HOME", tempdir.path())
        .args(["logs", "query", "--level", "error"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "error cli cli.error command failed",
        ));
}
