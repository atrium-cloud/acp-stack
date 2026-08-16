use serde_json::Value;

use crate::common::cli::*;

#[tokio::test(flavor = "multi_thread")]
async fn sessions_new_list_prompt_close_round_trip() {
    let harness = AgentCliHarness::spawn().await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home_with_socket(
        home.path(),
        &harness.base_url,
        ADMIN_KEY,
        Some(&harness.socket_path),
    );

    // Start the agent first so /v1/sessions has a live ACP connection.
    acps_command()
        .env("HOME", home.path())
        .args(["agent", "start", "--admin-key", ADMIN_KEY])
        .assert()
        .success();

    let new_output = acps_command()
        .env("HOME", home.path())
        .args(["sessions", "new", "--session-key", SESSION_KEY])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(new_output).expect("utf8");
    let session_id = stdout
        .lines()
        .find_map(|line| line.strip_prefix("session: "))
        .expect("session: <id> line")
        .trim()
        .to_owned();

    acps_command()
        .env("HOME", home.path())
        .args(["sessions", "list", "--range", "all"])
        .assert()
        .success()
        .stdout(predicates::str::contains(session_id.as_str()));

    acps_command()
        .env("HOME", home.path())
        .args([
            "sessions",
            "load",
            &session_id,
            "--session-key",
            SESSION_KEY,
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("session load: active"))
        .stdout(predicates::str::contains(session_id.as_str()));

    acps_command()
        .env("HOME", home.path())
        .args([
            "sessions",
            "resume",
            &session_id,
            "--session-key",
            SESSION_KEY,
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("session resume: active"))
        .stdout(predicates::str::contains(session_id.as_str()));

    acps_command()
        .env("HOME", home.path())
        .args([
            "sessions",
            "prompt",
            &session_id,
            "hello",
            "--session-key",
            SESSION_KEY,
        ])
        .timeout(std::time::Duration::from_secs(30))
        .assert()
        .success()
        .stdout(predicates::str::contains("prompt: completed"))
        .stdout(predicates::str::contains("stop_reason: end_turn"));

    acps_command()
        .env("HOME", home.path())
        .args([
            "sessions",
            "close",
            &session_id,
            "--session-key",
            SESSION_KEY,
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("session close: closed"));
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_new_format_json_returns_session_object() {
    let harness = AgentCliHarness::spawn().await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home_with_socket(
        home.path(),
        &harness.base_url,
        ADMIN_KEY,
        Some(&harness.socket_path),
    );

    acps_command()
        .env("HOME", home.path())
        .args(["agent", "start", "--admin-key", ADMIN_KEY])
        .assert()
        .success();

    let output = acps_command()
        .env("HOME", home.path())
        .args([
            "sessions",
            "new",
            "--format",
            "json",
            "--session-key",
            SESSION_KEY,
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let body: Value = serde_json::from_slice(&output).expect("session json parses");
    assert!(body["id"].as_str().is_some(), "{body}");
    assert!(body["cwd"].as_str().is_some(), "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_common_commands_format_json() {
    let harness = AgentCliHarness::spawn().await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home_with_socket(
        home.path(),
        &harness.base_url,
        ADMIN_KEY,
        Some(&harness.socket_path),
    );

    acps_command()
        .env("HOME", home.path())
        .args(["agent", "start", "--admin-key", ADMIN_KEY])
        .assert()
        .success();

    let new_output = acps_command()
        .env("HOME", home.path())
        .args([
            "sessions",
            "new",
            "--format",
            "json",
            "--session-key",
            SESSION_KEY,
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let new_body: Value = serde_json::from_slice(&new_output).expect("new json parses");
    let session_id = new_body["id"].as_str().expect("session id").to_owned();

    let list_output = acps_command()
        .env("HOME", home.path())
        .args(["sessions", "list", "--range", "all", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let list_body: Value = serde_json::from_slice(&list_output).expect("list json parses");
    assert_eq!(list_body["truncated"], false);
    assert!(
        list_body["sessions"]
            .as_array()
            .expect("sessions array")
            .iter()
            .any(|session| session["id"] == session_id),
        "{list_body}",
    );

    let status_output = acps_command()
        .env("HOME", home.path())
        .args(["sessions", "status", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let status_body: Value = serde_json::from_slice(&status_output).expect("status json parses");
    assert!(
        status_body["sessions"].as_array().is_some(),
        "{status_body}"
    );

    let load_output = acps_command()
        .env("HOME", home.path())
        .arg("sessions")
        .arg("load")
        .arg(&session_id)
        .args(["--format", "json", "--session-key", SESSION_KEY])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let load_body: Value = serde_json::from_slice(&load_output).expect("load json parses");
    assert_eq!(load_body["id"], session_id);
    assert_eq!(load_body["status"], "active");
    assert!(load_body["cwd"].as_str().is_some(), "{load_body}");

    let resume_output = acps_command()
        .env("HOME", home.path())
        .arg("sessions")
        .arg("resume")
        .arg(&session_id)
        .args(["--format", "json", "--session-key", SESSION_KEY])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let resume_body: Value = serde_json::from_slice(&resume_output).expect("resume json parses");
    assert_eq!(resume_body["id"], session_id);
    assert_eq!(resume_body["status"], "active");
    assert!(resume_body["cwd"].as_str().is_some(), "{resume_body}");

    let prompt_output = acps_command()
        .env("HOME", home.path())
        .arg("sessions")
        .arg("prompt")
        .arg(&session_id)
        .arg("hello")
        .args([
            "--no-wait",
            "--format",
            "json",
            "--session-key",
            SESSION_KEY,
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let prompt_body: Value = serde_json::from_slice(&prompt_output).expect("prompt json parses");
    assert_eq!(prompt_body["status"], "pending");
    assert!(prompt_body["prompt_id"].as_str().is_some(), "{prompt_body}");

    let cancel_output = acps_command()
        .env("HOME", home.path())
        .arg("sessions")
        .arg("cancel")
        .arg(&session_id)
        .args(["--format", "json", "--session-key", SESSION_KEY])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let cancel_body: Value = serde_json::from_slice(&cancel_output).expect("cancel json parses");
    assert_eq!(cancel_body["status"], "requested");
    assert_eq!(cancel_body["session_id"], session_id);

    let close_session_output = acps_command()
        .env("HOME", home.path())
        .args([
            "sessions",
            "new",
            "--format",
            "json",
            "--session-key",
            SESSION_KEY,
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let close_session_body: Value =
        serde_json::from_slice(&close_session_output).expect("close session json parses");
    let close_session_id = close_session_body["id"]
        .as_str()
        .expect("close session id")
        .to_owned();

    let close_output = acps_command()
        .env("HOME", home.path())
        .arg("sessions")
        .arg("close")
        .arg(&close_session_id)
        .args(["--format", "json", "--session-key", SESSION_KEY])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let close_body: Value = serde_json::from_slice(&close_output).expect("close json parses");
    assert_eq!(close_body["id"], close_session_id);
    assert_eq!(close_body["status"], "closed");
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_status_reports_no_active_session() {
    let harness = AgentCliHarness::spawn().await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home_with_socket(
        home.path(),
        &harness.base_url,
        ADMIN_KEY,
        Some(&harness.socket_path),
    );

    acps_command()
        .env("HOME", home.path())
        .args(["sessions", "status"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "No session activity in window.\n",
        ));
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_status_renders_recent_active_session() {
    let harness = AgentCliHarness::spawn().await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home_with_socket(
        home.path(),
        &harness.base_url,
        ADMIN_KEY,
        Some(&harness.socket_path),
    );

    acps_command()
        .env("HOME", home.path())
        .args(["agent", "start", "--admin-key", ADMIN_KEY])
        .assert()
        .success();

    let new_output = acps_command()
        .env("HOME", home.path())
        .args(["sessions", "new", "--session-key", SESSION_KEY])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(new_output).expect("utf8");
    let session_id = stdout
        .lines()
        .find_map(|line| line.strip_prefix("session: "))
        .expect("session: <id> line")
        .trim()
        .to_owned();

    acps_command()
        .env("HOME", home.path())
        .args(["sessions", "status"])
        .assert()
        .success()
        .stdout(predicates::str::contains("idle "))
        .stdout(predicates::str::contains("last_activity="))
        .stdout(predicates::str::contains("from=user"))
        .stdout(predicates::str::contains(session_id.as_str()));
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_status_format_json_returns_window() {
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
        .args(["sessions", "status", "--window", "1m", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let body: Value = serde_json::from_slice(&output).expect("sessions status json parses");
    assert_eq!(body["window"], "1m");
    assert!(body["window_start"].is_string(), "{body}");
    assert!(body["sessions"].as_array().is_some(), "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_prompt_no_wait_returns_immediately() {
    let harness = AgentCliHarness::spawn().await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home_with_socket(
        home.path(),
        &harness.base_url,
        ADMIN_KEY,
        Some(&harness.socket_path),
    );

    acps_command()
        .env("HOME", home.path())
        .args(["agent", "start", "--admin-key", ADMIN_KEY])
        .assert()
        .success();

    let new_output = acps_command()
        .env("HOME", home.path())
        .args(["sessions", "new", "--session-key", SESSION_KEY])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(new_output).expect("utf8");
    let session_id = stdout
        .lines()
        .find_map(|line| line.strip_prefix("session: "))
        .expect("session: <id> line")
        .trim()
        .to_owned();

    acps_command()
        .env("HOME", home.path())
        .args([
            "sessions",
            "prompt",
            &session_id,
            "ping",
            "--no-wait",
            "--session-key",
            SESSION_KEY,
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("prompt: pending"))
        .stdout(predicates::str::contains("prompt_id: "));
}
