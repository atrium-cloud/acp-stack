use serde_json::Value;
use std::fs;

use crate::common::cli::*;

#[test]
fn logs_query_shows_init_event() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command(tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success();

    let mut command = acps_command(tempdir.path());
    command
        .args(["logs", "query"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "info cli init.completed initialized",
        ));
}

#[cfg(unix)]
#[test]
fn logs_query_creates_owner_only_empty_state_when_missing() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command(tempdir.path())
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

    acps_command(tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success();
    acps_command(tempdir.path())
        .arg("status")
        .assert()
        .success();

    let mut limit_command = acps_command(tempdir.path());
    limit_command
        .args(["logs", "query", "--limit", "1"])
        .assert()
        .success()
        .stdout(predicates::str::contains("status.checked").count(1));

    let mut level_command = acps_command(tempdir.path());
    level_command
        .args(["logs", "query", "--level", "error"])
        .assert()
        .success()
        .stdout("");
}

#[test]
fn logs_query_json_emits_envelope_with_cursor() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command(tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success();
    acps_command(tempdir.path())
        .arg("status")
        .assert()
        .success();

    let output = acps_command(tempdir.path())
        .args(["logs", "query", "--limit", "1", "--json"])
        .output()
        .expect("acps logs query --json should execute");
    assert!(
        output.status.success(),
        "exit status: {:?}, stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout must be valid JSON");
    let events = parsed
        .get("events")
        .and_then(|v| v.as_array())
        .expect("events array present");
    assert_eq!(events.len(), 1, "limit=1 must return exactly one event");
    let event = &events[0];
    for field in [
        "id",
        "created_at",
        "level",
        "kind",
        "message",
        "payload_json",
        "source",
    ] {
        assert!(
            event.get(field).is_some(),
            "event JSON missing field `{field}`: {event}"
        );
    }
    let cursor = parsed
        .get("next_cursor")
        .expect("next_cursor key present even when null")
        .as_str()
        .expect("next_cursor populated when page saturates limit");
    assert!(
        !cursor.is_empty(),
        "next_cursor must be a non-empty id when limit=1 saturates"
    );

    let stderr = String::from_utf8(output.stderr).expect("stderr is utf8");
    assert!(
        !stderr.contains("-- more rows available"),
        "JSON mode must suppress the human cursor hint, got: {stderr}"
    );
}

#[test]
fn logs_query_global_format_json_matches_json_alias() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command(tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success();

    let output = acps_command(tempdir.path())
        .args(["logs", "query", "--limit", "1", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let parsed: Value = serde_json::from_slice(&output).expect("format json should parse");
    assert!(parsed["events"].as_array().is_some(), "{parsed}");
    assert!(parsed.get("next_cursor").is_some(), "{parsed}");
}

#[test]
fn logs_query_json_alias_conflicts_with_explicit_text_format() {
    let home = tempfile::tempdir().expect("home tempdir");
    acps_command(home.path())
        .args(["logs", "query", "--json", "--format", "text"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "--json conflicts with --format text",
        ));
}

#[test]
fn logs_tail_rejects_format_json_before_loading_config() {
    let home = tempfile::tempdir().expect("home tempdir");
    acps_command(home.path())
        .args(["logs", "tail", "--format", "json"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "logs tail does not support --format json",
        ));
}

#[test]
fn text_only_commands_reject_format_json_before_loading_config() {
    let home = tempfile::tempdir().expect("home tempdir");
    acps_command(home.path())
        .args(["subagent", "status", "--format", "json"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "subagent does not support --format json",
        ));
}

#[test]
fn completion_scripts_include_root_and_common_commands() {
    let home = tempfile::tempdir().expect("home tempdir");
    for shell in ["bash", "zsh", "fish", "powershell", "elvish"] {
        let output = acps_command(home.path())
            .args(["completion", shell])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let stdout = String::from_utf8(output).expect("completion is utf8");
        assert!(
            stdout.contains("acps"),
            "{shell} completion missing binary name"
        );
        assert!(
            stdout.contains("sessions"),
            "{shell} completion missing sessions"
        );
        assert!(
            stdout.contains("completion"),
            "{shell} completion missing completion command"
        );
    }
}

#[test]
fn completion_rejects_format_json() {
    let home = tempfile::tempdir().expect("home tempdir");
    acps_command(home.path())
        .args(["completion", "bash", "--format", "json"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "completion does not support --format json",
        ));
}

#[test]
fn failed_cli_command_records_error_after_state_exists() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command(tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success();

    let config_path = tempdir.path().join(".config/acp-stack/acps-config.toml");
    fs::write(
        config_path,
        VALID_CONFIG.replace(r#"bind = "127.0.0.1:7700""#, r#"bind = "bad""#),
    )
    .expect("invalid config should be written");

    acps_command(tempdir.path())
        .arg("status")
        .assert()
        .failure();

    let mut logs_command = acps_command(tempdir.path());
    logs_command
        .args(["logs", "query", "--level", "error"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "error cli cli.error command failed",
        ));
}

#[test]
fn parse_failure_records_error_after_state_exists() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command(tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success();

    acps_command(tempdir.path())
        .arg("unknown-command")
        .assert()
        .failure();

    let mut logs_command = acps_command(tempdir.path());
    logs_command
        .args(["logs", "query", "--level", "error"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "error cli cli.error command failed",
        ));
}

#[test]
fn help_invocations_do_not_record_error_events() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command(tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success();

    acps_command(tempdir.path())
        .arg("--help")
        .assert()
        .success();

    acps_command(tempdir.path())
        .arg("--version")
        .assert()
        .success();

    acps_command(tempdir.path())
        .args(["logs", "--help"])
        .assert()
        .success();

    acps_command(tempdir.path())
        .args(["logs", "query", "--help"])
        .assert()
        .success();

    acps_command(tempdir.path())
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

    acps_command(tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success();

    // Mixing a stray ANSI escape with a bare control byte: the runtime must strip ANSI and
    // still produce a payload that survives SQLite's json_valid().
    let bad_path = OsString::from_vec(b"/tmp/acp\x1b[31m-missing\x07\x08-file.toml".to_vec());

    acps_command(tempdir.path())
        .args(["config", "validate"])
        .arg(&bad_path)
        .assert()
        .failure();

    acps_command(tempdir.path())
        .args(["logs", "query", "--level", "error"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "error cli cli.error command failed",
        ));
}
