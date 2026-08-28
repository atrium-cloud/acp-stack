use std::time::Duration;

use acp_stack::runtime::agent::model_discovery::fetch_session_config_with_timeout;
use reqwest::StatusCode;
use serde_json::Value;
use tempfile::TempDir;

use crate::common::agent::{
    AgentHarness, EnvVarGuard, admin_bearer, http, test_config, write_config_options_fixture,
    write_kimi_registry_override, write_pi_registry_override,
};

#[tokio::test]
async fn agent_switch_copies_provider_secret_to_target_default_ref() {
    let tempdir = TempDir::new().expect("tempdir");
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

    let harness =
        AgentHarness::spawn_with_config_and_home(config, tempdir.path().to_path_buf()).await;
    let client = http().await;
    let response = client
        .post(format!("{}/v1/agent/switch", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&serde_json::json!({ "agent_id": "pi" }))
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
    let config_dir = tempdir.path().join(".config/acp-stack");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    write_kimi_registry_override(&config_dir);
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
            ("KIMI_API_KEY", "kimi-secret"),
        ])
        .expect("secrets");
    let fixture_path = write_config_options_fixture(tempdir.path(), &["kimi/kimi-k3"]);
    let _fixture_guard = EnvVarGuard::set("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &fixture_path);

    let harness =
        AgentHarness::spawn_with_config_and_home(config, tempdir.path().to_path_buf()).await;
    let client = http().await;
    let response = client
        .post(format!("{}/v1/agent/switch", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&serde_json::json!({ "agent_id": "kimi", "drop": true }))
        .send()
        .await
        .expect("send switch");
    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();
    assert_eq!(status, StatusCode::OK, "body: {body_text}");
    let body: Value = serde_json::from_str(&body_text).expect("switch json");
    assert_eq!(body["data"]["agent_id"], "kimi");
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
        secrets.get("KIMI_API_KEY").expect("target secret"),
        "kimi-secret"
    );
}

#[tokio::test]
async fn agent_switch_drop_reports_cleanup_failure_without_failing_switch() {
    let tempdir = TempDir::new().expect("tempdir");
    let config_dir = tempdir.path().join(".config/acp-stack");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    write_kimi_registry_override(&config_dir);
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
        .set_many([("KIMI_API_KEY", "kimi-secret")])
        .expect("kimi secret");
    let fixture_path = write_config_options_fixture(tempdir.path(), &["kimi/kimi-k3"]);
    let _fixture_guard = EnvVarGuard::set("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &fixture_path);

    let harness =
        AgentHarness::spawn_with_config_and_home(config, tempdir.path().to_path_buf()).await;
    let client = http().await;
    let response = client
        .post(format!("{}/v1/agent/switch", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&serde_json::json!({ "agent_id": "kimi", "drop": true }))
        .send()
        .await
        .expect("send switch");
    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();
    assert_eq!(status, StatusCode::OK, "body: {body_text}");
    let body: Value = serde_json::from_str(&body_text).expect("switch json");
    assert_eq!(body["data"]["agent_id"], "kimi");
    assert!(
        body["data"]["cleanup_errors"]
            .as_array()
            .is_some_and(|errors| !errors.is_empty()),
        "cleanup error should be reported: {body}"
    );
    let written = std::fs::read_to_string(&harness.config_path).expect("read config");
    assert!(written.contains(r#"id = "kimi""#));
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
