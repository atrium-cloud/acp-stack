use acp_stack::state::{
    INIT_RUN_FAILED, INIT_RUN_SUCCEEDED, INIT_STEP_FAILED, INIT_STEP_PENDING, INIT_STEP_RUNNING,
    INIT_STEP_SKIPPED, INIT_STEP_SUCCEEDED, INSTALLER_METHOD_GITHUB, INSTALLER_METHOD_SHELL,
    INSTALLER_OPERATION_INSTALL, INSTALLER_STATUS_RUNNING, InstallerRunFinish, InstallerRunInput,
    NewInitRun, NewInitStep, NewStackUpdateRun, STACK_UPDATE_OPERATION_CHECK,
    STACK_UPDATE_STATUS_SUCCEEDED, StateStore,
};

#[test]
fn installer_runs_round_trip_records_and_returns_version() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    store
        .append_installer_run(InstallerRunInput {
            agent_id: "test-agent",
            started_at: "2026-05-21T00:00:00.000000000Z",
            finished_at: Some("2026-05-21T00:00:01.000000000Z"),
            status: "ran",
            stdout: "",
            stderr: "",
            exit_status: Some(0),
            step: "harness",
            version: Some("v1.2.3"),
            operation: INSTALLER_OPERATION_INSTALL,
            method: Some(INSTALLER_METHOD_GITHUB),
            log_dir: None,
            apply_run_id: None,
        })
        .expect("harness row should append");
    store
        .append_installer_run(InstallerRunInput {
            agent_id: "test-agent",
            started_at: "2026-05-21T00:00:02.000000000Z",
            finished_at: Some("2026-05-21T00:00:03.000000000Z"),
            status: "ran",
            stdout: "",
            stderr: "",
            exit_status: Some(0),
            step: "adapter",
            version: None,
            operation: INSTALLER_OPERATION_INSTALL,
            method: None,
            log_dir: None,
            apply_run_id: None,
        })
        .expect("adapter row should append");

    let history = store
        .query_installer_runs(10)
        .expect("history should query");
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].step, "adapter");
    assert_eq!(history[0].agent_id.as_deref(), Some("test-agent"));
    assert!(history[0].version.is_none());
    assert_eq!(history[0].operation, INSTALLER_OPERATION_INSTALL);
    assert!(history[0].method.is_none());
    assert_eq!(history[1].step, "harness");
    assert_eq!(history[1].agent_id.as_deref(), Some("test-agent"));
    assert_eq!(history[1].version.as_deref(), Some("v1.2.3"));
    assert_eq!(history[1].method.as_deref(), Some(INSTALLER_METHOD_GITHUB));

    let latest = store
        .latest_successful_installer_runs_for_agent("test-agent")
        .expect("latest-by-step should query");
    assert_eq!(latest.len(), 2);
    let harness = latest
        .iter()
        .find(|row| row.step == "harness")
        .expect("harness row");
    assert_eq!(harness.version.as_deref(), Some("v1.2.3"));
    let adapter = latest
        .iter()
        .find(|row| row.step == "adapter")
        .expect("adapter row");
    assert!(adapter.version.is_none());
}

#[test]
fn stack_update_runs_round_trip() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    store
        .append_stack_update_run(NewStackUpdateRun {
            operation: STACK_UPDATE_OPERATION_CHECK,
            status: STACK_UPDATE_STATUS_SUCCEEDED,
            current_version: "0.1.0",
            target_version: Some("0.1.1"),
            target_tag: Some("v0.1.1"),
            classification: Some("security-critical"),
            breaking: false,
            major_upgrade: false,
            policy: "security-critical",
            auto: true,
            message: Some("eligible"),
            payload_json: r#"{"decision":"install"}"#,
        })
        .expect("stack update row should append");

    let runs = store
        .query_stack_update_runs(10)
        .expect("stack update runs should query");
    assert_eq!(runs.len(), 1);
    let run = &runs[0];
    assert_eq!(run.operation, STACK_UPDATE_OPERATION_CHECK);
    assert_eq!(run.status, STACK_UPDATE_STATUS_SUCCEEDED);
    assert_eq!(run.current_version, "0.1.0");
    assert_eq!(run.target_version.as_deref(), Some("0.1.1"));
    assert_eq!(run.target_tag.as_deref(), Some("v0.1.1"));
    assert_eq!(run.classification.as_deref(), Some("security-critical"));
    assert!(run.auto);
    assert_eq!(run.policy, "security-critical");
    assert_eq!(run.message.as_deref(), Some("eligible"));
    assert_eq!(run.payload_json, r#"{"decision":"install"}"#);
}

