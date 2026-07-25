use acp_stack::auth::KeyKind;
use acp_stack::config::LocalSessionAuth;
use acp_stack::secrets::SecretStore;
use acp_stack::state::EventFilter;
use reqwest::StatusCode;
use serde_json::Value;

mod common;
use common::HomeEnvGuard;
use common::api::{ADMIN_KEY, SESSION_KEY, ServerHarness};

#[tokio::test]
async fn config_export_returns_canonical_toml() {
    let harness = ServerHarness::spawn().await;
    let response = reqwest::Client::new()
        .get(format!("{}/v1/config/export", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    let toml = body["data"]["toml"].as_str().expect("toml string");
    assert!(toml.contains("[api]"));
    assert!(toml.contains("bind ="));
}

#[tokio::test]
async fn config_export_reads_current_runtime_config_file() {
    let harness = ServerHarness::spawn().await;
    let current = std::fs::read_to_string(&harness.config_path).expect("read config");
    let updated = current.replace(
        r#"public_url = "https://agent.example.com""#,
        r#"public_url = "https://updated.example.com""#,
    );
    std::fs::write(&harness.config_path, updated).expect("write updated config");

    let response = reqwest::Client::new()
        .get(format!("{}/v1/config/export", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    let toml = body["data"]["toml"].as_str().expect("toml string");
    assert!(toml.contains(r#"public_url = "https://updated.example.com""#));
}

#[tokio::test]
async fn config_validate_accepts_valid_toml() {
    let harness = ServerHarness::spawn().await;
    let toml = include_str!("fixtures/valid-placebo-stack.toml");
    let response = reqwest::Client::new()
        .post(format!("{}/v1/config/validate", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .body(toml.to_owned())
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["data"]["valid"], Value::Bool(true));
}

#[tokio::test]
async fn config_validate_rejects_garbage_with_400() {
    let harness = ServerHarness::spawn().await;
    let response = reqwest::Client::new()
        .post(format!("{}/v1/config/validate", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .body("this is not toml at all")
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["ok"], Value::Bool(false));
    // ConfigToml errors carry the dotted code "config.invalid".
    assert_eq!(body["error"]["code"], "config.invalid");
}

#[tokio::test]
async fn config_import_dry_run_returns_metadata() {
    let harness = ServerHarness::spawn().await;
    let original_config = std::fs::read_to_string(&harness.config_path).expect("read config");
    let toml = include_str!("fixtures/valid-placebo-stack.toml");
    let response = reqwest::Client::new()
        .post(format!(
            "{}/v1/config/import?dry_run=true",
            harness.base_url
        ))
        .header("Authorization", format!("Bearer {ADMIN_KEY}"))
        .body(toml.to_owned())
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["ok"], Value::Bool(true));
    assert_eq!(body["data"]["dry_run"], Value::Bool(true));
    assert_eq!(body["data"]["config_version"], Value::Number(1.into()));
    assert!(body["data"]["canonical_toml_size"].is_number());
    assert!(body["data"]["input_size"].is_number());
    assert!(body["data"].get("auth_refs_unchanged").is_none());
    assert!(body["data"]["target"].is_string());
    assert!(body["data"]["target_exists"].is_boolean());

    let current_config = std::fs::read_to_string(&harness.config_path).expect("read config");
    assert_eq!(current_config, original_config);
    let guard = harness.state.lock().await;
    let events = guard
        .query_events(EventFilter {
            limit: 10,
            kind: Some("server.config_imported"),
            ..EventFilter::default()
        })
        .expect("query events");
    assert!(events.is_empty(), "dry-run must not audit config import");
}

#[tokio::test]
async fn config_import_dry_run_accepts_legacy_auth_section_without_mutation() {
    let harness = ServerHarness::spawn().await;
    let original_config = std::fs::read_to_string(&harness.config_path).expect("read config");
    let toml = include_str!("fixtures/valid-placebo-stack.toml").replace(
        "[security.http]",
        r#"[auth]
session_key_ref = "ACP_STACK_SESSION_KEY"
admin_key_ref = "ACP_STACK_ADMIN_KEY"

[security.http]"#,
    );
    let response = reqwest::Client::new()
        .post(format!(
            "{}/v1/config/import?dry_run=true",
            harness.base_url
        ))
        .header("Authorization", format!("Bearer {ADMIN_KEY}"))
        .body(toml)
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["ok"], Value::Bool(true));
    assert!(body["data"]["canonical_toml_size"].is_number());

    let current_config = std::fs::read_to_string(&harness.config_path).expect("read config");
    assert_eq!(current_config, original_config);
    let guard = harness.state.lock().await;
    let events = guard
        .query_events(EventFilter {
            limit: 10,
            kind: Some("server.config_imported"),
            ..EventFilter::default()
        })
        .expect("query events");
    assert!(events.is_empty(), "dry-run must not audit config import");
}

#[tokio::test]
async fn config_import_applies_local_session_auth_to_runtime() {
    let harness = ServerHarness::spawn().await;
    assert_eq!(
        *harness.local_session_auth.read().await,
        LocalSessionAuth::SessionKey
    );

    let toml = format!(
        "{}\n[local]\nsession_auth = \"keyless\"\n",
        include_str!("fixtures/valid-placebo-stack.toml")
    );
    let response = reqwest::Client::new()
        .post(format!("{}/v1/config/import", harness.base_url))
        .header("Authorization", format!("Bearer {ADMIN_KEY}"))
        .body(toml)
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["data"]["imported"], true);
    assert_eq!(body["data"]["local_session_auth"], "keyless");
    assert_eq!(
        *harness.local_session_auth.read().await,
        LocalSessionAuth::Keyless
    );
}

#[tokio::test]
async fn auth_regenerate_session_key_replaces_old_session_verifier() {
    let harness = ServerHarness::spawn().await;
    let client = reqwest::Client::new();
    let response = client
        .post(format!(
            "{}/v1/auth/session-key/regenerate",
            harness.base_url
        ))
        .header("Authorization", format!("Bearer {ADMIN_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    let new_session_key = body["data"]["session_key"]
        .as_str()
        .expect("new session key");
    assert!(new_session_key.starts_with("acps_"));
    assert_ne!(new_session_key, SESSION_KEY);

    let guard = harness.state.lock().await;
    let verifiers = guard.load_auth_verifier_pair().expect("auth verifiers");
    assert_eq!(verifiers.verify(SESSION_KEY), None);
    assert_eq!(verifiers.verify(new_session_key), Some(KeyKind::Session));
    assert_eq!(verifiers.verify(ADMIN_KEY), Some(KeyKind::Admin));
}

#[tokio::test]
async fn auth_regenerate_session_key_rejects_session_key() {
    let harness = ServerHarness::spawn().await;
    let client = reqwest::Client::new();
    let response = client
        .post(format!(
            "{}/v1/auth/session-key/regenerate",
            harness.base_url
        ))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "auth.wrong_kind");
}

#[tokio::test]
async fn local_session_access_update_requires_admin_key() {
    let harness = ServerHarness::spawn().await;
    let response = reqwest::Client::new()
        .put(format!("{}/v1/auth/local-session-access", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .json(&serde_json::json!({ "session_auth": "keyless" }))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "auth.wrong_kind");
}

#[tokio::test]
async fn local_session_access_update_persists_and_updates_runtime_state() {
    let harness = ServerHarness::spawn().await;
    assert_eq!(
        *harness.local_session_auth.read().await,
        LocalSessionAuth::SessionKey
    );

    let client = reqwest::Client::new();
    let response = client
        .put(format!("{}/v1/auth/local-session-access", harness.base_url))
        .header("Authorization", format!("Bearer {ADMIN_KEY}"))
        .json(&serde_json::json!({ "session_auth": "keyless" }))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["data"]["session_auth"], "keyless");
    assert_eq!(
        *harness.local_session_auth.read().await,
        LocalSessionAuth::Keyless
    );

    let config = std::fs::read_to_string(&harness.config_path).expect("read config");
    assert!(config.contains("session_auth = \"keyless\""));

    let response = client
        .get(format!("{}/v1/status", harness.base_url))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn config_import_oversized_body_returns_413() {
    let harness = ServerHarness::spawn().await;
    let body = "x".repeat(2 * 1024 * 1024); // 2 MiB
    let response = reqwest::Client::new()
        .post(format!("{}/v1/config/import", harness.base_url))
        .header("Authorization", format!("Bearer {ADMIN_KEY}"))
        .body(body)
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["ok"], Value::Bool(false));
    assert_eq!(body["error"]["code"], "import.too_large");
}

/// The secret store is a whole-file read-modify-write; before the handlers
/// took the agent-config mutation lock, concurrent set/delete requests could
/// interleave open→mutate→persist and silently drop each other's writes.
/// With the lock, every accepted mutation must be visible afterwards.
#[tokio::test]
async fn concurrent_secret_mutations_do_not_drop_writes() {
    let home_dir = tempfile::tempdir().expect("home tempdir");
    SecretStore::open_or_create(home_dir.path()).expect("create home secret store");
    let _home = HomeEnvGuard::set(home_dir.path());

    let harness = ServerHarness::spawn().await;
    let client = reqwest::Client::new();

    let seed = client
        .post(format!("{}/v1/secrets", harness.base_url))
        .header("Authorization", format!("Bearer {ADMIN_KEY}"))
        .json(&serde_json::json!({ "name": "SEED_DOOMED", "value": "v" }))
        .send()
        .await
        .expect("seed set");
    assert_eq!(seed.status(), StatusCode::OK);

    let mut requests = tokio::task::JoinSet::new();
    for index in 0..8 {
        let client = client.clone();
        let base_url = harness.base_url.clone();
        requests.spawn(async move {
            client
                .post(format!("{base_url}/v1/secrets"))
                .header("Authorization", format!("Bearer {ADMIN_KEY}"))
                .json(&serde_json::json!({
                    "name": format!("CONCURRENT_{index}"),
                    "value": format!("value-{index}"),
                }))
                .send()
                .await
                .expect("concurrent set")
                .status()
        });
    }
    {
        let client = client.clone();
        let base_url = harness.base_url.clone();
        requests.spawn(async move {
            client
                .delete(format!("{base_url}/v1/secrets/SEED_DOOMED"))
                .header("Authorization", format!("Bearer {ADMIN_KEY}"))
                .send()
                .await
                .expect("concurrent delete")
                .status()
        });
    }
    while let Some(status) = requests.join_next().await {
        assert_eq!(status.expect("request task"), StatusCode::OK);
    }

    let store = SecretStore::open(home_dir.path()).expect("reopen store");
    for index in 0..8 {
        assert!(
            store.contains(&format!("CONCURRENT_{index}")),
            "concurrent set CONCURRENT_{index} was dropped by another writer"
        );
    }
    assert!(
        !store.contains("SEED_DOOMED"),
        "concurrent delete was dropped by another writer"
    );
}

#[tokio::test]
async fn config_validate_secret_ref_value_error_does_not_echo_secret() {
    let harness = ServerHarness::spawn().await;
    let secret = "sk-proj-inline-secret-value";
    let toml = include_str!("fixtures/valid-placebo-stack.toml")
        .replace("env = []", &format!(r#"env = ["{secret}"]"#));
    let response = reqwest::Client::new()
        .post(format!("{}/v1/config/validate", harness.base_url))
        .header("Authorization", format!("Bearer {SESSION_KEY}"))
        .body(toml)
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["ok"], Value::Bool(false));
    assert_eq!(body["error"]["code"], "config.invalid");
    assert!(
        !body["error"]["message"]
            .as_str()
            .expect("message")
            .contains(secret)
    );
}
