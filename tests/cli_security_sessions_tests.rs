#![cfg(all(feature = "dev-tools", feature = "test-fixtures"))]

use predicates::prelude::PredicateBooleanExt as _;
use serde_json::Value;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

mod common;
use common::cli::*;

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

    acps_command()
        .env("HOME", home.path())
        .env("ACP_STACK_SESSION_KEY", SESSION_KEY)
        .args(["array", "status"])
        .assert()
        .success()
        .stdout(predicates::str::contains("daemon: ready"))
        .stdout(predicates::str::contains("target: opencode"))
        .stdout(predicates::str::contains("state=stopped"));
}

#[tokio::test(flavor = "multi_thread")]
async fn security_check_calls_running_daemon_without_auth_key() {
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
        .args(["security", "check"])
        .assert()
        .success()
        .stdout(predicates::str::contains("ok: "))
        .stdout(predicates::str::contains("auth_failures_total:"))
        .stdout(predicates::str::contains("findings:"));
}

#[tokio::test(flavor = "multi_thread")]
async fn security_check_renders_hint_line_for_each_finding() {
    // Drive a finding by reporting an unspecified-address effective_bind; the
    // self-check turns that into `api.public_bind` (warning). The CLI must
    // render the diagnostic line AND an indented `hint:` line with the
    // remediation prose.
    let harness = AgentCliHarness::spawn_with_effective_bind("0.0.0.0:7700").await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home_with_socket(
        home.path(),
        &harness.base_url,
        ADMIN_KEY,
        Some(&harness.socket_path),
    );

    acps_command()
        .env("HOME", home.path())
        .args(["security", "check"])
        .assert()
        .success()
        .stdout(predicates::str::contains("api.public_bind"))
        .stdout(predicates::str::contains("    hint: "))
        .stdout(
            predicates::str::contains("loopback").or(predicates::str::contains("reverse proxy")),
        );
}

#[test]
fn security_check_does_not_accept_admin_key_flag() {
    acps_command()
        .args(["security", "check", "--admin-key", SESSION_KEY])
        .assert()
        .failure()
        .stderr(predicates::str::contains("unexpected argument"))
        .stderr(predicates::str::contains("--admin-key"));
}

