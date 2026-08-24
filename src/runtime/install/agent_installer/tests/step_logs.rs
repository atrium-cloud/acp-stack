use super::super::*;
use super::support::*;
use tempfile::TempDir;

#[test]
fn persist_step_logs_writes_files_and_sets_log_dir() {
    let tempdir = TempDir::new().expect("tempdir");
    let mut row = InstallerRowDraft {
        started_at: "2026-05-22T00:00:00.123456789Z".to_owned(),
        finished_at: Some("2026-05-22T00:00:01.000000000Z".to_owned()),
        status: "ran".into(),
        stdout: "hello stdout\n".into(),
        stderr: "hello stderr\n".into(),
        exit_status: Some(0),
        step: "harness".into(),
        method: Some(INSTALL_METHOD_GITHUB.to_owned()),
        version: Some("v1.0.0".into()),
        log_dir: None,
        persisted_run_id: None,
    };
    persist_step_logs_to_disk(&mut row, "test-agent", Some(tempdir.path()))
        .expect("logs should persist");
    let log_dir = row.log_dir.as_deref().expect("log_dir set on success");
    let stdout_path = std::path::Path::new(log_dir).join("stdout");
    let stderr_path = std::path::Path::new(log_dir).join("stderr");
    let stdout_body = std::fs::read_to_string(&stdout_path).expect("stdout written");
    let stderr_body = std::fs::read_to_string(&stderr_path).expect("stderr written");
    assert_eq!(stdout_body, "hello stdout\n");
    assert_eq!(stderr_body, "hello stderr\n");
}

#[test]
fn persist_step_logs_skips_when_streams_empty() {
    let tempdir = TempDir::new().expect("tempdir");
    let mut row = InstallerRowDraft {
        started_at: "2026-05-22T00:00:00.000000000Z".to_owned(),
        finished_at: Some("2026-05-22T00:00:00.000000000Z".to_owned()),
        status: "skipped".into(),
        stdout: String::new(),
        stderr: String::new(),
        exit_status: Some(0),
        step: "install".into(),
        method: Some(INSTALL_METHOD_SHELL.to_owned()),
        version: None,
        log_dir: None,
        persisted_run_id: None,
    };
    persist_step_logs_to_disk(&mut row, "test-agent", Some(tempdir.path()))
        .expect("empty streams should be a no-op");
    assert!(
        row.log_dir.is_none(),
        "log_dir must stay None when both streams are empty"
    );
}

#[test]
fn persist_step_logs_is_a_no_op_when_log_base_is_none() {
    let mut row = InstallerRowDraft {
        started_at: "2026-05-22T00:00:00.000000000Z".to_owned(),
        finished_at: None,
        status: "ran".into(),
        stdout: "anything".into(),
        stderr: String::new(),
        exit_status: Some(0),
        step: "harness".into(),
        method: Some(INSTALL_METHOD_SHELL.to_owned()),
        version: None,
        log_dir: None,
        persisted_run_id: None,
    };
    persist_step_logs_to_disk(&mut row, "test-agent", None)
        .expect("missing log base should be a no-op");
    assert!(row.log_dir.is_none());
}

#[test]
fn installer_log_persist_failure_prevents_history_row() {
    let tempdir = TempDir::new().expect("tempdir");
    let (_state_dir, store) = open_store();
    let log_base_file = tempdir.path().join("not-a-directory");
    std::fs::write(&log_base_file, b"file blocks log dir").expect("write blocker file");
    let install = install_config(
        "printf 'audit stdout\n'; mkdir -p bin; printf agent > bin/test-agent",
        "bin/test-agent",
    );

    let err = run_installer(
        "test-agent",
        &install,
        None,
        HashMap::new(),
        tempdir.path(),
        &store,
        Some(&log_base_file),
    )
    .expect_err("log persistence failure must fail install wrapper");

    assert!(matches!(err, StackError::AgentInstallerLogPersist { .. }));
    // History must never claim a completed run whose audit copy was lost: the `running` row
    // finalizes as `error` with no `log_dir`.
    let runs = store.query_installer_runs(10).expect("query");
    assert_eq!(runs.len(), 1, "the running row is finalized in place");
    assert_eq!(runs[0].status, "error");
    assert!(runs[0].log_dir.is_none());
    assert!(runs[0].stderr.contains("finalize failed"));
    assert!(
        store
            .query_active_installer_runs(None)
            .expect("active query")
            .is_empty(),
        "no step may be left reading as in-flight"
    );
}
