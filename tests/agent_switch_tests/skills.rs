use reqwest::StatusCode;
use serde_json::Value;
use tempfile::TempDir;

use crate::common::HomeEnvGuard;
use crate::common::agent::{
    AgentHarness, EnvVarGuard, admin_bearer, http, test_config,
    write_amp_linked_skills_registry_override, write_amp_registry_override,
    write_config_options_fixture, write_installed_skill, write_kimi_registry_override,
};

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
        .json(&serde_json::json!({ "agent_id": "amp" }))
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
        .json(&serde_json::json!({ "agent_id": "amp" }))
        .send()
        .await
        .expect("send switch");
    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();
    assert_eq!(status, StatusCode::OK, "body: {body_text}");
    let body: Value = serde_json::from_str(&body_text).expect("switch json");

    assert_eq!(body["data"]["agent_id"], "amp");
    // Shared install root: nothing to copy, but every installed skill still gets a symlink.
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
    // A file where the link dir's parent belongs fails the refresh; the switch must still
    // succeed and report why.
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
        .json(&serde_json::json!({ "agent_id": "amp" }))
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
    write_kimi_registry_override(&config_dir);
    write_installed_skill(
        &tempdir.path().join(".agents/skills"),
        "repo-map",
        "# Source Repo Map\n",
    );
    let mut secrets =
        acp_stack::secrets::SecretStore::open_or_create(tempdir.path()).expect("secret store");
    secrets
        .set_many([("KIMI_API_KEY", "kimi-secret")])
        .expect("kimi secret");
    let fixture_path = write_config_options_fixture(tempdir.path(), &["kimi/kimi-k3"]);
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
        .json(&serde_json::json!({ "agent_id": "kimi" }))
        .send()
        .await
        .expect("send switch");
    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();
    assert_eq!(status, StatusCode::OK, "body: {body_text}");
    let body: Value = serde_json::from_str(&body_text).expect("switch json");

    assert_eq!(body["data"]["agent_id"], "kimi");
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
        .json(&serde_json::json!({ "agent_id": "amp" }))
        .send()
        .await
        .expect("send switch");
    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();

    assert_eq!(status, StatusCode::CONFLICT, "body: {body_text}");
    let written = std::fs::read_to_string(&harness.config_path).expect("read config");
    assert!(written.contains(r#"id = "opencode""#));
}
