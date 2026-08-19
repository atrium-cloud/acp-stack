use reqwest::StatusCode;
use serde_json::Value;

use acp_stack::state::{
    INSTALLER_METHOD_SHELL, INSTALLER_OPERATION_INSTALL, INSTALLER_STATUS_RUNNING,
    InstallerRunInput, StateStore,
};

mod common;
use common::api::{ADMIN_KEY, SESSION_KEY, ServerHarness};

fn finished_input<'a>(
    agent_id: &'a str,
    started_at: &'a str,
    step: &'a str,
) -> InstallerRunInput<'a> {
    InstallerRunInput {
        agent_id,
        started_at,
        finished_at: Some("2026-05-21T00:00:42.000000000Z"),
        status: "ran",
        stdout: "preview",
        stderr: "",
        exit_status: Some(0),
        step,
        version: Some("v1.2.3"),
        operation: INSTALLER_OPERATION_INSTALL,
        method: Some(INSTALLER_METHOD_SHELL),
        log_dir: Some("/nonexistent/log/dir"),
        apply_run_id: None,
    }
}

fn running_input<'a>(
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

async fn get_runs(harness: &ServerHarness, query: &str, key: &str) -> (StatusCode, Value) {
    let response = reqwest::Client::new()
        .get(format!("{}/v1/installer/runs{query}", harness.base_url))
        .header("Authorization", format!("Bearer {key}"))
        .send()
        .await
        .expect("send");
    let status = response.status();
    let body: Value = response.json().await.expect("json");
    (status, body)
}

#[tokio::test]
async fn installer_runs_active_returns_only_running_rows_with_elapsed() {
    let harness = ServerHarness::spawn().await;
    {
        let guard = harness.state.lock().await;
        guard
            .append_installer_run(finished_input(
                "hermes-agent",
                "2026-05-21T00:00:00.000000000Z",
                "harness",
            ))
            .expect("finished row");
        guard
            .append_installer_run(running_input(
                "hermes-agent",
                "2026-05-21T00:01:00.000000000Z",
                "adapter",
            ))
            .expect("running row");
    }

    let (status, body) = get_runs(&harness, "?active=true", SESSION_KEY).await;
    assert_eq!(status, StatusCode::OK);
    let runs = body["data"]["runs"].as_array().expect("runs array");
    assert_eq!(runs.len(), 1, "only the in-flight step is active");
    let run = &runs[0];
    assert_eq!(run["agent_id"], "hermes-agent");
    assert_eq!(run["step"], "adapter");
    assert_eq!(run["method"], "shell");
    assert_eq!(run["operation"], "install");
    assert_eq!(run["status"], "running");
    assert!(run["started_at"].is_string());
    assert!(run["finished_at"].is_null());
    // Elapsed is computed server-side for running rows so pollers need no
    // clock sync with the daemon.
    let elapsed = run["elapsed_seconds"].as_i64().expect("elapsed present");
    assert!(elapsed >= 0);
    // Step metadata only: log previews and on-disk paths never leave the daemon.
    assert!(run.get("stdout").is_none());
    assert!(run.get("stderr").is_none());
    assert!(run.get("log_dir").is_none());
}

#[tokio::test]
async fn installer_runs_lists_history_newest_first_without_elapsed() {
    let harness = ServerHarness::spawn().await;
    {
        let guard = harness.state.lock().await;
        guard
            .append_installer_run(finished_input(
                "hermes-agent",
                "2026-05-21T00:00:00.000000000Z",
                "harness",
            ))
            .expect("finished row");
        guard
            .append_installer_run(running_input(
                "hermes-agent",
                "2026-05-21T00:01:00.000000000Z",
                "adapter",
            ))
            .expect("running row");
    }

    let (status, body) = get_runs(&harness, "", SESSION_KEY).await;
    assert_eq!(status, StatusCode::OK);
    let runs = body["data"]["runs"].as_array().expect("runs array");
    assert_eq!(runs.len(), 2);
    // History order is newest first; the finished row carries the terminal
    // fields and no elapsed time.
    assert_eq!(runs[0]["step"], "adapter");
    assert_eq!(runs[1]["step"], "harness");
    assert_eq!(runs[1]["status"], "ran");
    assert_eq!(runs[1]["finished_at"], "2026-05-21T00:00:42.000000000Z");
    assert_eq!(runs[1]["version"], "v1.2.3");
    assert_eq!(runs[1]["exit_status"], 0);
    assert!(runs[1].get("elapsed_seconds").is_none());
}

#[tokio::test]
async fn installer_runs_filters_by_agent() {
    let harness = ServerHarness::spawn().await;
    {
        let guard = harness.state.lock().await;
        guard
            .append_installer_run(running_input(
                "hermes-agent",
                "2026-05-21T00:01:00.000000000Z",
                "harness",
            ))
            .expect("hermes row");
        guard
            .append_installer_run(running_input(
                "opencode",
                "2026-05-21T00:02:00.000000000Z",
                "install",
            ))
            .expect("opencode row");
    }

    let (status, body) = get_runs(&harness, "?active=true&agent=hermes-agent", SESSION_KEY).await;
    assert_eq!(status, StatusCode::OK);
    let runs = body["data"]["runs"].as_array().expect("runs array");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["agent_id"], "hermes-agent");

    let (status, body) = get_runs(&harness, "?agent=opencode", SESSION_KEY).await;
    assert_eq!(status, StatusCode::OK);
    let runs = body["data"]["runs"].as_array().expect("runs array");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["agent_id"], "opencode");
}

#[tokio::test]
async fn installer_runs_is_session_tier() {
    let harness = ServerHarness::spawn().await;
    // Strict tiering: an admin key on a session-tier read route is rejected.
    let (status, _) = get_runs(&harness, "", ADMIN_KEY).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn installer_runs_active_reads_rows_written_via_another_connection() {
    let harness = ServerHarness::spawn().await;
    // Write through a second connection, the way deps apply effectively
    // publishes progress while holding the daemon's shared store mutex for
    // its whole run: the endpoint reads via its own short-lived connection
    // and must see the autocommit `running` row without waiting on that
    // mutex (WAL).
    let writer = StateStore::open(&harness.state_path).expect("second connection");
    writer
        .append_installer_run(running_input(
            "deps_apply",
            "2026-05-21T00:01:00.000000000Z",
            "deps_apply",
        ))
        .expect("running row via second connection");

    let (status, body) = get_runs(&harness, "?active=true", SESSION_KEY).await;
    assert_eq!(status, StatusCode::OK);
    let runs = body["data"]["runs"].as_array().expect("runs array");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["agent_id"], "deps_apply");
    assert_eq!(runs[0]["status"], "running");
    assert!(runs[0]["elapsed_seconds"].is_i64());
}
