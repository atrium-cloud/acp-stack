use predicates::prelude::PredicateBooleanExt as _;
use serde_json::Value;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use crate::common::cli::*;

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