#[test]
fn latest_successful_installer_runs_are_scoped_by_agent_id() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    store
        .append_installer_run(InstallerRunInput {
            agent_id: "first-agent",
            started_at: "2026-05-21T00:00:00.000000000Z",
            finished_at: Some("2026-05-21T00:00:01.000000000Z"),
            status: "ran",
            stdout: "",
            stderr: "",
            exit_status: Some(0),
            step: "harness",
            version: Some("v1.0.0"),
            operation: INSTALLER_OPERATION_INSTALL,
            method: Some(INSTALLER_METHOD_GITHUB),
            log_dir: None,
            apply_run_id: None,
        })
        .expect("first agent row should append");
    store
        .append_installer_run(InstallerRunInput {
            agent_id: "second-agent",
            started_at: "2026-05-21T00:00:02.000000000Z",
            finished_at: Some("2026-05-21T00:00:03.000000000Z"),
            status: "ran",
            stdout: "",
            stderr: "",
            exit_status: Some(0),
            step: "harness",
            version: Some("v9.9.9"),
            operation: INSTALLER_OPERATION_INSTALL,
            method: Some(INSTALLER_METHOD_GITHUB),
            log_dir: None,
            apply_run_id: None,
        })
        .expect("second agent row should append");

    let latest = store
        .latest_successful_installer_runs_for_agent("first-agent")
        .expect("latest-by-step should query");
    assert_eq!(latest.len(), 1);
    assert_eq!(latest[0].agent_id.as_deref(), Some("first-agent"));
    assert_eq!(latest[0].version.as_deref(), Some("v1.0.0"));
}

#[test]
fn installer_runs_round_trip_records_log_dir() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    store
        .append_installer_run(InstallerRunInput {
            agent_id: "test-agent",
            started_at: "2026-05-22T10:00:00.000000000Z",
            finished_at: Some("2026-05-22T10:00:01.000000000Z"),
            status: "ran",
            stdout: "out",
            stderr: "err",
            exit_status: Some(0),
            step: "harness",
            version: Some("v1.0.0"),
            operation: INSTALLER_OPERATION_INSTALL,
            method: Some(INSTALLER_METHOD_GITHUB),
            log_dir: Some("/var/lib/acp-stack/installer-logs/test-agent/2026-05-22T10:00:00.000000000Z/harness"),
            apply_run_id: Some("dap_test"),
        })
        .expect("row with log_dir should append");

    let history = store.query_installer_runs(10).expect("query");
    assert_eq!(history.len(), 1);
    assert_eq!(
        history[0].log_dir.as_deref(),
        Some("/var/lib/acp-stack/installer-logs/test-agent/2026-05-22T10:00:00.000000000Z/harness")
    );
    assert_eq!(history[0].apply_run_id.as_deref(), Some("dap_test"));
}

#[test]
fn latest_successful_installer_runs_skips_failed_rows() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    store
        .append_installer_run(InstallerRunInput {
            agent_id: "test-agent",
            started_at: "2026-05-21T00:00:00.000000000Z",
            finished_at: Some("2026-05-21T00:00:01.000000000Z"),
            status: "ran",
            stdout: "",
            stderr: "",
            exit_status: Some(0),
            step: "install",
            version: Some("v1.0.0"),
            operation: INSTALLER_OPERATION_INSTALL,
            method: Some(INSTALLER_METHOD_GITHUB),
            log_dir: None,
            apply_run_id: None,
        })
        .expect("first ran row should append");
    store
        .append_installer_run(InstallerRunInput {
            agent_id: "test-agent",
            started_at: "2026-05-21T00:00:02.000000000Z",
            finished_at: Some("2026-05-21T00:00:03.000000000Z"),
            status: "failed",
            stdout: "",
            stderr: "boom",
            exit_status: Some(1),
            step: "install",
            version: None,
            operation: INSTALLER_OPERATION_INSTALL,
            method: Some(INSTALLER_METHOD_GITHUB),
            log_dir: None,
            apply_run_id: None,
        })
        .expect("second failed row should append");

    let latest = store
        .latest_successful_installer_runs_for_agent("test-agent")
        .expect("latest-by-step should query");
    assert_eq!(latest.len(), 1);
    assert_eq!(latest[0].status, "ran");
    assert_eq!(latest[0].version.as_deref(), Some("v1.0.0"));
}

