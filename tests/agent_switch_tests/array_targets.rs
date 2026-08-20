use reqwest::StatusCode;
use serde_json::{Value, json};
use tempfile::TempDir;

use acp_stack::runtime::agent::switch_journal::switch_journal_path;

use crate::common::HomeEnvGuard;
use crate::common::agent::{
    AgentHarness, add_codex_placebo_target, add_hermes_placebo_target, add_kimi_placebo_target,
    admin_bearer, http, session_bearer, test_config,
};

#[tokio::test]
async fn agent_switch_selects_existing_array_target_config() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    let mut config = test_config();
    config.array.enabled = true;
    add_codex_placebo_target(&mut config);
    let harness = AgentHarness::spawn_with_config(config).await;
    let client = http().await;

    let primary_start = client
        .post(format!("{}/v1/agent/start", harness.base_url))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("start primary");
    assert_eq!(primary_start.status(), StatusCode::OK);

    let response = client
        .post(format!("{}/v1/agent/switch", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&json!({ "agent": "codex" }))
        .send()
        .await
        .expect("switch target");
    let status = response.status();
    let body: Value = response.json().await.expect("switch json");
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["agent_id"], "codex");
    assert_eq!(body["data"]["provider_status"], "selected");
    assert_eq!(body["data"]["restarted"], true);

    let status_body: Value = client
        .get(format!("{}/v1/array/status", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("send array status")
        .json()
        .await
        .expect("array status json");
    assert_eq!(status_body["data"]["primary_target"], "codex");
    let targets = status_body["data"]["targets"]
        .as_array()
        .expect("targets should be an array");
    assert!(
        targets
            .iter()
            .any(|target| { target["id"] == "codex" && target["process_state"] == "running" })
    );
    assert!(
        targets
            .iter()
            .any(|target| { target["id"] == "opencode" && target["process_state"] == "stopped" })
    );
}

#[tokio::test]
async fn agent_switch_same_target_bare_body_is_noop() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    let harness = AgentHarness::spawn().await;
    let client = http().await;
    let config_before = std::fs::read_to_string(&harness.config_path).expect("config before");

    let response = client
        .post(format!("{}/v1/agent/switch", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&json!({ "agent": "opencode" }))
        .send()
        .await
        .expect("switch to current target");
    let status = response.status();
    let body: Value = response.json().await.expect("switch json");
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["agent_id"], "opencode");
    assert_eq!(body["data"]["old_agent_id"], "opencode");
    assert_eq!(body["data"]["provider_status"], "no_op");
    assert_eq!(body["data"]["restarted"], false);
    assert_eq!(body["data"]["restart_started"], false);

    let config_after = std::fs::read_to_string(&harness.config_path).expect("config after");
    assert_eq!(config_after, config_before, "no-op must not rewrite config");
    let journal_path = switch_journal_path(&harness.config_path).expect("journal path");
    assert!(!journal_path.exists(), "no-op must not journal a switch");
}

#[tokio::test]
async fn agent_switch_same_target_with_provider_flag_is_rejected() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    let harness = AgentHarness::spawn().await;
    let client = http().await;

    let response = client
        .post(format!("{}/v1/agent/switch", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&json!({ "agent": "opencode", "provider": "openrouter" }))
        .send()
        .await
        .expect("switch with provider flag");
    let status = response.status();
    let body: Value = response.json().await.expect("switch json");
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert_eq!(body["error"]["code"], "request.invalid_param");
}

#[tokio::test]
async fn agent_switch_same_target_with_api_key_ref_flag_is_rejected() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    let harness = AgentHarness::spawn().await;
    let client = http().await;

    let response = client
        .post(format!("{}/v1/agent/switch", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&json!({ "agent": "opencode", "api_key_ref": "OPENCODE_API_KEY" }))
        .send()
        .await
        .expect("switch with api_key_ref flag");
    let status = response.status();
    let body: Value = response.json().await.expect("switch json");
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert_eq!(body["error"]["code"], "request.invalid_param");
}

