use super::super::step_runners::{
    CapturedOutput, DEFAULT_INSTALLER_TIMEOUT, finalize_shell_step, select_install_path,
};
use super::super::*;
use super::support::*;
use std::time::Duration;
use tempfile::TempDir;

#[test]
fn declared_shell_timeout_overrides_the_default_budget() {
    let install = shell_install_set_with_timeout("true", "slow-agent", 2700);
    let spec = select_install_path("slow-agent", "harness.install", &install, None, None)
        .expect("shell path resolves");
    match spec {
        ResolvedInstallSpec::Shell { timeout, .. } => {
            assert_eq!(timeout, Duration::from_secs(2700));
        }
        other => panic!("expected the shell path, got {other:?}"),
    }
}

#[test]
fn absent_shell_timeout_keeps_the_default_budget() {
    let install = shell_install_set("true", "quick-agent");
    let spec = select_install_path("quick-agent", "harness.install", &install, None, None)
        .expect("shell path resolves");
    match spec {
        ResolvedInstallSpec::Shell { timeout, .. } => {
            assert_eq!(timeout, DEFAULT_INSTALLER_TIMEOUT);
            assert_eq!(timeout, Duration::from_secs(600));
        }
        other => panic!("expected the shell path, got {other:?}"),
    }
}

#[test]
fn timed_out_step_keeps_the_installer_output_it_captured() {
    let tempdir = TempDir::new().expect("tempdir");
    // One second of budget against a recipe that prints and then hangs, so the kill lands mid-run.
    let install = shell_install_set_with_timeout(
        "printf 'installer-progress\\n'; printf 'installer-warning\\n' >&2; sleep 30",
        "timeout-agent",
        1,
    );
    let entry = native_entry(
        "timeout-agent",
        "Timeout Agent",
        Some("docs/agents/timeout-agent.md"),
        harness_spec("timeout-agent", install),
    );

    let mut result = install_resolved_capture(
        &agent_config("timeout-agent"),
        &entry,
        HashMap::new(),
        tempdir.path(),
        tempdir.path(),
        None,
        tempdir.path(),
    );

    assert!(
        matches!(result.outcome, Err(StackError::AgentInstallerTimeout)),
        "the step must still fail as a timeout, got {:?}",
        result.outcome
    );
    assert_eq!(result.rows.len(), 1);
    let row = &mut result.rows[0];
    assert_eq!(row.status, "timeout");
    assert!(
        row.stdout.contains("installer-progress"),
        "stdout captured before the kill must survive, got `{}`",
        row.stdout
    );
    assert!(
        row.stderr.contains("installer-warning"),
        "stderr captured before the kill must survive, got `{}`",
        row.stderr
    );
    assert!(
        row.stderr.contains("[installer timed out after 1s]"),
        "the marker is appended to the captured stderr, got `{}`",
        row.stderr
    );

    let log_base = tempdir.path().join("installer-logs");
    persist_step_logs_to_disk(row, "timeout-agent", Some(&log_base)).expect("logs persist");
    let log_dir = row
        .log_dir
        .as_deref()
        .expect("timeout row records a log dir");
    let stdout_body =
        std::fs::read_to_string(std::path::Path::new(log_dir).join("stdout")).expect("stdout file");
    let stderr_body =
        std::fs::read_to_string(std::path::Path::new(log_dir).join("stderr")).expect("stderr file");
    assert!(stdout_body.contains("installer-progress"));
    assert!(stderr_body.contains("installer-warning"));
}

#[test]
fn escape_hatch_timeout_row_keeps_output_and_appends_the_marker() {
    let tempdir = TempDir::new().expect("tempdir");
    let captured = CapturedOutput {
        stdout: "escape hatch stdout".to_owned(),
        stderr: "escape hatch stderr".to_owned(),
        exit_status: None,
        timed_out_after: Some(DEFAULT_INSTALLER_TIMEOUT),
    };

    let result = finalize_shell_step(
        STEP_INSTALL,
        "2026-08-19T00:00:00.000000000Z".to_owned(),
        Ok(captured),
        "never-created",
        None,
        tempdir.path(),
        tempdir.path(),
    );

    assert!(
        matches!(result.outcome, Err(StackError::AgentInstallerTimeout)),
        "got {:?}",
        result.outcome
    );
    assert_eq!(result.row.status, "timeout");
    assert_eq!(result.row.stdout, "escape hatch stdout");
    assert!(result.row.stderr.starts_with("escape hatch stderr"));
    assert!(
        result
            .row
            .stderr
            .contains("[installer timed out after 600s]"),
        "got `{}`",
        result.row.stderr
    );
}