#[test]
fn init_run_records_round_trip_with_steps() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    let run = store
        .create_init_run(NewInitRun {
            runtime_user: Some("acp"),
            agent_id: Some("codex"),
            args_json: r#"{"agent":"codex"}"#,
        })
        .expect("init run should append");
    assert_eq!(run.status, "pending");
    assert!(run.id.starts_with("irun_"));

    let step = store
        .append_init_step(NewInitStep {
            run_id: &run.id,
            ordinal: 1,
            kind: "agent_install",
            payload_json: r#"{"step":"agent_install"}"#,
        })
        .expect("step should append");
    assert_eq!(step.status, INIT_STEP_PENDING);

    store
        .mark_init_step_running(&step.id)
        .expect("running mark should succeed");
    store
        .mark_init_step_succeeded(
            &step.id,
            Some("/tmp/install-logs/agent_install"),
            r#"{"installer_run_id":"ins_abc"}"#,
        )
        .expect("succeeded mark should succeed");

    let steps = store.query_init_steps(&run.id).expect("steps should query");
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].status, INIT_STEP_SUCCEEDED);
    assert_eq!(
        steps[0].log_dir.as_deref(),
        Some("/tmp/install-logs/agent_install"),
    );
    assert!(steps[0].started_at.is_some());
    assert!(steps[0].finished_at.is_some());

    store
        .finalize_init_run(&run.id, INIT_RUN_SUCCEEDED)
        .expect("finalize should succeed");
    let reloaded = store
        .lookup_init_run(&run.id)
        .expect("lookup should succeed")
        .expect("run row should exist");
    assert_eq!(reloaded.status, INIT_RUN_SUCCEEDED);
    assert!(reloaded.finished_at.is_some());
}

#[test]
fn init_step_skipped_keeps_started_at_and_clears_error() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    let run = store
        .create_init_run(NewInitRun {
            runtime_user: None,
            agent_id: None,
            args_json: "{}",
        })
        .expect("init run should append");
    let step = store
        .append_init_step(NewInitStep {
            run_id: &run.id,
            ordinal: 1,
            kind: "config_validate",
            payload_json: "{}",
        })
        .expect("step should append");

    store
        .mark_init_step_running(&step.id)
        .expect("running mark");
    store
        .mark_init_step_failed(
            &step.id,
            None,
            "config.invalid",
            "missing field foo",
            r#"{"attempt":1}"#,
        )
        .expect("failed mark");

    let steps = store.query_init_steps(&run.id).expect("steps");
    assert_eq!(steps[0].status, INIT_STEP_FAILED);
    assert_eq!(steps[0].error_kind.as_deref(), Some("config.invalid"));

    // The verifier-skipped path must clear the prior error tuple.
    store
        .mark_init_step_skipped(&step.id, r#"{"attempt":1,"verified":true}"#)
        .expect("skipped mark");
    let steps = store.query_init_steps(&run.id).expect("steps reloaded");
    assert_eq!(steps[0].status, INIT_STEP_SKIPPED);
    assert!(steps[0].error_kind.is_none());
    assert!(steps[0].error_detail.is_none());
    assert!(steps[0].payload_json.contains("\"verified\":true"));
}

