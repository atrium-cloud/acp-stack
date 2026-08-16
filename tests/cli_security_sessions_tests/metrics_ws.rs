use predicates::prelude::PredicateBooleanExt as _;
use serde_json::Value;

use crate::common::cli::*;

#[tokio::test(flavor = "multi_thread")]
async fn metrics_summary_format_json_returns_summary() {
    let harness = AgentCliHarness::spawn().await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home_with_socket(
        home.path(),
        &harness.base_url,
        ADMIN_KEY,
        Some(&harness.socket_path),
    );

    let output = acps_command()
        .env("HOME", home.path())
        .args(["metrics", "summary", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let body: Value = serde_json::from_slice(&output).expect("metrics json parses");
    assert!(body["counts"].is_object(), "{body}");
    assert!(body["window"].is_object(), "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn ws_common_commands_format_json() {
    let harness = AgentCliHarness::spawn().await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home_with_socket(
        home.path(),
        &harness.base_url,
        ADMIN_KEY,
        Some(&harness.socket_path),
    );

    let connections_output = acps_command()
        .env("HOME", home.path())
        .args(["ws", "connections", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let connections_body: Value =
        serde_json::from_slice(&connections_output).expect("connections json parses");
    assert!(
        connections_body["connections"].as_array().is_some(),
        "{connections_body}",
    );

    let sessions_output = acps_command()
        .env("HOME", home.path())
        .args(["ws", "sessions", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let sessions_body: Value =
        serde_json::from_slice(&sessions_output).expect("sessions json parses");
    assert!(
        sessions_body["sessions"].as_array().is_some(),
        "{sessions_body}"
    );

    let disconnect_output = acps_command()
        .env("HOME", home.path())
        .args([
            "ws",
            "disconnect",
            "--connection-id",
            "missing",
            "--format",
            "json",
            "--admin-key",
            ADMIN_KEY,
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let disconnect_body: Value =
        serde_json::from_slice(&disconnect_output).expect("disconnect json parses");
    assert_eq!(disconnect_body["requested"], 0);
}

#[test]
fn ws_disconnect_requires_target_before_admin_key() {
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home(home.path(), "http://127.0.0.1:9", ADMIN_KEY);

    acps_command()
        .env("HOME", home.path())
        .args(["ws", "disconnect"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--connection-id or --session-id"))
        .stderr(predicates::str::contains("--admin-key").not());
}
