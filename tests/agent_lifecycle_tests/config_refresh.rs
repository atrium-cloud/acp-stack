use reqwest::StatusCode;
use serde_json::Value;

use crate::common::agent::{AgentHarness, admin_bearer, http};

#[tokio::test]
async fn agent_restart_starts_when_not_running() {
    // Restart on a stopped supervisor degenerates into a plain start.
    let harness = AgentHarness::spawn().await;
    let client = http().await;
    let response = client
        .post(format!("{}/v1/agent/restart", harness.base_url))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("send restart");
    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();
    assert_eq!(status, StatusCode::OK, "body: {body_text}");
    let body: Value = serde_json::from_str(&body_text).expect("restart json");
    assert_eq!(body["ok"], true);
    assert!(body["data"]["started_at"].as_str().is_some());
    assert!(body["data"]["stopped_at"].as_str().is_some());
    assert!(body["data"]["capabilities"].is_object());
    // Prior process didn't exist, so prior_exit_status is null.
    assert!(body["data"]["prior_exit_status"].is_null());
}

#[tokio::test]
async fn agent_restart_picks_up_config_written_after_daemon_start() {
    // Regression: the restart handler must re-read config from disk, or the
    // in-memory `state.config` cache hands the supervisor a stale config.
    use serde_json::Value as JsonValue;

    let harness = AgentHarness::spawn().await;
    let client = http().await;
    let initial = std::fs::read_to_string(&harness.config_path).expect("read initial config");

    // Mutate the on-disk config AFTER the daemon cached its copy, pointing
    // `command` at an unresolvable path: reading from disk fails the spawn,
    // while a regression to the cached config would succeed.
    let mutated = initial.replace(
        &format!("command = \"{}\"", env!("CARGO_BIN_EXE_placebo-agent")),
        "command = \"/nonexistent/absolutely-not-a-binary\"",
    );
    std::fs::write(&harness.config_path, &mutated).expect("write mutated config");

    let response = client
        .post(format!("{}/v1/agent/restart", harness.base_url))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("send restart");
    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();
    assert!(
        status.is_server_error() || status == StatusCode::BAD_GATEWAY,
        "restart must fail when on-disk command no longer exists; got {status} body={body_text}",
    );
    let body: JsonValue = serde_json::from_str(&body_text).expect("restart err json");
    let code = body["error"]["code"].as_str().expect("error code present");
    // Either failure proves the on-disk command was honored; a fallback to
    // the cached config would return 200 instead.
    assert!(
        matches!(code, "agent.spawn_failed" | "agent.initialize_failed"),
        "unexpected error code `{code}`; expected agent.spawn_failed or agent.initialize_failed",
    );
}

#[tokio::test]
async fn agent_start_picks_up_config_written_after_daemon_start() {
    use serde_json::Value as JsonValue;

    let harness = AgentHarness::spawn().await;
    let client = http().await;
    let initial = std::fs::read_to_string(&harness.config_path).expect("read initial config");

    let mutated = initial.replace(
        &format!("command = \"{}\"", env!("CARGO_BIN_EXE_placebo-agent")),
        "command = \"/nonexistent/absolutely-not-a-binary\"",
    );
    std::fs::write(&harness.config_path, &mutated).expect("write mutated config");

    let response = client
        .post(format!("{}/v1/agent/start", harness.base_url))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("send start");
    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();
    assert!(
        status.is_server_error() || status == StatusCode::BAD_GATEWAY,
        "start must fail when on-disk command no longer exists; got {status} body={body_text}",
    );
    let body: JsonValue = serde_json::from_str(&body_text).expect("start err json");
    let code = body["error"]["code"].as_str().expect("error code present");
    assert!(
        matches!(code, "agent.spawn_failed" | "agent.initialize_failed"),
        "unexpected error code `{code}`; expected agent.spawn_failed or agent.initialize_failed",
    );
}