#[test]
fn init_run_finalize_failed_records_terminal_status() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    let run = store
        .create_init_run(NewInitRun {
            runtime_user: None,
            agent_id: None,
            args_json: "{}",
        })
        .expect("init run should append");
    let step = store
        .append_init_step(NewInitStep {
            run_id: &run.id,
            ordinal: 1,
            kind: "agent_install",
            payload_json: "{}",
        })
        .expect("step should append");
    store.mark_init_step_running(&step.id).expect("running");
    store
        .mark_init_step_failed(&step.id, None, "installer.exit_nonzero", "exit=1", "{}")
        .expect("failed");
    store
        .finalize_init_run(&run.id, INIT_RUN_FAILED)
        .expect("finalize failed");

    let latest = store
        .latest_init_run()
        .expect("latest")
        .expect("latest row");
    assert_eq!(latest.id, run.id);
    assert_eq!(latest.status, INIT_RUN_FAILED);
    let _ = INIT_STEP_RUNNING;
}

#[test]
fn init_step_payload_must_be_valid_json() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    let run = store
        .create_init_run(NewInitRun {
            runtime_user: None,
            agent_id: None,
            args_json: "{}",
        })
        .expect("init run");
    let error = store
        .append_init_step(NewInitStep {
            run_id: &run.id,
            ordinal: 1,
            kind: "agent_install",
            payload_json: "not json",
        })
        .expect_err("invalid payload should be rejected");
    assert!(error.to_string().to_lowercase().contains("json"));
}

#[test]
fn duplicate_ordinal_within_run_is_rejected() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    let run = store
        .create_init_run(NewInitRun {
            runtime_user: None,
            agent_id: None,
            args_json: "{}",
        })
        .expect("init run");
    store
        .append_init_step(NewInitStep {
            run_id: &run.id,
            ordinal: 1,
            kind: "agent_install",
            payload_json: "{}",
        })
        .expect("first step");
    let error = store
        .append_init_step(NewInitStep {
            run_id: &run.id,
            ordinal: 1,
            kind: "config_validate",
            payload_json: "{}",
        })
        .expect_err("duplicate ordinal should fail UNIQUE");
    assert!(error.to_string().to_lowercase().contains("unique"));
}

fn running_installer_input<'a>(
    agent_id: &'a str,
    started_at: &'a str,
    step: &'a str,
) -> InstallerRunInput<'a> {
    InstallerRunInput {
        agent_id,
        started_at,
        finished_at: None,
        status: INSTALLER_STATUS_RUNNING,
        stdout: "",
        stderr: "",
        exit_status: None,
        step,
        version: None,
        operation: INSTALLER_OPERATION_INSTALL,
        method: Some(INSTALLER_METHOD_SHELL),
        log_dir: None,
        apply_run_id: None,
    }
}

#[test]
fn installer_run_running_row_is_visible_then_finalized_in_place() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    let running = store
        .append_installer_run(running_installer_input(
            "test-agent",
            "2026-05-21T00:00:00.000000000Z",
            "harness",
        ))
        .expect("running row should insert");

    let active = store
        .query_active_installer_runs(None)
        .expect("active query");
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].id, running.id);
    assert_eq!(active[0].status, INSTALLER_STATUS_RUNNING);
    assert!(active[0].finished_at.is_none());
    assert_eq!(active[0].method.as_deref(), Some(INSTALLER_METHOD_SHELL));

    store
        .finish_installer_run(
            &running.id,
            InstallerRunFinish {
                started_at: "2026-05-21T00:00:00.000000000Z",
                finished_at: Some("2026-05-21T00:00:42.000000000Z"),
                status: "ran",
                stdout: "done",
                stderr: "",
                exit_status: Some(0),
                version: Some("v1.2.3"),
                log_dir: Some("/tmp/installer-logs/test-agent/step"),
            },
        )
        .expect("finish should update the running row");

    // The same row id carries the terminal state; no second row appears.
    assert!(
        store
            .query_active_installer_runs(None)
            .expect("active query")
            .is_empty()
    );
    let history = store.query_installer_runs(10).expect("history");
    assert_eq!(history.len(), 1);
    let row = &history[0];
    assert_eq!(row.id, running.id);
    assert_eq!(row.status, "ran");
    assert_eq!(
        row.finished_at.as_deref(),
        Some("2026-05-21T00:00:42.000000000Z")
    );
    assert_eq!(row.stdout, "done");
    assert_eq!(row.exit_status, Some(0));
    assert_eq!(row.version.as_deref(), Some("v1.2.3"));
    assert_eq!(
        row.log_dir.as_deref(),
        Some("/tmp/installer-logs/test-agent/step")
    );
    // Identity fields fixed at insert survive the update untouched.
    assert_eq!(row.agent_id.as_deref(), Some("test-agent"));
    assert_eq!(row.step, "harness");
    assert_eq!(row.operation, INSTALLER_OPERATION_INSTALL);
}