#[tokio::test]
async fn agent_switch_same_target_with_drop_is_rejected() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    let harness = AgentHarness::spawn().await;
    let client = http().await;

    let response = client
        .post(format!("{}/v1/agent/switch", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&json!({ "agent": "opencode", "drop": true }))
        .send()
        .await
        .expect("switch with drop flag");
    let status = response.status();
    let body: Value = response.json().await.expect("switch json");
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert_eq!(body["error"]["code"], "request.invalid_param");
}

#[tokio::test]
async fn agent_switch_existing_kimi_target_reports_canonical_secret_ref() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    let mut secrets =
        acp_stack::secrets::SecretStore::open_or_create(tempdir.path()).expect("secret store");
    secrets
        .set_many([("KIMI_API_KEY", "kimi-secret")])
        .expect("kimi secret");

    let mut config = test_config();
    config.array.enabled = true;
    add_kimi_placebo_target(&mut config);
    let harness = AgentHarness::spawn_with_config(config).await;

    let response = http()
        .await
        .post(format!("{}/v1/agent/switch", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&json!({ "agent": "kimi" }))
        .send()
        .await
        .expect("switch target");
    let status = response.status();
    let body: Value = response.json().await.expect("switch json");

    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["agent_id"], "kimi");
    assert_eq!(body["data"]["provider_status"], "selected");
    assert_eq!(body["data"]["required_env_refs"], json!(["KIMI_API_KEY"]));
}

#[tokio::test]
async fn agent_switch_existing_hermes_target_reports_required_env_refs() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    let mut secrets =
        acp_stack::secrets::SecretStore::open_or_create(tempdir.path()).expect("secret store");
    secrets
        .set_many([("OPENROUTER_API_KEY", "hermes-secret")])
        .expect("hermes secret");

    let mut config = test_config();
    config.array.enabled = true;
    add_hermes_placebo_target(&mut config);
    let harness = AgentHarness::spawn_with_config(config).await;

    let response = http()
        .await
        .post(format!("{}/v1/agent/switch", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&json!({ "agent": "hermes" }))
        .send()
        .await
        .expect("switch target");
    let status = response.status();
    let body: Value = response.json().await.expect("switch json");

    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["agent_id"], "hermes");
    assert_eq!(body["data"]["provider_status"], "selected");
    assert_eq!(
        body["data"]["required_env_refs"],
        json!(["OPENROUTER_API_KEY"])
    );
}

#[tokio::test]
async fn agent_switch_to_existing_running_target_keeps_it_running() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    let mut config = test_config();
    config.array.enabled = true;
    add_codex_placebo_target(&mut config);
    let harness = AgentHarness::spawn_with_config(config).await;
    let client = http().await;

    let primary_start = client
        .post(format!("{}/v1/agent/start", harness.base_url))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("start primary");
    assert_eq!(primary_start.status(), StatusCode::OK);
    let secondary_start = client
        .post(format!(
            "{}/v1/array/targets/{}/start",
            harness.base_url, "codex"
        ))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("start secondary");
    assert_eq!(secondary_start.status(), StatusCode::OK);

    let response = client
        .post(format!("{}/v1/agent/switch", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&json!({ "agent": "codex" }))
        .send()
        .await
        .expect("switch target");
    let status = response.status();
    let body: Value = response.json().await.expect("switch json");
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["agent_id"], "codex");
    assert_eq!(body["data"]["restarted"], true);
    assert_eq!(body["data"]["restart_started"], false);

    let status_body: Value = client
        .get(format!("{}/v1/array/status", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("send array status")
        .json()
        .await
        .expect("array status json");
    let targets = status_body["data"]["targets"]
        .as_array()
        .expect("targets should be an array");
    assert!(
        targets
            .iter()
            .any(|target| { target["id"] == "codex" && target["process_state"] == "running" })
    );
    assert!(
        targets
            .iter()
            .any(|target| { target["id"] == "opencode" && target["process_state"] == "stopped" })
    );
}