#[tokio::test(flavor = "multi_thread")]
async fn security_history_renders_table_and_next_page_cursor() {
    let harness = AgentCliHarness::spawn().await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home_with_socket(
        home.path(),
        &harness.base_url,
        ADMIN_KEY,
        Some(&harness.socket_path),
    );

    let _first_run_id = run_security_check_and_extract_run_id(home.path());
    let second_run_id = run_security_check_and_extract_run_id(home.path());

    acps_command()
        .env("HOME", home.path())
        .args([
            "security",
            "history",
            "--limit",
            "1",
            "--admin-key",
            ADMIN_KEY,
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("id"))
        .stdout(predicates::str::contains("started_at"))
        .stdout(predicates::str::contains("status"))
        .stdout(predicates::str::contains("crit"))
        .stdout(predicates::str::contains("warn"))
        .stdout(predicates::str::contains("auth"))
        .stdout(predicates::str::contains("srun_"))
        .stdout(predicates::str::contains(second_run_id.as_str()))
        .stdout(predicates::str::contains("failed").or(predicates::str::contains("succeeded")))
        .stdout(predicates::str::contains("next page: --after "));
}

#[tokio::test(flavor = "multi_thread")]
async fn security_history_json_renders_runs_and_cursor() {
    let harness = AgentCliHarness::spawn().await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home_with_socket(
        home.path(),
        &harness.base_url,
        ADMIN_KEY,
        Some(&harness.socket_path),
    );

    let _first_run_id = run_security_check_and_extract_run_id(home.path());
    let second_run_id = run_security_check_and_extract_run_id(home.path());

    let output = acps_command()
        .env("HOME", home.path())
        .args([
            "security",
            "history",
            "--limit",
            "1",
            "--json",
            "--admin-key",
            ADMIN_KEY,
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let body: Value = serde_json::from_slice(&output).expect("history json should parse");
    let runs = body["runs"].as_array().expect("runs should be an array");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["id"], second_run_id);
    assert!(
        body["next_cursor"].as_str().is_some(),
        "full first page should include a next cursor: {body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn security_history_global_format_json_matches_json_alias() {
    let harness = AgentCliHarness::spawn().await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home_with_socket(
        home.path(),
        &harness.base_url,
        ADMIN_KEY,
        Some(&harness.socket_path),
    );

    let run_id = run_security_check_and_extract_run_id(home.path());

    let output = acps_command()
        .env("HOME", home.path())
        .args([
            "security",
            "history",
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
    let body: Value = serde_json::from_slice(&output).expect("history json should parse");
    let runs = body["runs"].as_array().expect("runs should be an array");
    assert!(runs.iter().any(|run| run["id"] == run_id), "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn security_history_json_alias_conflicts_with_explicit_text_format() {
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
        .args(["security", "history", "--json", "--format", "text"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "--json conflicts with --format text",
        ));
}

#[test]
fn security_history_json_alias_conflict_precedes_config_load() {
    let home = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", home.path())
        .args(["security", "history", "--json", "--format", "text"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "--json conflicts with --format text",
        ));
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn security_show_renders_run_findings_hints_and_details() {
    let harness = AgentCliHarness::spawn().await;
    std::fs::set_permissions(&harness.state_path, fs::Permissions::from_mode(0o644))
        .expect("loosen state db mode");
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home_with_socket(
        home.path(),
        &harness.base_url,
        ADMIN_KEY,
        Some(&harness.socket_path),
    );

    let run_id = run_security_check_and_extract_run_id(home.path());

    acps_command()
        .env("HOME", home.path())
        .args(["security", "show", &run_id, "--admin-key", ADMIN_KEY])
        .assert()
        .success()
        .stdout(predicates::str::contains(format!("run_id: {run_id}")))
        .stdout(predicates::str::contains("started_at:"))
        .stdout(predicates::str::contains("finished_at:"))
        .stdout(predicates::str::contains("status:"))
        .stdout(predicates::str::contains("critical:"))
        .stdout(predicates::str::contains("warning:"))
        .stdout(predicates::str::contains("runtime.path_mode_loose"))
        .stdout(predicates::str::contains("    hint: "))
        .stdout(predicates::str::contains("    details: "))
        .stdout(predicates::str::contains("\"path\""))
        .stdout(predicates::str::contains("\"kind\""));
}

#[test]
fn security_show_rejects_invalid_run_id_before_daemon_request() {
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home(home.path(), "http://127.0.0.1:9", ADMIN_KEY);

    acps_command()
        .env("HOME", home.path())
        .args(["security", "show", "srun/not-safe"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("expected an alphanumeric run id"))
        .stderr(predicates::str::contains("--admin-key").not())
        .stderr(predicates::str::contains("/v1/security/history").not());
}

#[test]
fn security_history_rejects_invalid_limit_before_admin_key() {
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home(home.path(), "http://127.0.0.1:9", ADMIN_KEY);

    acps_command()
        .env("HOME", home.path())
        .args(["security", "history", "--limit", "0"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("limit must be"))
        .stderr(predicates::str::contains("--admin-key").not())
        .stderr(predicates::str::contains("/v1/security/history").not());
}

#[tokio::test(flavor = "multi_thread")]
async fn security_history_uses_admin_key_not_session_key() {
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
        .args(["security", "history", "--admin-key", SESSION_KEY])
        .assert()
        .failure()
        .stderr(predicates::str::contains("/v1/security/history"))
        .stderr(predicates::str::contains("401"));
}

#[tokio::test(flavor = "multi_thread")]
async fn security_show_uses_admin_key_not_session_key() {
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
        .args([
            "security",
            "show",
            "srun_does_not_exist",
            "--admin-key",
            SESSION_KEY,
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("/v1/security/history/{run_id}"))
        .stderr(predicates::str::contains("401"));
}

fn run_security_check_and_extract_run_id(home: &std::path::Path) -> String {
    let output = acps_command()
        .env("HOME", home)
        .args(["security", "check"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("security check stdout should be utf8");
    stdout
        .lines()
        .find_map(|line| line.strip_prefix("run_id: "))
        .expect("security check should print run_id")
        .trim()
        .to_owned()
}

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
        acps_command()
            .env("HOME", home.path())
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

    acps_command()
        .env("HOME", home.path())
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

    acps_command()
        .env("HOME", home.path())
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

    acps_command()
        .env("HOME", home.path())
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

    acps_command()
        .env("HOME", home.path())
        .args([
            "auth",
            "local-session-access",
            "enable",
            "--admin-key",
            ADMIN_KEY,
        ])
        .assert()
        .success();

    acps_command()
        .env("HOME", home.path())
        .args(["agent", "start", "--admin-key", ADMIN_KEY])
        .assert()
        .success();

    acps_command()
        .env("HOME", home.path())
        .env_remove("ACP_STACK_SESSION_KEY")
        .args(["sessions", "new"])
        .assert()
        .success()
        .stdout(predicates::str::contains("session: "));

    acps_command()
        .env("HOME", home.path())
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

    acps_command()
        .env("HOME", home.path())
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

    acps_command()
        .env("HOME", home.path())
        .args(["agent", "start", "--admin-key", ADMIN_KEY])
        .assert()
        .success();

    acps_command()
        .env("HOME", home.path())
        .args(["sessions", "new", "--session-key", SESSION_KEY])
        .assert()
        .success()
        .stdout(predicates::str::contains("session: "));
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

#[tokio::test(flavor = "multi_thread")]
async fn agent_start_reports_daemon_auth_failure() {
    let harness = AgentCliHarness::spawn().await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home(
        home.path(),
        &harness.base_url,
        "acps_admin_wrongwrongwrongwrongwrongwrongwrongwrongwrong",
    );

    acps_command()
        .env("HOME", home.path())
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
