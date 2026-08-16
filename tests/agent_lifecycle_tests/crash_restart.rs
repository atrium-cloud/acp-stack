use std::time::Duration;

use reqwest::StatusCode;
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::common::agent::{AgentHarness, admin_bearer, http, session_bearer, test_config};

#[cfg(unix)]
fn kill_process(pid: u32) {
    let result = unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    let result = if result == 0 {
        result
    } else {
        unsafe { libc::kill(pid as i32, libc::SIGKILL) }
    };
    assert_eq!(result, 0, "failed to SIGKILL fake agent pid {pid}");
}

#[cfg(unix)]
fn read_fake_agent_pid(path: &std::path::Path) -> u32 {
    std::fs::read_to_string(path)
        .expect("fake agent pid file")
        .trim()
        .parse()
        .expect("fake agent pid parses")
}

#[cfg(unix)]
async fn wait_for_agent_status(
    client: &reqwest::Client,
    base_url: &str,
    predicate: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let body: Value = client
            .get(format!("{base_url}/v1/agent/status"))
            .header("Authorization", session_bearer())
            .send()
            .await
            .expect("status")
            .json()
            .await
            .expect("status json");
        if predicate(&body["data"]) {
            return body["data"].clone();
        }
        if std::time::Instant::now() > deadline {
            panic!("agent status did not reach expected state; last body: {body}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(unix)]
async fn start_agent_for_crash_test(client: &reqwest::Client, base_url: &str) -> u32 {
    let response = client
        .post(format!("{base_url}/v1/agent/start"))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("start");
    let status = response.status();
    let body: Value = response.json().await.expect("start json");
    assert_eq!(status, StatusCode::OK, "body: {body}");
    body["data"]["pid"]
        .as_u64()
        .and_then(|pid| u32::try_from(pid).ok())
        .expect("start response pid")
}

#[cfg(unix)]
async fn create_session_for_crash_test(client: &reqwest::Client, base_url: &str) -> String {
    let response = client
        .post(format!("{base_url}/v1/sessions"))
        .header("Authorization", session_bearer())
        .json(&json!({}))
        .send()
        .await
        .expect("create session");
    let status = response.status();
    let body: Value = response.json().await.expect("create session json");
    assert_eq!(status, StatusCode::OK, "body: {body}");
    body["data"]["id"].as_str().expect("session id").to_owned()
}

#[cfg(unix)]
#[tokio::test]
async fn on_crash_policy_restarts_agent_and_allows_session_resume() {
    let tempdir = TempDir::new().expect("tempdir");
    let pid_path = tempdir.path().join("placebo-agent.pid");
    let mut config = test_config();
    let workspace = tempdir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    config.workspace.root = workspace.to_string_lossy().into_owned();
    config.workspace.uploads = workspace.join("uploads").to_string_lossy().into_owned();
    config.agent.cwd = Some(config.workspace.root.clone());
    config.agent.restart = "on-crash".to_owned();
    config.agent.args.extend([
        "--write-pid".to_owned(),
        pid_path.to_string_lossy().into_owned(),
    ]);
    let harness = AgentHarness::spawn_with_config(config).await;
    let client = http().await;

    let reported_first_pid = start_agent_for_crash_test(&client, &harness.base_url).await;
    let first_pid = read_fake_agent_pid(&pid_path);
    assert_eq!(first_pid, reported_first_pid);
    let session_id = create_session_for_crash_test(&client, &harness.base_url).await;

    kill_process(first_pid);
    let status = wait_for_agent_status(&client, &harness.base_url, |data| {
        data["process_state"].as_str() == Some("running")
            && data["pid"]
                .as_u64()
                .is_some_and(|pid| pid != u64::from(first_pid))
    })
    .await;
    let restarted_pid = status["pid"].as_u64().expect("restarted pid");
    assert_ne!(restarted_pid, u64::from(first_pid));

    let response = client
        .post(format!(
            "{}/v1/sessions/{}/resume",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({}))
        .send()
        .await
        .expect("resume");
    let resume_status = response.status();
    let resume_body: Value = response.json().await.expect("resume json");
    assert_eq!(resume_status, StatusCode::OK, "body: {resume_body}");

    let store = harness.state.lock().await;
    let lifecycle = store.query_agent_lifecycle(50).expect("lifecycle");
    drop(store);
    let kinds: Vec<&str> = lifecycle
        .iter()
        .map(|row| row.event_kind.as_str())
        .collect();
    assert!(kinds.contains(&"agent.exited"), "kinds: {kinds:?}");
    assert!(
        kinds.contains(&"agent.restart_scheduled"),
        "kinds: {kinds:?}"
    );
    assert!(
        lifecycle
            .iter()
            .filter(|row| row.event_kind == "agent.started")
            .count()
            >= 2,
        "expected initial and restarted agent.started rows, got {kinds:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn never_policy_does_not_restart_after_agent_crash() {
    let tempdir = TempDir::new().expect("tempdir");
    let pid_path = tempdir.path().join("placebo-agent.pid");
    let mut config = test_config();
    let workspace = tempdir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    config.workspace.root = workspace.to_string_lossy().into_owned();
    config.workspace.uploads = workspace.join("uploads").to_string_lossy().into_owned();
    config.agent.cwd = Some(config.workspace.root.clone());
    config.agent.restart = "never".to_owned();
    config.agent.args.extend([
        "--write-pid".to_owned(),
        pid_path.to_string_lossy().into_owned(),
    ]);
    let harness = AgentHarness::spawn_with_config(config).await;
    let client = http().await;

    let reported_first_pid = start_agent_for_crash_test(&client, &harness.base_url).await;
    let first_pid = read_fake_agent_pid(&pid_path);
    assert_eq!(first_pid, reported_first_pid);
    let session_id = create_session_for_crash_test(&client, &harness.base_url).await;

    kill_process(first_pid);
    wait_for_agent_status(&client, &harness.base_url, |data| {
        data["process_state"].as_str() == Some("stopped") && data["pid"].is_null()
    })
    .await;

    let response = client
        .post(format!(
            "{}/v1/sessions/{}/resume",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({}))
        .send()
        .await
        .expect("resume");
    let resume_status = response.status();
    let resume_body: Value = response.json().await.expect("resume json");
    assert_eq!(resume_status, StatusCode::CONFLICT, "body: {resume_body}");
    assert_eq!(resume_body["error"]["code"], "agent.not_running");

    let store = harness.state.lock().await;
    let lifecycle = store.query_agent_lifecycle(50).expect("lifecycle");
    drop(store);
    let kinds: Vec<&str> = lifecycle
        .iter()
        .map(|row| row.event_kind.as_str())
        .collect();
    assert!(kinds.contains(&"agent.exited"), "kinds: {kinds:?}");
    assert!(kinds.contains(&"agent.restart_skipped"), "kinds: {kinds:?}");
    assert!(
        !kinds.contains(&"agent.restart_scheduled"),
        "never policy must not schedule restart: {kinds:?}"
    );
}
