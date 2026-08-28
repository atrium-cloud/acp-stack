use std::fs;

use crate::common::cli::*;

#[tokio::test(flavor = "multi_thread")]
async fn array_status_overlays_daemon_state_when_session_access_is_available() {
    let harness = AgentCliHarness::spawn().await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home_with_socket(
        home.path(),
        &harness.base_url,
        ADMIN_KEY,
        Some(&harness.socket_path),
    );

    acps_command(home.path())
        .env("ACP_STACK_SESSION_KEY", SESSION_KEY)
        .args(["array", "status"])
        .assert()
        .success()
        .stdout(predicates::str::contains("daemon: ready"))
        .stdout(predicates::str::contains("target: opencode"))
        .stdout(predicates::str::contains("state=stopped"));
}

#[test]
fn sessions_mutating_commands_require_explicit_session_key() {
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home(home.path(), "http://127.0.0.1:9", ADMIN_KEY);

    for args in [
        vec!["sessions", "new"],
        vec!["sessions", "load", "sess_test"],
        vec!["sessions", "resume", "sess_test"],
        vec!["sessions", "fork", "sess_test"],
        vec!["sessions", "prompt", "sess_test", "hello"],
        vec!["sessions", "cancel", "sess_test"],
        vec!["sessions", "close", "sess_test"],
    ] {
        acps_command(home.path())
            .env_remove("ACP_STACK_SESSION_KEY")
            .args(args)
            .assert()
            .failure()
            .stderr(predicates::str::contains(
                "--session-key or ACP_STACK_SESSION_KEY",
            ));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn auth_local_session_access_enable_and_disable_call_daemon() {
    let harness = AgentCliHarness::spawn().await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home_with_socket(
        home.path(),
        &harness.base_url,
        ADMIN_KEY,
        Some(&harness.socket_path),
    );

    acps_command(home.path())
        .args([
            "auth",
            "local-session-access",
            "enable",
            "--admin-key",
            ADMIN_KEY,
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("local session access: keyless"));

    let daemon_config = fs::read_to_string(&harness.config_path).expect("daemon config");
    assert!(daemon_config.contains("session_auth = \"keyless\""));

    acps_command(home.path())
        .args([
            "auth",
            "local-session-access",
            "disable",
            "--admin-key",
            ADMIN_KEY,
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "local session access: session-key",
        ));
}

#[test]
fn auth_local_session_access_status_reports_config() {
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home_with_socket_and_session_auth(
        home.path(),
        "http://127.0.0.1:9",
        ADMIN_KEY,
        None,
        Some("keyless"),
    );

    acps_command(home.path())
        .args(["auth", "local-session-access", "status"])
        .assert()
        .success()
        .stdout(predicates::str::contains("local session access: keyless"));
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_new_uses_local_socket_without_key_when_enabled() {
    let harness = AgentCliHarness::spawn().await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home_with_socket_and_session_auth(
        home.path(),
        &harness.base_url,
        ADMIN_KEY,
        Some(&harness.socket_path),
        Some("keyless"),
    );

    acps_command(home.path())
        .args([
            "auth",
            "local-session-access",
            "enable",
            "--admin-key",
            ADMIN_KEY,
        ])
        .assert()
        .success();

    acps_command(home.path())
        .args(["agent", "start", "--admin-key", ADMIN_KEY])
        .assert()
        .success();

    acps_command(home.path())
        .env_remove("ACP_STACK_SESSION_KEY")
        .args(["sessions", "new"])
        .assert()
        .success()
        .stdout(predicates::str::contains("session: "));

    acps_command(home.path())
        .args([
            "auth",
            "local-session-access",
            "disable",
            "--admin-key",
            ADMIN_KEY,
        ])
        .assert()
        .success();
    write_cli_home_with_socket(
        home.path(),
        &harness.base_url,
        ADMIN_KEY,
        Some(&harness.socket_path),
    );

    acps_command(home.path())
        .env_remove("ACP_STACK_SESSION_KEY")
        .args(["sessions", "new"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "--session-key or ACP_STACK_SESSION_KEY",
        ));
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_new_explicit_key_uses_public_api_even_when_local_keyless() {
    let harness = AgentCliHarness::spawn().await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    let missing_socket = home.path().join("missing.sock");
    write_cli_home_with_socket_and_session_auth(
        home.path(),
        &harness.base_url,
        ADMIN_KEY,
        Some(&missing_socket),
        Some("keyless"),
    );

    acps_command(home.path())
        .args(["agent", "start", "--admin-key", ADMIN_KEY])
        .assert()
        .success();

    acps_command(home.path())
        .args(["sessions", "new", "--session-key", SESSION_KEY])
        .assert()
        .success()
        .stdout(predicates::str::contains("session: "));
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_start_reports_daemon_auth_failure() {
    let harness = AgentCliHarness::spawn().await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home(
        home.path(),
        &harness.base_url,
        "acps_admin_wrongwrongwrongwrongwrongwrongwrongwrongwrong",
    );

    acps_command(home.path())
        .args([
            "agent",
            "start",
            "--admin-key",
            "acps_admin_wrongwrongwrongwrongwrongwrongwrongwrongwrong",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "agent API request to /v1/agent/start failed with status 401",
        ));
}
