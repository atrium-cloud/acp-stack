use acp_stack::config::load_config_from_str;
use reqwest::StatusCode;
use serde_json::Value;
use tempfile::TempDir;

use crate::common::HomeEnvGuard;
use crate::common::agent::{
    AgentHarness, EnvVarGuard, admin_bearer, http, session_bearer, switch_mcp_config, test_config,
    write_amp_registry_override, write_config_options_fixture, write_kimi_registry_override,
};

#[tokio::test]
async fn agent_switch_requires_admin_key() {
    let harness = AgentHarness::spawn().await;
    let client = http().await;
    let response = client
        .post(format!("{}/v1/agent/switch", harness.base_url))
        .header("Authorization", session_bearer())
        .json(&serde_json::json!({ "agent_id": "kimi" }))
        .send()
        .await
        .expect("send switch");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "auth.wrong_kind");
}

#[tokio::test]
async fn agent_switch_installs_target_and_returns_model_choices() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    let config_dir = tempdir.path().join(".config/acp-stack");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    write_kimi_registry_override(&config_dir);
    let mut config = test_config();
    let workspace = tempdir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    config.workspace.root = workspace.to_string_lossy().into_owned();
    config.workspace.uploads = workspace.join("uploads").to_string_lossy().into_owned();
    config.agent.cwd = Some(config.workspace.root.clone());
    let mut secrets =
        acp_stack::secrets::SecretStore::open_or_create(tempdir.path()).expect("secret store");
    secrets
        .set_many([("KIMI_API_KEY", "kimi-secret")])
        .expect("kimi secret");
    let fixture_path = write_config_options_fixture(tempdir.path(), &["kimi/kimi-k3"]);
    let _fixture_guard = EnvVarGuard::set("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &fixture_path);

    let harness = AgentHarness::spawn_with_config(config).await;
    let client = http().await;
    let response = client
        .post(format!("{}/v1/agent/switch", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&serde_json::json!({ "agent_id": "kimi" }))
        .send()
        .await
        .expect("send switch");
    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();
    assert_eq!(status, StatusCode::OK, "body: {body_text}");
    let body: Value = serde_json::from_str(&body_text).expect("switch json");
    assert_eq!(body["data"]["old_agent_id"], "opencode");
    assert_eq!(body["data"]["agent_id"], "kimi");
    assert_eq!(body["data"]["provider_status"], "not_applicable");
    assert_eq!(body["data"]["set_model"], true);
    assert_eq!(
        body["data"]["follow_up"],
        "acps agent set --model <model-id>"
    );
    assert!(matches!(
        body["data"]["install"]["outcome"].as_str(),
        Some("installed" | "already_present")
    ));
    // Same `{value, display_name?}` shape `/v1/models` serves.
    assert_eq!(body["data"]["models"][0]["value"], "kimi/kimi-k3");
    assert!(body["data"]["models"][0].get("display_name").is_none());

    let written = std::fs::read_to_string(&harness.config_path).expect("read config");
    assert!(written.contains(r#"id = "kimi""#));
    assert!(written.contains(r#"env = ["KIMI_API_KEY"]"#));
    assert!(!written.contains("[agent.provider]"));
    assert!(!written.contains("model ="));
}

#[tokio::test]
async fn agent_switch_preserves_mcp_runtime_config() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    let config_dir = tempdir.path().join(".config/acp-stack");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    write_kimi_registry_override(&config_dir);
    let mut config = test_config();
    let workspace = tempdir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    config.workspace.root = workspace.to_string_lossy().into_owned();
    config.workspace.uploads = workspace.join("uploads").to_string_lossy().into_owned();
    config.agent.cwd = Some(config.workspace.root.clone());
    let expected_mcp = switch_mcp_config();
    config.mcp = expected_mcp.clone();
    let mut secrets =
        acp_stack::secrets::SecretStore::open_or_create(tempdir.path()).expect("secret store");
    secrets
        .set_many([("KIMI_API_KEY", "kimi-secret")])
        .expect("kimi secret");
    let fixture_path = write_config_options_fixture(tempdir.path(), &["kimi/kimi-k3"]);
    let _fixture_guard = EnvVarGuard::set("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &fixture_path);

    let harness = AgentHarness::spawn_with_config(config).await;
    let client = http().await;
    let response = client
        .post(format!("{}/v1/agent/switch", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&serde_json::json!({ "agent_id": "kimi" }))
        .send()
        .await
        .expect("send switch");
    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();
    assert_eq!(status, StatusCode::OK, "body: {body_text}");

    let written = std::fs::read_to_string(&harness.config_path).expect("read config");
    let written_config = load_config_from_str(&written).expect("written config parses");
    assert_eq!(written_config.agent.id, "kimi");
    assert_eq!(written_config.mcp, expected_mcp);
}

#[tokio::test]
async fn agent_switch_preserves_adapter_metadata_and_skips_model_follow_up() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    let config_dir = tempdir.path().join(".config/acp-stack");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    write_amp_registry_override(&config_dir);
    let mut config = test_config();
    let workspace = tempdir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    config.workspace.root = workspace.to_string_lossy().into_owned();
    config.workspace.uploads = workspace.join("uploads").to_string_lossy().into_owned();
    config.agent.cwd = Some(config.workspace.root.clone());
    let mut secrets =
        acp_stack::secrets::SecretStore::open_or_create(tempdir.path()).expect("secret store");
    secrets
        .set_many([("AMP_API_KEY", "amp-secret")])
        .expect("amp secret");

    let harness = AgentHarness::spawn_with_config(config).await;
    let client = http().await;
    let response = client
        .post(format!("{}/v1/agent/switch", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&serde_json::json!({ "agent_id": "amp" }))
        .send()
        .await
        .expect("send switch");
    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();
    assert_eq!(status, StatusCode::OK, "body: {body_text}");
    let body: Value = serde_json::from_str(&body_text).expect("switch json");
    assert_eq!(body["data"]["agent_id"], "amp");
    assert_eq!(body["data"]["set_model"], false);
    assert!(body["data"].get("follow_up").is_none());
    assert!(
        body["data"]["models"]
            .as_array()
            .expect("models array")
            .is_empty()
    );

    let response = client
        .get(format!("{}/v1/agent/status", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("send status");
    let status_body: Value = response.json().await.expect("status json");
    assert_eq!(status_body["data"]["agent"]["id"], "amp");
    assert_eq!(status_body["data"]["agent"]["adapter"]["id"], "true");
    assert_eq!(
        status_body["data"]["agent"]["adapter"]["upstream_agent"],
        "true"
    );
}
