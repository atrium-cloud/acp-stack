use acp_stack::config::{ArrayTargetConfig, Config};
use reqwest::StatusCode;
use serde_json::Value;

use crate::common::agent::{
    AgentHarness, add_codex_placebo_target, admin_bearer, http, session_bearer, test_config,
};

#[tokio::test]
async fn array_start_sees_target_added_after_daemon_start() {
    let harness = AgentHarness::spawn().await;
    let mut updated =
        Config::load_from_path(&harness.config_path).expect("config should load from disk");
    updated.array.enabled = true;
    let mut secondary = updated.agent.clone();
    secondary.id = "placebo-secondary".to_owned();
    secondary.name = "Placebo Secondary".to_owned();
    updated.array.targets.push(ArrayTargetConfig {
        id: "placebo-secondary".to_owned(),
        agent: secondary,
    });
    std::fs::write(
        &harness.config_path,
        updated.to_canonical_toml().expect("canonical config"),
    )
    .expect("config should be rewritten");

    let client = http().await;
    let response = client
        .post(format!(
            "{}/v1/array/targets/{}/start",
            harness.base_url, "placebo-secondary"
        ))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("send array start");
    let status = response.status();
    let body: Value = response.json().await.expect("array start json");

    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["ok"], true);
    assert!(body["data"]["capabilities"].is_object());
}

#[tokio::test]
async fn array_start_rejects_non_default_target_when_array_is_off() {
    let mut config = test_config();
    add_codex_placebo_target(&mut config);
    let harness = AgentHarness::spawn_with_config(config).await;
    let client = http().await;

    let response = client
        .post(format!(
            "{}/v1/array/targets/{}/start",
            harness.base_url, "codex"
        ))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("send array start");
    let status = response.status();
    let body: Value = response.json().await.expect("array start json");

    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert_eq!(body["error"]["code"], "request.invalid_param");
    assert!(
        body["error"]["message"]
            .as_str()
            .expect("message should be string")
            .contains("Array mode is off")
    );
}

#[tokio::test]
async fn array_status_reports_daemon_targets() {
    let mut config = test_config();
    config.array.enabled = true;
    add_codex_placebo_target(&mut config);
    let harness = AgentHarness::spawn_with_config(config).await;
    let client = http().await;

    let response = client
        .get(format!("{}/v1/array/status", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("send array status");
    let status = response.status();
    let body: Value = response.json().await.expect("array status json");

    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["enabled"], true);
    assert_eq!(body["data"]["primary_target"], "opencode");
    let targets = body["data"]["targets"]
        .as_array()
        .expect("targets should be an array");
    assert!(
        targets
            .iter()
            .any(|target| { target["id"] == "opencode" && target["process_state"] == "stopped" })
    );
    assert!(
        targets
            .iter()
            .any(|target| { target["id"] == "codex" && target["process_state"] == "stopped" })
    );
}

#[tokio::test]
async fn array_status_rejects_admin_key() {
    // Strict tiering: the read-only array status route is session-tier and must
    // reject a valid admin key with auth.wrong_kind (no admin superset).
    let harness = AgentHarness::spawn().await;
    let client = http().await;
    let response = client
        .get(format!("{}/v1/array/status", harness.base_url))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("send array status");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "auth.wrong_kind");
}

#[tokio::test]
async fn array_capabilities_rejects_admin_key() {
    // Session-tier per-target capabilities route also rejects admin keys.
    let harness = AgentHarness::spawn().await;
    let client = http().await;
    let response = client
        .get(format!(
            "{}/v1/array/targets/{}/capabilities",
            harness.base_url, "opencode"
        ))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("send array capabilities");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "auth.wrong_kind");
}

#[tokio::test]
async fn array_target_mutations_reject_session_key() {
    // The four state-altering per-target routes are admin-tier; a session key
    // must never gain the power to install/start/stop/restart an agent process.
    // The require_admin layer rejects before the handler routes on target_id,
    // so this guards against an accidental downgrade into the session router.
    let harness = AgentHarness::spawn().await;
    let client = http().await;
    for action in ["install", "start", "stop", "restart"] {
        let response = client
            .post(format!(
                "{}/v1/array/targets/{}/{}",
                harness.base_url, "opencode", action
            ))
            .header("Authorization", session_bearer())
            .send()
            .await
            .expect("send array mutation");
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "array {action} must reject a session key",
        );
        let body: Value = response.json().await.expect("json");
        assert_eq!(
            body["error"]["code"], "auth.wrong_kind",
            "array {action} wrong-tier code",
        );
    }
}