#[test]
fn installer_run_concurrent_steps_track_independently() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    // Adapter-backed installs run harness and adapter steps on parallel threads.
    let harness = store
        .append_installer_run(running_installer_input(
            "test-agent",
            "2026-05-21T00:00:00.000000000Z",
            "harness",
        ))
        .expect("harness running row");
    let adapter = store
        .append_installer_run(running_installer_input(
            "test-agent",
            "2026-05-21T00:00:00.000000001Z",
            "adapter",
        ))
        .expect("adapter running row");

    let active = store
        .query_active_installer_runs(Some("test-agent"))
        .expect("active query");
    assert_eq!(active.len(), 2);
    // Oldest first, so a progress view renders steps in start order.
    assert_eq!(active[0].step, "harness");
    assert_eq!(active[1].step, "adapter");
    assert!(
        store
            .query_active_installer_runs(Some("other-agent"))
            .expect("active query")
            .is_empty()
    );

    store
        .finish_installer_run(
            &harness.id,
            InstallerRunFinish {
                started_at: "2026-05-21T00:00:00.000000000Z",
                finished_at: Some("2026-05-21T00:00:07.000000000Z"),
                status: "ran",
                stdout: "",
                stderr: "",
                exit_status: Some(0),
                version: None,
                log_dir: None,
            },
        )
        .expect("finish harness");

    let active = store
        .query_active_installer_runs(Some("test-agent"))
        .expect("active query");
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].id, adapter.id);
}

#[test]
fn finish_installer_run_rejects_unknown_and_completed_rows() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    let finish = InstallerRunFinish {
        started_at: "2026-05-21T00:00:00.000000000Z",
        finished_at: Some("2026-05-21T00:00:01.000000000Z"),
        status: "failed",
        stdout: "",
        stderr: "boom",
        exit_status: Some(1),
        version: None,
        log_dir: None,
    };
    store
        .finish_installer_run("run-nonexistent", finish.clone())
        .expect_err("finishing an unknown row must fail");

    let running = store
        .append_installer_run(running_installer_input(
            "test-agent",
            "2026-05-21T00:00:00.000000000Z",
            "install",
        ))
        .expect("running row");
    store
        .finish_installer_run(&running.id, finish.clone())
        .expect("first finish");
    // A second finish must not rewrite a completed audit row.
    store
        .finish_installer_run(&running.id, finish)
        .expect_err("double finish must fail");
    let history = store.query_installer_runs(10).expect("history");
    assert_eq!(history[0].status, "failed");
    assert_eq!(history[0].stderr, "boom");
}

#[test]
fn finish_installer_run_truncates_oversize_streams() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    let running = store
        .append_installer_run(running_installer_input(
            "test-agent",
            "2026-05-21T00:00:00.000000000Z",
            "install",
        ))
        .expect("running row");
    let oversize = "x".repeat(acp_stack::state::INSTALLER_OUTPUT_CAP_BYTES + 4096);
    store
        .finish_installer_run(
            &running.id,
            InstallerRunFinish {
                started_at: "2026-05-21T00:00:00.000000000Z",
                finished_at: Some("2026-05-21T00:00:01.000000000Z"),
                status: "ran",
                stdout: &oversize,
                stderr: "",
                exit_status: Some(0),
                version: None,
                log_dir: None,
            },
        )
        .expect("finish");
    let history = store.query_installer_runs(10).expect("history");
    assert!(
        history[0].stdout.len() < oversize.len(),
        "finish must apply the same defense-in-depth cap as insert"
    );
    assert!(history[0].stdout.contains("[truncated,"));
}
