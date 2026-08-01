#![cfg(feature = "test-fixtures")]

//! End-to-end coverage for `POST /v1/agent/switch` and the native-config
//! import routes: target install, skill porting, provider-secret migration,
//! source cleanup, and the inspect/import/cancel rollback loop.
//!
//! All tests drive a real `acps` HTTP server against a `Config` whose
//! `[agent].command` is the standalone placebo ACP fixture.

use std::time::Duration;

use acp_stack::config::load_config_from_str;
use acp_stack::runtime::agent::model_discovery::fetch_session_config_with_timeout;
use reqwest::StatusCode;
use serde_json::{Value, json};
use tempfile::TempDir;

mod common;
use common::HomeEnvGuard;
use common::agent::{
    AgentHarness, EnvVarGuard, add_codex_placebo_target, add_kimi_placebo_target, admin_bearer,
    http, session_bearer, switch_mcp_config, test_config,
    write_amp_linked_skills_registry_override, write_amp_registry_override,
    write_config_options_fixture, write_cursor_registry_override, write_installed_skill,
    write_pi_registry_override,
};

#[tokio::test]
async fn agent_switch_requires_admin_key() {
    let harness = AgentHarness::spawn().await;
    let client = http().await;
    let response = client
        .post(format!("{}/v1/agent/switch", harness.base_url))
        .header("Authorization", session_bearer())
        .json(&serde_json::json!({ "agent": "cursor" }))
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
    write_cursor_registry_override(&config_dir);
    let mut config = test_config();
    let workspace = tempdir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    config.workspace.root = workspace.to_string_lossy().into_owned();
    config.workspace.uploads = workspace.join("uploads").to_string_lossy().into_owned();
    config.agent.cwd = Some(config.workspace.root.clone());
    let mut secrets =
        acp_stack::secrets::SecretStore::open_or_create(tempdir.path()).expect("secret store");
    secrets
        .set_many([("CURSOR_API_KEY", "cursor-secret")])
        .expect("cursor secret");
    let fixture_path = write_config_options_fixture(tempdir.path(), &["cursor/gpt-5.5"]);
    let _fixture_guard = EnvVarGuard::set("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &fixture_path);

    let harness = AgentHarness::spawn_with_config(config).await;
    let client = http().await;
    let response = client
        .post(format!("{}/v1/agent/switch", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&serde_json::json!({ "agent": "cursor" }))
        .send()
        .await
        .expect("send switch");
    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();
    assert_eq!(status, StatusCode::OK, "body: {body_text}");
    let body: Value = serde_json::from_str(&body_text).expect("switch json");
    assert_eq!(body["data"]["old_agent_id"], "opencode");
    assert_eq!(body["data"]["agent_id"], "cursor");
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
    assert_eq!(body["data"]["models"][0], "cursor/gpt-5.5");

    let written = std::fs::read_to_string(&harness.config_path).expect("read config");
    assert!(written.contains(r#"id = "cursor""#));
    assert!(written.contains(r#"env = ["CURSOR_API_KEY"]"#));
    assert!(!written.contains("[agent.provider]"));
    assert!(!written.contains("model ="));
}

#[tokio::test]
async fn agent_switch_preserves_mcp_runtime_config() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    let config_dir = tempdir.path().join(".config/acp-stack");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    write_cursor_registry_override(&config_dir);
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
        .set_many([("CURSOR_API_KEY", "cursor-secret")])
        .expect("cursor secret");
    let fixture_path = write_config_options_fixture(tempdir.path(), &["cursor/gpt-5.5"]);
    let _fixture_guard = EnvVarGuard::set("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &fixture_path);

    let harness = AgentHarness::spawn_with_config(config).await;
    let client = http().await;
    let response = client
        .post(format!("{}/v1/agent/switch", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&serde_json::json!({ "agent": "cursor" }))
        .send()
        .await
        .expect("send switch");
    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();
    assert_eq!(status, StatusCode::OK, "body: {body_text}");

    let written = std::fs::read_to_string(&harness.config_path).expect("read config");
    let written_config = load_config_from_str(&written).expect("written config parses");
    assert_eq!(written_config.agent.id, "cursor");
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
        .json(&serde_json::json!({ "agent": "amp" }))
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

#[tokio::test]
async fn agent_switch_ports_skills_to_target_install_dir() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    let config_dir = tempdir.path().join(".config/acp-stack");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    write_amp_registry_override(&config_dir);
    write_installed_skill(
        &tempdir.path().join(".agents/skills"),
        "repo-map",
        "# Source Repo Map\n",
    );
    let mut secrets =
        acp_stack::secrets::SecretStore::open_or_create(tempdir.path()).expect("secret store");
    secrets
        .set_many([("AMP_API_KEY", "amp-secret")])
        .expect("amp secret");

    let mut config = test_config();
    let workspace = tempdir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    config.workspace.root = workspace.to_string_lossy().into_owned();
    config.workspace.uploads = workspace.join("uploads").to_string_lossy().into_owned();
    config.agent.cwd = Some(config.workspace.root.clone());
    let harness = AgentHarness::spawn_with_config(config).await;
    let response = http()
        .await
        .post(format!("{}/v1/agent/switch", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&serde_json::json!({ "agent": "amp" }))
        .send()
        .await
        .expect("send switch");
    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();
    assert_eq!(status, StatusCode::OK, "body: {body_text}");
    let body: Value = serde_json::from_str(&body_text).expect("switch json");

    assert_eq!(body["data"]["agent_id"], "amp");
    assert_eq!(body["data"]["skills_port"]["status"], "copied");
    assert_eq!(body["data"]["skills_port"]["copied"][0]["name"], "repo-map");
    assert!(
        tempdir
            .path()
            .join(".config/agents/skills/repo-map/SKILL.md")
            .is_file()
    );
}

#[tokio::test]
async fn agent_switch_links_skills_into_target_link_dir() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    let config_dir = tempdir.path().join(".config/acp-stack");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    write_amp_linked_skills_registry_override(&config_dir);
    write_installed_skill(
        &tempdir.path().join(".agents/skills"),
        "repo-map",
        "# Source Repo Map\n",
    );
    let mut secrets =
        acp_stack::secrets::SecretStore::open_or_create(tempdir.path()).expect("secret store");
    secrets
        .set_many([("AMP_API_KEY", "amp-secret")])
        .expect("amp secret");

    let mut config = test_config();
    let workspace = tempdir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    config.workspace.root = workspace.to_string_lossy().into_owned();
    config.workspace.uploads = workspace.join("uploads").to_string_lossy().into_owned();
    config.agent.cwd = Some(config.workspace.root.clone());
    let harness = AgentHarness::spawn_with_config(config).await;
    let response = http()
        .await
        .post(format!("{}/v1/agent/switch", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&serde_json::json!({ "agent": "amp" }))
        .send()
        .await
        .expect("send switch");
    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();
    assert_eq!(status, StatusCode::OK, "body: {body_text}");
    let body: Value = serde_json::from_str(&body_text).expect("switch json");

    assert_eq!(body["data"]["agent_id"], "amp");
    // Shared install root: nothing to copy, but the link dir still gets a
    // symlink for every installed skill.
    assert_eq!(body["data"]["skills_port"]["status"], "shared");
    assert_eq!(body["data"]["skills_link"]["linked"][0]["name"], "repo-map");
    let link = tempdir.path().join(".amp/skills/repo-map");
    let metadata = std::fs::symlink_metadata(&link).expect("link metadata");
    assert!(metadata.file_type().is_symlink());
    assert!(link.join("SKILL.md").is_file());
}

#[tokio::test]
async fn agent_switch_reports_skills_link_error_without_failing() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    let config_dir = tempdir.path().join(".config/acp-stack");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    write_amp_linked_skills_registry_override(&config_dir);
    write_installed_skill(
        &tempdir.path().join(".agents/skills"),
        "repo-map",
        "# Source Repo Map\n",
    );
    // A regular file where the link dir's parent should be makes the link
    // refresh fail; the switch itself must still succeed and report why.
    std::fs::write(tempdir.path().join(".amp"), "not a directory\n").expect("amp file");
    let mut secrets =
        acp_stack::secrets::SecretStore::open_or_create(tempdir.path()).expect("secret store");
    secrets
        .set_many([("AMP_API_KEY", "amp-secret")])
        .expect("amp secret");

    let mut config = test_config();
    let workspace = tempdir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    config.workspace.root = workspace.to_string_lossy().into_owned();
    config.workspace.uploads = workspace.join("uploads").to_string_lossy().into_owned();
    config.agent.cwd = Some(config.workspace.root.clone());
    let harness = AgentHarness::spawn_with_config(config).await;
    let response = http()
        .await
        .post(format!("{}/v1/agent/switch", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&serde_json::json!({ "agent": "amp" }))
        .send()
        .await
        .expect("send switch");
    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();
    assert_eq!(status, StatusCode::OK, "body: {body_text}");
    let body: Value = serde_json::from_str(&body_text).expect("switch json");

    assert_eq!(body["data"]["agent_id"], "amp");
    assert!(body["data"]["skills_link"].is_null(), "body: {body_text}");
    let error = body["data"]["skills_link_error"]
        .as_str()
        .expect("skills_link_error present");
    assert!(error.contains(".amp"), "error: {error}");
}

#[tokio::test]
async fn agent_switch_reports_shared_skills_dir_without_copying() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    let config_dir = tempdir.path().join(".config/acp-stack");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    write_cursor_registry_override(&config_dir);
    write_installed_skill(
        &tempdir.path().join(".agents/skills"),
        "repo-map",
        "# Source Repo Map\n",
    );
    let mut secrets =
        acp_stack::secrets::SecretStore::open_or_create(tempdir.path()).expect("secret store");
    secrets
        .set_many([("CURSOR_API_KEY", "cursor-secret")])
        .expect("cursor secret");
    let fixture_path = write_config_options_fixture(tempdir.path(), &["cursor/gpt-5.5"]);
    let _fixture_guard = EnvVarGuard::set("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &fixture_path);

    let mut config = test_config();
    let workspace = tempdir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    config.workspace.root = workspace.to_string_lossy().into_owned();
    config.workspace.uploads = workspace.join("uploads").to_string_lossy().into_owned();
    config.agent.cwd = Some(config.workspace.root.clone());
    let harness = AgentHarness::spawn_with_config(config).await;
    let response = http()
        .await
        .post(format!("{}/v1/agent/switch", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&serde_json::json!({ "agent": "cursor" }))
        .send()
        .await
        .expect("send switch");
    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();
    assert_eq!(status, StatusCode::OK, "body: {body_text}");
    let body: Value = serde_json::from_str(&body_text).expect("switch json");

    assert_eq!(body["data"]["agent_id"], "cursor");
    assert_eq!(body["data"]["skills_port"]["status"], "shared");
    assert!(
        tempdir
            .path()
            .join(".agents/skills/repo-map/SKILL.md")
            .is_file()
    );
}

#[tokio::test]
async fn agent_switch_skill_port_failure_aborts_config_write() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    let config_dir = tempdir.path().join(".config/acp-stack");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    write_amp_registry_override(&config_dir);
    write_installed_skill(
        &tempdir.path().join(".agents/skills"),
        "repo-map",
        "# Source Repo Map\n",
    );
    std::fs::create_dir_all(tempdir.path().join(".config/agents/skills")).expect("target root");
    std::fs::write(
        tempdir.path().join(".config/agents/skills/repo-map"),
        "not a directory\n",
    )
    .expect("conflict");
    let mut secrets =
        acp_stack::secrets::SecretStore::open_or_create(tempdir.path()).expect("secret store");
    secrets
        .set_many([("AMP_API_KEY", "amp-secret")])
        .expect("amp secret");

    let mut config = test_config();
    let workspace = tempdir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    config.workspace.root = workspace.to_string_lossy().into_owned();
    config.workspace.uploads = workspace.join("uploads").to_string_lossy().into_owned();
    config.agent.cwd = Some(config.workspace.root.clone());
    let harness = AgentHarness::spawn_with_config(config).await;
    let response = http()
        .await
        .post(format!("{}/v1/agent/switch", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&serde_json::json!({ "agent": "amp" }))
        .send()
        .await
        .expect("send switch");
    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();

    assert_eq!(status, StatusCode::CONFLICT, "body: {body_text}");
    let written = std::fs::read_to_string(&harness.config_path).expect("read config");
    assert!(written.contains(r#"id = "opencode""#));
}

#[tokio::test]
async fn agent_switch_copies_provider_secret_to_target_default_ref() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    let config_dir = tempdir.path().join(".config/acp-stack");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    write_pi_registry_override(&config_dir);
    let mut config = test_config();
    let workspace = tempdir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    config.workspace.root = workspace.to_string_lossy().into_owned();
    config.workspace.uploads = workspace.join("uploads").to_string_lossy().into_owned();
    config.agent.cwd = Some(config.workspace.root.clone());
    config.agent.env = vec![
        "CLOUDFLARE_API_TOKEN".to_owned(),
        "CLOUDFLARE_ACCOUNT_ID".to_owned(),
        "CLOUDFLARE_GATEWAY_ID".to_owned(),
    ];
    config.agent.provider = Some(acp_stack::config::AgentProviderConfig {
        id: "cloudflare-ai-gateway".to_owned(),
        model: Some("cloudflare-ai-gateway/workers-ai/@cf/test".to_owned()),
        api_key_ref: Some("CLOUDFLARE_API_TOKEN".to_owned()),
        custom: None,
    });
    let mut secrets =
        acp_stack::secrets::SecretStore::open_or_create(tempdir.path()).expect("secret store");
    secrets
        .set_many([
            ("CLOUDFLARE_API_TOKEN", "cloudflare-secret"),
            ("CLOUDFLARE_ACCOUNT_ID", "account-id"),
            ("CLOUDFLARE_GATEWAY_ID", "gateway-id"),
        ])
        .expect("cloudflare secrets");
    let fixture_path = write_config_options_fixture(
        tempdir.path(),
        &["cloudflare-ai-gateway/workers-ai/@cf/test"],
    );
    let _fixture_guard = EnvVarGuard::set("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &fixture_path);

    let harness = AgentHarness::spawn_with_config(config).await;
    let client = http().await;
    let response = client
        .post(format!("{}/v1/agent/switch", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&serde_json::json!({ "agent": "pi" }))
        .send()
        .await
        .expect("send switch");
    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();
    assert_eq!(status, StatusCode::OK, "body: {body_text}");
    let body: Value = serde_json::from_str(&body_text).expect("switch json");
    assert_eq!(body["data"]["agent_id"], "pi");
    assert_eq!(body["data"]["api_key_ref"], "CLOUDFLARE_API_KEY");
    assert_eq!(
        body["data"]["secret_migrations"][0]["from_ref"],
        "CLOUDFLARE_API_TOKEN"
    );
    assert_eq!(
        body["data"]["secret_migrations"][0]["to_ref"],
        "CLOUDFLARE_API_KEY"
    );

    let secrets = acp_stack::secrets::SecretStore::open(tempdir.path()).expect("secret store");
    assert_eq!(
        secrets.get("CLOUDFLARE_API_KEY").expect("copied secret"),
        "cloudflare-secret"
    );
    let written = std::fs::read_to_string(&harness.config_path).expect("read config");
    assert!(written.contains(r#"api_key_ref = "CLOUDFLARE_API_KEY""#));
    assert!(written.contains(r#""CLOUDFLARE_API_KEY""#));
}

#[tokio::test]
async fn agent_switch_drop_cleans_source_config_and_preserves_secrets() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    let config_dir = tempdir.path().join(".config/acp-stack");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    write_cursor_registry_override(&config_dir);
    let mut config = test_config();
    let workspace = tempdir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    config.workspace.root = workspace.to_string_lossy().into_owned();
    config.workspace.uploads = workspace.join("uploads").to_string_lossy().into_owned();
    config.agent.cwd = Some(config.workspace.root.clone());
    config.agent.env = vec!["OPENAI_API_KEY".to_owned()];
    config.agent.provider = Some(acp_stack::config::AgentProviderConfig {
        id: "openai".to_owned(),
        model: Some("openai/gpt-5.5".to_owned()),
        api_key_ref: Some("OPENAI_API_KEY".to_owned()),
        custom: None,
    });
    let opencode_path = tempdir
        .path()
        .join(".config")
        .join("opencode")
        .join("opencode.json");
    std::fs::create_dir_all(opencode_path.parent().expect("path has parent"))
        .expect("opencode dir");
    std::fs::write(
        &opencode_path,
        r#"{"$schema":"https://opencode.ai/config.json","model":"openai/gpt-5.5","small_model":"openai/gpt-5.5","enabled_providers":["openai"],"provider":{"openai":{"options":{"apiKey":"{env:OPENAI_API_KEY}"}}},"theme":"keep"}"#,
    )
    .expect("opencode config");
    let mut secrets =
        acp_stack::secrets::SecretStore::open_or_create(tempdir.path()).expect("secret store");
    secrets
        .set_many([
            ("OPENAI_API_KEY", "openai-secret"),
            ("CURSOR_API_KEY", "cursor-secret"),
        ])
        .expect("secrets");
    let fixture_path = write_config_options_fixture(tempdir.path(), &["cursor/gpt-5.5"]);
    let _fixture_guard = EnvVarGuard::set("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &fixture_path);

    let harness = AgentHarness::spawn_with_config(config).await;
    let client = http().await;
    let response = client
        .post(format!("{}/v1/agent/switch", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&serde_json::json!({ "agent": "cursor", "drop": true }))
        .send()
        .await
        .expect("send switch");
    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();
    assert_eq!(status, StatusCode::OK, "body: {body_text}");
    let body: Value = serde_json::from_str(&body_text).expect("switch json");
    assert_eq!(body["data"]["agent_id"], "cursor");
    assert_eq!(
        body["data"]["cleaned_configs"][0]["path"],
        opencode_path.to_string_lossy().as_ref()
    );

    let value: Value = serde_json::from_str(
        &std::fs::read_to_string(&opencode_path).expect("opencode config remains"),
    )
    .expect("opencode json");
    assert_eq!(value["theme"], "keep");
    assert!(value.get("model").is_none());
    assert!(value.get("provider").is_none());

    let secrets = acp_stack::secrets::SecretStore::open(tempdir.path()).expect("secret store");
    assert_eq!(
        secrets.get("OPENAI_API_KEY").expect("source secret"),
        "openai-secret"
    );
    assert_eq!(
        secrets.get("CURSOR_API_KEY").expect("target secret"),
        "cursor-secret"
    );
}

#[tokio::test]
async fn agent_switch_drop_reports_cleanup_failure_without_failing_switch() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    let config_dir = tempdir.path().join(".config/acp-stack");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    write_cursor_registry_override(&config_dir);
    let mut config = test_config();
    let workspace = tempdir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    config.workspace.root = workspace.to_string_lossy().into_owned();
    config.workspace.uploads = workspace.join("uploads").to_string_lossy().into_owned();
    config.agent.cwd = Some(config.workspace.root.clone());
    let opencode_path = tempdir
        .path()
        .join(".config")
        .join("opencode")
        .join("opencode.json");
    std::fs::create_dir_all(opencode_path.parent().expect("path has parent"))
        .expect("opencode dir");
    std::fs::write(&opencode_path, "not json").expect("opencode config");
    let mut secrets =
        acp_stack::secrets::SecretStore::open_or_create(tempdir.path()).expect("secret store");
    secrets
        .set_many([("CURSOR_API_KEY", "cursor-secret")])
        .expect("cursor secret");
    let fixture_path = write_config_options_fixture(tempdir.path(), &["cursor/gpt-5.5"]);
    let _fixture_guard = EnvVarGuard::set("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &fixture_path);

    let harness = AgentHarness::spawn_with_config(config).await;
    let client = http().await;
    let response = client
        .post(format!("{}/v1/agent/switch", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&serde_json::json!({ "agent": "cursor", "drop": true }))
        .send()
        .await
        .expect("send switch");
    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();
    assert_eq!(status, StatusCode::OK, "body: {body_text}");
    let body: Value = serde_json::from_str(&body_text).expect("switch json");
    assert_eq!(body["data"]["agent_id"], "cursor");
    assert!(
        body["data"]["cleanup_errors"]
            .as_array()
            .is_some_and(|errors| !errors.is_empty()),
        "cleanup error should be reported: {body}"
    );
    let written = std::fs::read_to_string(&harness.config_path).expect("read config");
    assert!(written.contains(r#"id = "cursor""#));
}

#[tokio::test]
async fn model_discovery_timeout_shuts_down_provisional_agent() {
    let _fixture_guard = EnvVarGuard::unset("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH");
    let tempdir = TempDir::new().expect("tempdir");
    let pid_path = tempdir.path().join("placebo-agent.pid");
    let mut config = test_config();
    config.agent.args = vec![
        "acp".into(),
        "--session-new-stall".into(),
        "--write-pid".into(),
        pid_path.to_string_lossy().into_owned(),
    ];

    let err = fetch_session_config_with_timeout(tempdir.path(), &config, Duration::from_millis(50))
        .await
        .expect_err("discovery should time out");
    assert_eq!(err.error_code(), "agent.initialize_failed");
    assert!(
        err.to_string().contains("model discovery exceeded"),
        "unexpected error: {err}",
    );

    #[cfg(unix)]
    {
        let pid_text = std::fs::read_to_string(&pid_path).expect("pid written");
        let pid: u32 = pid_text.trim().parse().expect("pid parses");
        for _ in 0..40 {
            if process_is_gone(pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("placebo-agent process {pid} still alive after discovery timeout");
    }
}

#[cfg(unix)]
fn process_is_gone(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as i32, 0) };
    result != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

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

// Content string that sits between the 1 MiB per-file content cap and the
// ~6 MiB whole-request cap, so the request layer admits it and the handler
// rejects it on the content cap.
const OVER_CONTENT_UNDER_REQUEST_BYTES: usize = 2 * 1_048_576;

/// The inspect route is layered with `RequestBodyLimitLayer(IMPORT_REQUEST_SIZE_LIMIT)`,
/// which is deliberately looser than the 1 MiB content cap the handler enforces.
/// A ~2 MiB content string must reach the handler and fail on the content cap;
/// a content string large enough to blow past the ~6 MiB request cap must be
/// rejected at the body-limit layer before the handler runs.
#[tokio::test]
async fn native_config_inspect_request_layer_defers_to_content_cap() {
    let harness = AgentHarness::spawn().await;
    let home = harness
        .config_path
        .parent()
        .expect("config path has parent")
        .to_path_buf();
    let _home = HomeEnvGuard::set(&home);
    let client = http().await;

    // Between the content cap and the request cap: reaches the handler, fails
    // on the content cap with `native_config_too_large` (HTTP 413).
    let over_content = "x".repeat(OVER_CONTENT_UNDER_REQUEST_BYTES);
    let response = client
        .post(format!(
            "{}/v1/agent/config/native/inspect",
            harness.base_url
        ))
        .header("Authorization", admin_bearer())
        .json(&json!({ "filename": "opencode.json", "content": over_content }))
        .send()
        .await
        .expect("send inspect");
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body: Value = response.json().await.expect("inspect json");
    assert_eq!(body["error"]["code"], "native_config_too_large");

    // Past the whole-request cap: rejected at the body-limit layer. The
    // middleware response may not be JSON, so assert on status and only on
    // the envelope if the body parses as JSON. The layer also stops reading
    // and may abort the connection before the client finishes writing the
    // oversize body, so a reset/broken-pipe mid-send races the 413 response;
    // both are the rejection observed from the socket.
    let over_request = "x".repeat(acp_stack::config::IMPORT_REQUEST_SIZE_LIMIT + 1_048_576);
    let result = client
        .post(format!(
            "{}/v1/agent/config/native/inspect",
            harness.base_url
        ))
        .header("Authorization", admin_bearer())
        .json(&json!({ "filename": "opencode.json", "content": over_request }))
        .send()
        .await;
    match result {
        Ok(response) => {
            assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
            let text = response.text().await.unwrap_or_default();
            if let Ok(body) = serde_json::from_str::<Value>(&text) {
                assert_ne!(
                    body["error"]["code"], "native_config_too_large",
                    "oversize request should be rejected by the body-limit layer, not the content cap: {body}"
                );
            }
        }
        Err(error) => {
            // The abort can surface as a reset, broken pipe, or hyper's
            // sourceless "incomplete message", so no error shape is asserted
            // here. A crashed server would also land in this arm, so prove
            // the server is still alive and rejecting by re-issuing the
            // content-cap request and re-asserting its typed 413.
            let retry_content = "x".repeat(OVER_CONTENT_UNDER_REQUEST_BYTES);
            let response = client
                .post(format!(
                    "{}/v1/agent/config/native/inspect",
                    harness.base_url
                ))
                .header("Authorization", admin_bearer())
                .json(&json!({ "filename": "opencode.json", "content": retry_content }))
                .send()
                .await
                .unwrap_or_else(|retry_error| {
                    panic!("server unreachable after oversize send failed ({error}): {retry_error}")
                });
            assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
            let body: Value = response
                .json()
                .await
                .expect("inspect json after oversize send abort");
            assert_eq!(body["error"]["code"], "native_config_too_large");
        }
    }
}

/// Drives the full inspect -> import -> cancel rollback loop through the HTTP
/// layer with a model-free native config, so apply never triggers model
/// discovery or an agent launch. Covers the happy-path rollback and the
/// digest-guard rejection when the applied native file is mutated on disk,
/// plus admin-tier enforcement on the cancel route.
#[tokio::test]
async fn native_config_cancel_rolls_back_and_guards_digest() {
    let harness = AgentHarness::spawn().await;
    let home = harness
        .config_path
        .parent()
        .expect("config path has parent")
        .to_path_buf();
    let _home = HomeEnvGuard::set(&home);
    // The import prepare path opens the secret store read-only, so it must
    // exist under HOME even though `{"theme":"dark"}` carries no secret refs.
    acp_stack::secrets::SecretStore::open_or_create(&home).expect("secret store");
    let native_path = home.join(".config").join("opencode").join("opencode.json");
    let client = http().await;

    // Admin-tier enforcement: the cancel route rejects a session key with
    // `auth.wrong_kind` (401), matching the other admin-route tests here.
    let rejected = client
        .post(format!(
            "{}/v1/agent/config/native/import/op_missing/cancel",
            harness.base_url
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("send session cancel");
    assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);
    let rejected_body: Value = rejected.json().await.expect("json");
    assert_eq!(rejected_body["error"]["code"], "auth.wrong_kind");

    let canonical_before =
        std::fs::read(&harness.config_path).expect("canonical config before import");

    let operation_id = apply_theme_import(&client, &harness.base_url).await;

    // Applied without a running agent: no restart required, native file on disk.
    assert!(
        native_path.is_file(),
        "native file should exist after apply"
    );

    // Happy path: cancel rolls back, dropping the native file and restoring
    // the canonical config bytes verbatim.
    let cancel = client
        .post(format!(
            "{}/v1/agent/config/native/import/{operation_id}/cancel",
            harness.base_url
        ))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("send cancel");
    let status = cancel.status();
    let body: Value = cancel.json().await.expect("cancel json");
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["status"], "cancelled");
    assert!(
        !native_path.exists(),
        "native file should be removed after cancel rollback"
    );
    let canonical_after =
        std::fs::read(&harness.config_path).expect("canonical config after cancel");
    assert_eq!(
        canonical_before, canonical_after,
        "canonical config bytes should be restored by rollback"
    );

    // Digest guard: a fresh apply, then mutate the applied native file on
    // disk. Cancel must refuse with `native_config_rollback_conflict` (409)
    // rather than roll back over the tampered file.
    let guarded_operation_id = apply_theme_import(&client, &harness.base_url).await;
    assert!(
        native_path.is_file(),
        "native file should exist after apply"
    );
    let mut mutated = std::fs::read(&native_path).expect("read applied native file");
    mutated.extend_from_slice(b"\n// tampered\n");
    std::fs::write(&native_path, &mutated).expect("mutate applied native file");

    let guarded_cancel = client
        .post(format!(
            "{}/v1/agent/config/native/import/{guarded_operation_id}/cancel",
            harness.base_url
        ))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("send guarded cancel");
    let guarded_status = guarded_cancel.status();
    let guarded_body: Value = guarded_cancel.json().await.expect("guarded cancel json");
    assert_eq!(guarded_status, StatusCode::CONFLICT, "body: {guarded_body}");
    assert_eq!(
        guarded_body["error"]["code"],
        "native_config_rollback_conflict"
    );
}

#[tokio::test]
async fn native_config_import_serializes_with_agent_config_mutation_lock() {
    let harness = AgentHarness::spawn().await;
    let home = harness
        .config_path
        .parent()
        .expect("config path has parent")
        .to_path_buf();
    let _home = HomeEnvGuard::set(&home);
    acp_stack::secrets::SecretStore::open_or_create(&home).expect("secret store");
    let client = http().await;

    let inspect = client
        .post(format!(
            "{}/v1/agent/config/native/inspect",
            harness.base_url
        ))
        .header("Authorization", admin_bearer())
        .json(&json!({ "filename": "opencode.json", "content": r#"{"theme":"dark"}"# }))
        .send()
        .await
        .expect("send inspect");
    let inspect_body: Value = inspect.json().await.expect("inspect json");
    let revision = inspect_body["data"]["revision"]
        .as_str()
        .expect("inspect revision")
        .to_owned();

    // Hold the cross-process mutation lock; the import must block on it and
    // only complete after release. `acps agent set` and the other serialized
    // writers take the same lock, so this pins the import side of the pairing.
    let lock = acp_stack::fs_util::acquire_agent_config_mutation_file_lock(&harness.config_path)
        .expect("acquire mutation lock");
    let import_client = client.clone();
    let base_url = harness.base_url.clone();
    let import_task = tokio::spawn(async move {
        import_client
            .post(format!("{base_url}/v1/agent/config/native/import"))
            .header("Authorization", admin_bearer())
            .json(&json!({
                "revision": revision,
                "selected_managed_field_ids": [],
                "executable_settings_acknowledged": false
            }))
            .send()
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    assert!(
        !import_task.is_finished(),
        "import must wait while the mutation lock is held"
    );
    drop(lock);
    let import = import_task
        .await
        .expect("join import task")
        .expect("send import");
    let status = import.status();
    let body: Value = import.json().await.expect("import json");
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["status"], "applied", "body: {body}");
}

/// Inspect `{"theme":"dark"}` then import the empty selection, returning the
/// applied operation id. The theme-only config carries no model key, so apply
/// never triggers model discovery or an agent launch.
async fn apply_theme_import(client: &reqwest::Client, base_url: &str) -> String {
    let inspect = client
        .post(format!("{base_url}/v1/agent/config/native/inspect"))
        .header("Authorization", admin_bearer())
        .json(&json!({ "filename": "opencode.json", "content": r#"{"theme":"dark"}"# }))
        .send()
        .await
        .expect("send inspect");
    let inspect_status = inspect.status();
    let inspect_body: Value = inspect.json().await.expect("inspect json");
    assert_eq!(inspect_status, StatusCode::OK, "body: {inspect_body}");
    let revision = inspect_body["data"]["revision"]
        .as_str()
        .expect("inspect revision")
        .to_owned();

    let import = client
        .post(format!("{base_url}/v1/agent/config/native/import"))
        .header("Authorization", admin_bearer())
        .json(&json!({
            "revision": revision,
            "selected_managed_field_ids": [],
            "executable_settings_acknowledged": false
        }))
        .send()
        .await
        .expect("send import");
    let import_status = import.status();
    let import_body: Value = import.json().await.expect("import json");
    assert_eq!(import_status, StatusCode::OK, "body: {import_body}");
    assert_eq!(
        import_body["data"]["status"], "applied",
        "body: {import_body}"
    );
    assert_eq!(
        import_body["data"]["restart"]["required"], false,
        "no running agent, so no restart required: {import_body}"
    );
    import_body["data"]["operation_id"]
        .as_str()
        .expect("operation id")
        .to_owned()
}
