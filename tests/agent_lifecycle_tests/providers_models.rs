use std::collections::BTreeMap;
use std::ffi::OsString;

use acp_stack::config::{AgentProviderConfig, Config};
use acp_stack::secrets::{ProviderCredential, ProviderCredentialSet, SecretStore};
use reqwest::StatusCode;
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::common::HomeEnvGuard;
use crate::common::agent::{
    AgentHarness, EnvVarGuard, add_codex_placebo_target, admin_bearer, http, session_bearer,
    spawn_provider_models_server, test_config,
};

#[tokio::test]
async fn providers_lists_supported_providers_for_configured_agent() {
    // Pure embedded-mapping lookup: the endpoint answers without spawning.
    let harness = AgentHarness::spawn().await;
    let client = http().await;

    let response = client
        .get(format!("{}/v1/providers", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("providers json");
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["agent_id"], "opencode");
    let providers = body["data"]["providers"]
        .as_array()
        .expect("providers array");
    assert!(
        !providers.is_empty(),
        "embedded mapping lists providers for opencode",
    );
    for provider in providers {
        assert!(
            provider["id"].as_str().is_some(),
            "missing id on {provider:?}",
        );
        assert!(
            provider["name"].as_str().is_some(),
            "missing name on {provider:?}",
        );
    }
}

#[tokio::test]
async fn providers_follow_default_target_changed_on_disk() {
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

    let response = http()
        .await
        .get(format!("{}/v1/providers", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("send providers");
    let status = response.status();
    let body: Value = response.json().await.expect("providers json");

    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["agent_id"], "codex");
}

#[tokio::test]
async fn providers_requires_session_key() {
    let harness = AgentHarness::spawn().await;
    let client = http().await;
    let response = client
        .get(format!("{}/v1/providers", harness.base_url))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn models_returns_fixture_advertised_values() {
    // The fixture file drives model discovery so no real agent binary spawns.
    let tempdir = TempDir::new().expect("tempdir");
    let fixture_path = tempdir.path().join("config-options.json");
    let fixture_body = serde_json::json!([
        {
            "id": "model",
            "name": "Model",
            "category": "model",
            "type": "select",
            "currentValue": "openai/gpt-4o",
            "options": [
                { "value": "openai/gpt-4o", "name": "openai/gpt-4o" },
                { "value": "anthropic/claude-3-5-sonnet", "name": "anthropic/claude-3-5-sonnet" }
            ]
        },
        {
            "id": "mode",
            "name": "Mode",
            "category": "mode",
            "type": "select",
            "currentValue": "default",
            "options": [
                { "value": "default", "name": "default" },
                { "value": "yolo", "name": "yolo" }
            ]
        },
        {
            "id": "reasoning_effort",
            "name": "Reasoning Effort",
            "category": "thought_level",
            "type": "select",
            "currentValue": "medium",
            "options": [
                { "value": "low", "name": "Low" },
                { "value": "medium", "name": "Medium" },
                { "value": "high", "name": "High" }
            ]
        }
    ]);
    std::fs::write(&fixture_path, fixture_body.to_string()).expect("write fixture");

    let _fixture_guard = EnvVarGuard::set("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &fixture_path);

    let harness = AgentHarness::spawn().await;
    let client = http().await;
    let response = client
        .get(format!("{}/v1/models", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("send");

    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();
    assert_eq!(status, StatusCode::OK, "body: {body_text}");
    let body: Value = serde_json::from_str(&body_text).expect("models json");
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["agent_id"], "opencode");
    assert_eq!(body["data"]["source"], "acp_advertised");
    let models = body["data"]["models"].as_array().expect("models array");
    assert!(
        models
            .iter()
            .any(|m| m["value"].as_str() == Some("openai/gpt-4o")),
        "advertised model values missing: {models:?}",
    );
    let modes = body["data"]["modes"].as_array().expect("modes array");
    assert!(
        modes.iter().any(|m| m.as_str() == Some("default")),
        "advertised mode values missing: {modes:?}",
    );
    let efforts = body["data"]["efforts"].as_array().expect("efforts array");
    assert!(
        efforts.iter().any(|e| e.as_str() == Some("medium")),
        "advertised effort values missing: {efforts:?}",
    );
}

#[tokio::test]
async fn agent_config_options_returns_the_full_advertised_set() {
    let tempdir = TempDir::new().expect("tempdir");
    let fixture_path = tempdir.path().join("config-options.json");
    let fixture_body = serde_json::json!([
        {
            "id": "model",
            "name": "Model",
            "category": "model",
            "type": "select",
            "currentValue": "openai/gpt-4o",
            "options": [{ "value": "openai/gpt-4o", "name": "openai/gpt-4o" }]
        },
        {
            "id": "fast",
            "name": "Fast mode",
            "category": "model_config",
            "type": "boolean",
            "currentValue": false
        },
        {
            "id": "agent",
            "name": "Agent",
            "type": "select",
            "currentValue": "default",
            "options": [
                { "value": "default", "name": "Default" },
                { "value": "researcher", "name": "Researcher" }
            ]
        }
    ]);
    std::fs::write(&fixture_path, fixture_body.to_string()).expect("write fixture");
    let _fixture_guard = EnvVarGuard::set("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &fixture_path);

    let harness = AgentHarness::spawn().await;
    let response = http()
        .await
        .get(format!("{}/v1/agent/config-options", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("send");

    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();
    assert_eq!(status, StatusCode::OK, "body: {body_text}");
    let body: Value = serde_json::from_str(&body_text).expect("config options json");
    assert_eq!(body["ok"], true);
    let options = body["data"]["config_options"]
        .as_array()
        .expect("options array");
    assert_eq!(options.len(), 3, "{body}");
    let fast = options
        .iter()
        .find(|option| option["id"] == "fast")
        .expect("boolean option present");
    assert_eq!(fast["type"], "boolean");
    assert_eq!(fast["category"], "model_config");
    assert_eq!(fast["current_value"], Value::Bool(false));
    let agent_option = options
        .iter()
        .find(|option| option["id"] == "agent")
        .expect("category-less option present");
    assert!(agent_option.get("category").is_none(), "{body}");
    assert_eq!(
        agent_option["options"].as_array().map(Vec::len),
        Some(2),
        "{body}"
    );
}

/// Seed a structured provider credential under `home`; duplicated from
/// `common::cli` because that module is gated behind `dev-tools`.
fn seed_provider_credential(home: &std::path::Path, provider_id: &str, env_names: &[&str]) {
    let mut store = SecretStore::open_or_create(home).expect("secret store should open");
    let values = env_names
        .iter()
        .map(|name| ((*name).to_owned(), format!("test-{name}")))
        .collect::<BTreeMap<_, _>>();
    store
        .set_many(
            values
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str())),
        )
        .expect("flat test secrets should be stored");
    let mut catalog = store.provider_credentials().clone();
    catalog.insert(
        provider_id.to_owned(),
        ProviderCredentialSet::aliasless(ProviderCredential::new(values, BTreeMap::new())),
    );
    store
        .replace_provider_credentials(catalog, &[])
        .expect("provider credential should be stored");
}

/// Config for the provider-catalog `/v1/models` path: codex takes the model
/// verbatim for OpenRouter and OpenRouter declares a `models_url`, so the
/// handler serves the catalog instead of the ACP-advertised list.
fn codex_openrouter_config() -> Config {
    let mut config = test_config();
    config.agent.id = "codex".to_owned();
    config.agent.name = "Codex".to_owned();
    config.agent.provider = Some(AgentProviderConfig {
        id: "openrouter".to_owned(),
        model: Some("deepseek/deepseek-v4-flash".to_owned()),
        api_key_ref: None,
        custom: None,
    });
    config
}

/// Hermes takes the model verbatim like Codex/OpenRouter but advertises no
/// ACP v1 `configOptions`, so the catalog-outage fallback cannot serve an
/// advertised model list.
fn hermes_openrouter_config() -> Config {
    let mut config = codex_openrouter_config();
    config.agent.id = "hermes".to_owned();
    config.agent.name = "Hermes Agent".to_owned();
    config
}

/// Config-options fixture with `model`, `mode`, and `thought_level` categories.
fn write_models_mode_fixture(root: &std::path::Path) -> std::path::PathBuf {
    let fixture_path = root.join("config-options.json");
    let fixture_body = serde_json::json!([
        {
            "id": "model",
            "name": "Model",
            "category": "model",
            "type": "select",
            "currentValue": "fixture/model-a",
            "options": [
                { "value": "fixture/model-a", "name": "fixture/model-a" },
                { "value": "fixture/model-b", "name": "fixture/model-b" }
            ]
        },
        {
            "id": "mode",
            "name": "Mode",
            "category": "mode",
            "type": "select",
            "currentValue": "default",
            "options": [
                { "value": "default", "name": "default" },
                { "value": "yolo", "name": "yolo" }
            ]
        },
        {
            "id": "reasoning_effort",
            "name": "Reasoning Effort",
            "category": "thought_level",
            "type": "select",
            "currentValue": "medium",
            "options": [
                { "value": "low", "name": "Low" },
                { "value": "medium", "name": "Medium" },
                { "value": "high", "name": "High" }
            ]
        }
    ]);
    std::fs::write(&fixture_path, fixture_body.to_string()).expect("write fixture");
    fixture_path
}

/// Shared env for the catalog-path tests. The fixture vars go through one
/// `EnvVarGuard::set_many` so the discovery-fixture mutex is taken exactly once.
fn catalog_fixture_env(
    home: &std::path::Path,
    models_base: &str,
    fixture_path: std::path::PathBuf,
) -> (HomeEnvGuard<'static>, EnvVarGuard<'static>) {
    let home_guard = HomeEnvGuard::set(home);
    let fixture_guard = EnvVarGuard::set_many(vec![
        (
            "ACP_STACK_PROVIDER_MODELS_BASE",
            OsString::from(models_base),
        ),
        ("OPENROUTER_API_KEY", OsString::from("test-openrouter-key")),
        (
            "ACP_STACK_AGENT_CONFIG_OPTIONS_PATH",
            fixture_path.into_os_string(),
        ),
    ]);
    (home_guard, fixture_guard)
}

#[tokio::test]
async fn models_serves_provider_catalog_for_codex_openrouter() {
    let tempdir = TempDir::new().expect("tempdir");
    let home = tempdir.path().join("home");
    seed_provider_credential(&home, "openrouter", &["OPENROUTER_API_KEY"]);
    let fixture_path = write_models_mode_fixture(tempdir.path());
    let base = spawn_provider_models_server(json!({
        "data": [
            { "id": "openai/gpt-5.5", "name": "GPT-5.5" },
            { "id": "deepseek/deepseek-v4-flash" },
        ]
    }));
    let _guards = catalog_fixture_env(&home, &base, fixture_path);

    let harness = AgentHarness::spawn_with_config(codex_openrouter_config()).await;
    let response = http()
        .await
        .get(format!("{}/v1/models", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("send");

    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();
    assert_eq!(status, StatusCode::OK, "body: {body_text}");
    let body: Value = serde_json::from_str(&body_text).expect("models json");
    assert_eq!(body["data"]["source"], "provider_catalog");
    assert!(
        body["data"].get("catalog_error").is_none(),
        "unexpected catalog_error: {body}"
    );
    let models = body["data"]["models"].as_array().expect("models array");
    assert!(
        models
            .iter()
            .any(|model| model["value"].as_str() == Some("openai/gpt-5.5")
                && model["display_name"].as_str() == Some("GPT-5.5")),
        "catalog model with display name missing: {models:?}",
    );
    assert!(
        models
            .iter()
            .any(|model| model["value"].as_str() == Some("deepseek/deepseek-v4-flash")),
        "catalog model missing: {models:?}",
    );
    let modes = body["data"]["modes"].as_array().expect("modes array");
    assert!(
        modes.iter().any(|mode| mode.as_str() == Some("default")),
        "fixture mode values missing: {modes:?}",
    );
    let efforts = body["data"]["efforts"].as_array().expect("efforts array");
    assert!(
        efforts.iter().any(|effort| effort.as_str() == Some("high")),
        "fixture effort values missing on the catalog path: {efforts:?}",
    );
}

#[tokio::test]
async fn models_falls_back_to_acp_with_catalog_error() {
    let tempdir = TempDir::new().expect("tempdir");
    let home = tempdir.path().join("home");
    seed_provider_credential(&home, "openrouter", &["OPENROUTER_API_KEY"]);
    let fixture_path = write_models_mode_fixture(tempdir.path());
    // Dead port forces the catalog fetch to fail.
    let _guards = catalog_fixture_env(&home, "http://127.0.0.1:1", fixture_path);

    let harness = AgentHarness::spawn_with_config(codex_openrouter_config()).await;
    let response = http()
        .await
        .get(format!("{}/v1/models", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("send");

    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();
    assert_eq!(status, StatusCode::OK, "body: {body_text}");
    let body: Value = serde_json::from_str(&body_text).expect("models json");
    assert_eq!(body["data"]["source"], "acp_advertised");
    let catalog_error = body["data"]["catalog_error"]
        .as_str()
        .expect("catalog_error should be a string");
    assert!(
        !catalog_error.is_empty(),
        "catalog_error should describe the failure"
    );
    let models = body["data"]["models"].as_array().expect("models array");
    assert!(
        models
            .iter()
            .any(|model| model["value"].as_str() == Some("fixture/model-a")),
        "fixture model values missing: {models:?}",
    );

    // Second poll lands inside the failure-backoff window.
    let response = http()
        .await
        .get(format!("{}/v1/models", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("send");
    let body: Value = response.json().await.expect("models json");
    assert_eq!(body["data"]["source"], "acp_advertised");
    let backed_off_error = body["data"]["catalog_error"]
        .as_str()
        .expect("catalog_error should be a string");
    assert!(
        backed_off_error.contains("request failed"),
        "backoff should serve the stored reason, got: {backed_off_error}"
    );
    assert_eq!(
        backed_off_error
            .matches("model catalog fetch failed")
            .count(),
        1,
        "stored reason must not double the error prefix: {backed_off_error}"
    );
}

#[tokio::test]
async fn models_degrades_to_empty_for_hermes_on_catalog_outage() {
    let tempdir = TempDir::new().expect("tempdir");
    let home = tempdir.path().join("home");
    seed_provider_credential(&home, "openrouter", &["OPENROUTER_API_KEY"]);
    // Hermes advertises no v1 `configOptions`; the fixture carries only modes.
    let fixture_path = tempdir.path().join("config-options.json");
    std::fs::write(
        &fixture_path,
        serde_json::json!([
            {
                "id": "mode",
                "name": "Mode",
                "category": "mode",
                "type": "select",
                "currentValue": "default",
                "options": [{ "value": "default", "name": "default" }]
            }
        ])
        .to_string(),
    )
    .expect("write fixture");
    let _guards = catalog_fixture_env(&home, "http://127.0.0.1:1", fixture_path);

    let harness = AgentHarness::spawn_with_config(hermes_openrouter_config()).await;
    let response = http()
        .await
        .get(format!("{}/v1/models", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("send");

    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();
    assert_eq!(status, StatusCode::OK, "body: {body_text}");
    let body: Value = serde_json::from_str(&body_text).expect("models json");
    assert_eq!(body["data"]["source"], "acp_advertised");
    let catalog_error = body["data"]["catalog_error"]
        .as_str()
        .expect("catalog_error should be a string");
    assert!(
        !catalog_error.is_empty(),
        "catalog_error should describe the failure"
    );
    assert_eq!(
        body["data"]["models"].as_array().expect("models array"),
        &Vec::<Value>::new(),
        "hermes has no ACP-advertised models to fall back to: {body}"
    );
    let modes = body["data"]["modes"].as_array().expect("modes array");
    assert!(
        modes.iter().any(|mode| mode.as_str() == Some("default")),
        "fixture mode values missing: {modes:?}",
    );
    assert_eq!(
        body["data"]["efforts"].as_array().expect("efforts array"),
        &Vec::<Value>::new(),
        "an agent advertising no effort option serves an empty list: {body}"
    );
}

#[tokio::test]
async fn models_serves_stale_cache_without_catalog_error() {
    let tempdir = TempDir::new().expect("tempdir");
    let home = tempdir.path().join("home");
    seed_provider_credential(&home, "openrouter", &["OPENROUTER_API_KEY"]);
    // The ancient `fetched_at` forces a refresh, which fails against the dead
    // port and leaves this entry to serve.
    let cache_dir = home.join(".config").join("acp-stack");
    std::fs::create_dir_all(&cache_dir).expect("cache dir");
    std::fs::write(
        cache_dir.join("provider-models.json"),
        json!({
            "version": 1,
            "providers": {
                "openrouter": {
                    "fetched_at": 1,
                    "models": [
                        { "value": "cached/openrouter-model", "display_name": "Cached Model" }
                    ]
                }
            }
        })
        .to_string(),
    )
    .expect("write cache");
    let fixture_path = write_models_mode_fixture(tempdir.path());
    let _guards = catalog_fixture_env(&home, "http://127.0.0.1:1", fixture_path);

    let harness = AgentHarness::spawn_with_config(codex_openrouter_config()).await;
    let response = http()
        .await
        .get(format!("{}/v1/models", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("send");

    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();
    assert_eq!(status, StatusCode::OK, "body: {body_text}");
    let body: Value = serde_json::from_str(&body_text).expect("models json");
    assert_eq!(body["data"]["source"], "provider_catalog");
    assert!(
        body["data"].get("catalog_error").is_none(),
        "stale cache must serve without catalog_error: {body}"
    );
    let models = body["data"]["models"].as_array().expect("models array");
    assert!(
        models.iter().any(
            |model| model["value"].as_str() == Some("cached/openrouter-model")
                && model["display_name"].as_str() == Some("Cached Model")
        ),
        "cached models missing: {models:?}",
    );
}

#[tokio::test]
async fn models_rejects_admin_key() {
    // Strict tiering: an admin key is not a session-key superset.
    let harness = AgentHarness::spawn().await;
    let client = http().await;
    let response = client
        .get(format!("{}/v1/models", harness.base_url))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "auth.wrong_kind");
}