#[tokio::test]
async fn array_target_stop_and_restart_lifecycle() {
    // Exercise the previously-untested stop/restart routes for a secondary
    // target: start -> running, stop -> stopped, restart -> running.
    let mut config = test_config();
    config.array.enabled = true;
    add_codex_placebo_target(&mut config);
    let harness = AgentHarness::spawn_with_config(config).await;
    let client = http().await;

    let start = client
        .post(format!(
            "{}/v1/array/targets/{}/start",
            harness.base_url, "codex"
        ))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("send array start");
    assert_eq!(start.status(), StatusCode::OK);
    let start_body: Value = start.json().await.expect("start json");
    assert!(start_body["data"]["capabilities"].is_object());

    let stop = client
        .post(format!(
            "{}/v1/array/targets/{}/stop",
            harness.base_url, "codex"
        ))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("send array stop");
    assert_eq!(stop.status(), StatusCode::OK);

    let status: Value = client
        .get(format!("{}/v1/array/status", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("send array status")
        .json()
        .await
        .expect("status json");
    let codex_state = status["data"]["targets"]
        .as_array()
        .expect("targets array")
        .iter()
        .find(|target| target["id"] == "codex")
        .map(|target| target["process_state"].clone())
        .expect("codex target present");
    assert_eq!(codex_state, "stopped");

    let restart = client
        .post(format!(
            "{}/v1/array/targets/{}/restart",
            harness.base_url, "codex"
        ))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("send array restart");
    assert_eq!(restart.status(), StatusCode::OK);
    let restart_body: Value = restart.json().await.expect("restart json");
    assert!(restart_body["data"]["capabilities"].is_object());
}

#[tokio::test]
async fn agent_aliases_follow_default_target_changed_on_disk() {
    let mut config = test_config();
    config.array.enabled = true;
    add_codex_placebo_target(&mut config);
    let harness = AgentHarness::spawn_with_config(config).await;

    let mut updated =
        Config::load_from_path(&harness.config_path).expect("config should load from disk");
    let codex_agent = updated
        .array
        .target("codex")
        .expect("codex target exists")
        .agent
        .clone();
    updated.array.primary_target = "codex".to_owned();
    updated.agent = codex_agent;
    std::fs::write(
        &harness.config_path,
        updated.to_canonical_toml().expect("canonical config"),
    )
    .expect("config should be rewritten");

    let client = http().await;
    let start_response = client
        .post(format!("{}/v1/agent/start", harness.base_url))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("send agent start");
    let start_status = start_response.status();
    let start_body: Value = start_response.json().await.expect("start json");
    assert_eq!(start_status, StatusCode::OK, "body: {start_body}");

    let status_body: Value = client
        .get(format!("{}/v1/agent/status", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("send agent status")
        .json()
        .await
        .expect("status json");
    assert_eq!(status_body["data"]["agent"]["id"], "codex");
    assert_eq!(status_body["data"]["process_state"], "running");
}

#[tokio::test]
async fn health_ready_follows_default_target_changed_on_disk() {
    let mut config = test_config();
    config.array.enabled = true;
    add_codex_placebo_target(&mut config);
    let harness = AgentHarness::spawn_with_config(config).await;

    let mut updated =
        Config::load_from_path(&harness.config_path).expect("config should load from disk");
    let codex_agent = updated
        .array
        .target("codex")
        .expect("codex target exists")
        .agent
        .clone();
    updated.array.primary_target = "codex".to_owned();
    updated.agent = codex_agent;
    std::fs::write(
        &harness.config_path,
        updated.to_canonical_toml().expect("canonical config"),
    )
    .expect("config should be rewritten");

    let body: Value = http()
        .await
        .get(format!("{}/v1/health/ready", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("send health")
        .json()
        .await
        .expect("health json");
    assert_eq!(body["data"]["agent"]["id"], "codex");
}

#[tokio::test]
async fn agent_restart_requires_admin_key() {
    let harness = AgentHarness::spawn().await;
    let client = http().await;
    let response = client
        .post(format!("{}/v1/agent/restart", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "auth.wrong_kind");
}
