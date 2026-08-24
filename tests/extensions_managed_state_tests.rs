//! Integration tests for `POST /v1/admin/extensions/{name}/apply` (the
//! managed-state extension seam).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use acp_stack::api::{self, AppState, RuntimePaths};
use acp_stack::auth::AuthVerifierSet;
use acp_stack::config::{Config, ExtensionConfig, ExtensionType, load_config_from_str};
use acp_stack::secrets::{
    CredentialSource, ProviderCredential, ProviderCredentialSet, SecretStore,
};
use acp_stack::state::{EventFilter, StateStore};
use reqwest::StatusCode;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::Mutex as TokioMutex;
use tokio::task::JoinHandle;

const SESSION_KEY: &str = "acps_session_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const ADMIN_KEY: &str = "acps_admin_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const NAMESPACE: &str = "platform-state";
const PEER_NAMESPACE: &str = "peer-state";

/// Serializes HOME mutations across the parallel `#[tokio::test]` functions
/// here. Sound only while EVERY HOME read in this binary holds this guard: a
/// test or helper that reads HOME without `HomeEnvGuard::set` races the unsafe
/// `set_var` below and is undefined behavior on multi-threaded runs.
static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct HomeEnvGuard<'a> {
    _lock: std::sync::MutexGuard<'a, ()>,
    previous: Option<std::ffi::OsString>,
}

impl HomeEnvGuard<'_> {
    fn set(home: &std::path::Path) -> Self {
        let lock = HOME_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = std::env::var_os("HOME");
        // SAFETY: HOME_LOCK is held, and every HOME read in this binary routes
        // through this guard, so no read races the mutation.
        unsafe {
            std::env::set_var("HOME", home);
        }
        Self {
            _lock: lock,
            previous,
        }
    }
}

impl Drop for HomeEnvGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: lock still held while the prior HOME is restored.
        unsafe {
            match self.previous.take() {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
        }
    }
}

struct ServerHarness {
    base_url: String,
    home: PathBuf,
    client: reqwest::Client,
    join: JoinHandle<acp_stack::error::Result<()>>,
    _tempdir: TempDir,
    state: Arc<TokioMutex<StateStore>>,
    _home_guard: HomeEnvGuard<'static>,
}

impl ServerHarness {
    async fn spawn() -> Self {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let home_guard = HomeEnvGuard::set(tempdir.path());
        // The handler's SecretStore::open requires an existing store.
        SecretStore::open_or_create(tempdir.path()).expect("create secret store");

        let state_path = tempdir.path().join("state.sqlite");
        let store = StateStore::open(&state_path).expect("state open");
        store.migrate().expect("migrate");
        store
            .insert_auth_key_pair(&AuthVerifierSet::create(SESSION_KEY, ADMIN_KEY))
            .expect("seed auth verifiers");

        let config_path = tempdir.path().join("acps-config.toml");
        let config = test_config();
        std::fs::write(
            &config_path,
            config.to_canonical_toml().expect("canonical test config"),
        )
        .expect("write runtime config");

        let runtime_paths = RuntimePaths::new(config_path, state_path);
        let app_state = AppState::with_effective_bind_and_runtime_paths(
            config,
            store,
            SESSION_KEY.to_owned(),
            ADMIN_KEY.to_owned(),
            "127.0.0.1:7700".to_owned(),
            runtime_paths,
        );
        let state = app_state.state.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let local = listener.local_addr().expect("local addr");
        let join = tokio::spawn(async move { api::serve(app_state, listener).await });
        Self {
            base_url: format!("http://{local}"),
            home: tempdir.path().to_path_buf(),
            client: reqwest::Client::new(),
            join,
            _tempdir: tempdir,
            state,
            _home_guard: home_guard,
        }
    }

    async fn post_apply(&self, namespace: &str, key: &str, body: Value) -> reqwest::Response {
        self.client
            .post(format!(
                "{}/v1/admin/extensions/{namespace}/apply",
                self.base_url
            ))
            .bearer_auth(key)
            .json(&body)
            .send()
            .await
            .expect("apply request")
    }

    fn reopen_store(&self) -> SecretStore {
        SecretStore::open(&self.home).expect("reopen secret store")
    }

    /// The handler reloads the runtime config from disk on every apply, so
    /// rewriting the file stages a config change mid-test.
    fn rewrite_runtime_config(&self, config: &Config) {
        std::fs::write(
            self.home.join("acps-config.toml"),
            config.to_canonical_toml().expect("canonical config"),
        )
        .expect("rewrite runtime config");
    }
}

impl Drop for ServerHarness {
    fn drop(&mut self) {
        self.join.abort();
    }
}

fn managed_state_extension() -> ExtensionConfig {
    ExtensionConfig {
        extension_type: ExtensionType::ManagedState,
        provider: Vec::new(),
        provider_timeout: None,
        provider_stderr: Default::default(),
        workload_env: Default::default(),
        capability: Some("provider-credential".to_owned()),
    }
}

fn test_config() -> Config {
    let toml_text = include_str!("fixtures/valid-placebo-stack.toml");
    let mut config = load_config_from_str(toml_text).expect("config parses");
    config
        .extensions
        .insert(NAMESPACE.to_owned(), managed_state_extension());
    config
        .extensions
        .insert(PEER_NAMESPACE.to_owned(), managed_state_extension());
    config
}

fn apply_body(revision: i64, selection: Value) -> Value {
    json!({
        "schema_version": 1,
        "revision": revision,
        "desired": {
            "kind": "provider-credential",
            "selection": selection,
        }
    })
}

fn openai_selection(value: &str) -> Value {
    json!({
        "provider_id": "openai",
        "values": { "OPENAI_API_KEY": value },
    })
}

#[tokio::test]
async fn rejects_session_tier_key() {
    let harness = ServerHarness::spawn().await;
    let response = harness
        .post_apply(NAMESPACE, SESSION_KEY, apply_body(7, Value::Null))
        .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body: Value = response.json().await.expect("error envelope");
    assert_eq!(body["ok"], false);
    assert_eq!(body["error"]["code"], "auth.wrong_kind");
}

#[tokio::test]
async fn rejects_missing_authorization() {
    let harness = ServerHarness::spawn().await;
    let response = harness
        .client
        .post(format!(
            "{}/v1/admin/extensions/{NAMESPACE}/apply",
            harness.base_url
        ))
        .json(&apply_body(7, Value::Null))
        .send()
        .await
        .expect("apply request");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn unknown_namespace_is_not_found() {
    let harness = ServerHarness::spawn().await;
    let response = harness
        .post_apply("no-such-namespace", ADMIN_KEY, apply_body(7, Value::Null))
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body: Value = response.json().await.expect("error envelope");
    assert_eq!(body["error"]["code"], "extensions.not_found");
}

#[tokio::test]
async fn rejects_unsupported_schema_version() {
    let harness = ServerHarness::spawn().await;
    let mut body = apply_body(7, Value::Null);
    body["schema_version"] = json!(2);
    let response = harness.post_apply(NAMESPACE, ADMIN_KEY, body).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("error envelope");
    assert_eq!(body["error"]["code"], "request.invalid_param");
}

#[tokio::test]
async fn rejects_nonpositive_revision() {
    let harness = ServerHarness::spawn().await;
    let response = harness
        .post_apply(NAMESPACE, ADMIN_KEY, apply_body(0, Value::Null))
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn rejects_missing_desired_and_missing_selection_keys() {
    let harness = ServerHarness::spawn().await;
    let response = harness
        .post_apply(
            NAMESPACE,
            ADMIN_KEY,
            json!({"schema_version": 1, "revision": 7}),
        )
        .await;
    assert!(response.status().is_client_error());

    // Absent `selection` key must be a parse error, not a destructive clear.
    let response = harness
        .post_apply(
            NAMESPACE,
            ADMIN_KEY,
            json!({
                "schema_version": 1,
                "revision": 7,
                "desired": { "kind": "provider-credential" },
            }),
        )
        .await;
    assert!(response.status().is_client_error());
    assert!(
        harness
            .reopen_store()
            .managed_state_record(NAMESPACE)
            .is_none()
    );
}

#[tokio::test]
async fn rejects_unknown_body_fields() {
    let harness = ServerHarness::spawn().await;
    let mut body = apply_body(7, Value::Null);
    body["relay"] = json!({"endpoint": "https://relay.example"});
    let response = harness.post_apply(NAMESPACE, ADMIN_KEY, body).await;
    assert!(response.status().is_client_error());
}

#[tokio::test]
async fn rejects_unknown_provider() {
    let harness = ServerHarness::spawn().await;
    let response = harness
        .post_apply(
            NAMESPACE,
            ADMIN_KEY,
            apply_body(
                7,
                json!({
                    "provider_id": "definitely-unknown-provider",
                    "values": { "SOME_KEY": "value" },
                }),
            ),
        )
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("error envelope");
    assert_eq!(body["error"]["code"], "request.invalid_param");
}

#[tokio::test]
async fn rejects_missing_required_companion_env_var() {
    let harness = ServerHarness::spawn().await;
    let response = harness
        .post_apply(
            NAMESPACE,
            ADMIN_KEY,
            apply_body(
                7,
                json!({
                    "provider_id": "cloudflare-ai-gateway",
                    "values": { "CLOUDFLARE_API_KEY": "cf-key" },
                }),
            ),
        )
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn rejects_env_var_outside_provider_contract() {
    let harness = ServerHarness::spawn().await;
    let response = harness
        .post_apply(
            NAMESPACE,
            ADMIN_KEY,
            apply_body(
                7,
                json!({
                    "provider_id": "openai",
                    "values": {
                        "OPENAI_API_KEY": "sk-value",
                        "UNRELATED_ENV": "value",
                    },
                }),
            ),
        )
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

fn config_with_custom_provider() -> Config {
    use acp_stack::config::{AgentCustomProviderConfig, AgentProviderConfig, CustomProviderApi};
    let mut config = test_config();
    config.agent.env.push("MY_CUSTOM_KEY".to_owned());
    config.agent.provider = Some(AgentProviderConfig {
        id: "my-custom".to_owned(),
        model: Some("my-model".to_owned()),
        api_key_ref: Some("MY_CUSTOM_KEY".to_owned()),
        custom: Some(AgentCustomProviderConfig {
            name: "My Custom".to_owned(),
            base_url: "https://example.test/v1".to_owned(),
            api: CustomProviderApi::default(),
            model_name: None,
            context: 128_000,
            output_max_tokens: 8_192,
        }),
    });
    config
}

fn custom_selection(value: &str) -> Value {
    json!({
        "provider_id": "my-custom",
        "values": { "MY_CUSTOM_KEY": value },
    })
}

#[tokio::test]
async fn custom_provider_apply_uses_configured_api_key_ref_contract() {
    let harness = ServerHarness::spawn().await;
    harness.rewrite_runtime_config(&config_with_custom_provider());

    let response = harness
        .post_apply(
            NAMESPACE,
            ADMIN_KEY,
            apply_body(3, custom_selection("ck-1")),
        )
        .await;
    let status = response.status();
    let body: Value = response.json().await.expect("envelope");
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["outcome"], "applied");
    let store = harness.reopen_store();
    let (credential, _alias) = store
        .provider_credential_set("my-custom")
        .expect("catalog set")
        .selected(None)
        .expect("selected credential");
    assert_eq!(
        credential.values.get("MY_CUSTOM_KEY").map(String::as_str),
        Some("ck-1")
    );
}

/// The handler reload is lenient: an MCP server the strict loader would
/// reject is dropped rather than failing an unrelated credential rotation.
#[tokio::test]
async fn apply_succeeds_when_the_runtime_config_carries_an_invalid_mcp_server() {
    use acp_stack::config::{McpServerConfig, McpStdioServer};

    let harness = ServerHarness::spawn().await;
    let mut config = config_with_custom_provider();
    config
        .mcp
        .servers
        .push(McpServerConfig::Stdio(McpStdioServer {
            name: "broken".to_owned(),
            command: String::new(),
            args: Vec::new(),
            env: Vec::new(),
        }));
    harness.rewrite_runtime_config(&config);

    let response = harness
        .post_apply(
            NAMESPACE,
            ADMIN_KEY,
            apply_body(3, custom_selection("ck-1")),
        )
        .await;
    let status = response.status();
    let body: Value = response.json().await.expect("envelope");
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["outcome"], "applied");
    let store = harness.reopen_store();
    let (credential, _alias) = store
        .provider_credential_set("my-custom")
        .expect("catalog set")
        .selected(None)
        .expect("selected credential");
    assert_eq!(
        credential.values.get("MY_CUSTOM_KEY").map(String::as_str),
        Some("ck-1")
    );
}

#[tokio::test]
async fn custom_provider_apply_rejects_keys_outside_configured_contract() {
    let harness = ServerHarness::spawn().await;
    harness.rewrite_runtime_config(&config_with_custom_provider());

    let response = harness
        .post_apply(
            NAMESPACE,
            ADMIN_KEY,
            apply_body(
                3,
                json!({
                    "provider_id": "my-custom",
                    "values": { "MY_CUSTOM_KEY": "ck-1", "EXTRA_KEY": "nope" },
                }),
            ),
        )
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let response = harness
        .post_apply(
            NAMESPACE,
            ADMIN_KEY,
            apply_body(
                3,
                json!({
                    "provider_id": "my-custom",
                    "values": { "WRONG_KEY": "ck-1" },
                }),
            ),
        )
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        harness
            .reopen_store()
            .managed_state_record(NAMESPACE)
            .is_none()
    );
}

#[tokio::test]
async fn custom_provider_apply_retries_same_revision_after_config_lands() {
    let harness = ServerHarness::spawn().await;

    // No custom provider declared yet: rejected before any watermark persists.
    let response = harness
        .post_apply(
            NAMESPACE,
            ADMIN_KEY,
            apply_body(5, custom_selection("ck-1")),
        )
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("error envelope");
    assert_eq!(body["error"]["code"], "request.invalid_param");
    assert!(
        harness
            .reopen_store()
            .managed_state_record(NAMESPACE)
            .is_none()
    );

    harness.rewrite_runtime_config(&config_with_custom_provider());
    let response = harness
        .post_apply(
            NAMESPACE,
            ADMIN_KEY,
            apply_body(5, custom_selection("ck-1")),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("envelope");
    assert_eq!(body["data"]["outcome"], "applied");
}

#[tokio::test]
async fn apply_replay_conflict_stale_and_clear_lifecycle() {
    let harness = ServerHarness::spawn().await;

    let response = harness
        .post_apply(
            NAMESPACE,
            ADMIN_KEY,
            apply_body(7, openai_selection("sk-a")),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("envelope");
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["applied_revision"], 7);
    assert_eq!(body["data"]["outcome"], "applied");
    {
        let store = harness.reopen_store();
        let credential = store
            .provider_credential_set("openai")
            .and_then(|set| set.sole.as_ref())
            .expect("stored credential");
        assert_eq!(credential.values["OPENAI_API_KEY"], "sk-a");
        assert_eq!(
            credential.source,
            CredentialSource::External(NAMESPACE.to_owned())
        );
    }

    let response = harness
        .post_apply(
            NAMESPACE,
            ADMIN_KEY,
            apply_body(7, openai_selection("sk-a")),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("envelope");
    assert_eq!(body["data"]["outcome"], "noop");

    let response = harness
        .post_apply(
            NAMESPACE,
            ADMIN_KEY,
            apply_body(7, openai_selection("sk-b")),
        )
        .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body: Value = response.json().await.expect("envelope");
    assert_eq!(body["error"]["code"], "extensions.revision_conflict");

    let response = harness
        .post_apply(NAMESPACE, ADMIN_KEY, apply_body(6, Value::Null))
        .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);

    let response = harness
        .post_apply(NAMESPACE, ADMIN_KEY, apply_body(8, Value::Null))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("envelope");
    assert_eq!(body["data"]["outcome"], "cleared");
    {
        let store = harness.reopen_store();
        assert!(store.provider_credential_set("openai").is_none());
        let record = store
            .managed_state_record(NAMESPACE)
            .expect("watermark survives clear");
        assert_eq!(record.revision, 8);
        assert!(record.provider_id.is_none());
    }
}

fn openai_selection_with_base_url(value: &str, base_url: &str) -> Value {
    json!({
        "provider_id": "openai",
        "values": { "OPENAI_API_KEY": value },
        "base_url": base_url,
    })
}

/// An endpoint override is only accepted for an agent whose registry entry
/// declares `set_provider_base_url`, so these tests repoint the placebo.
fn use_endpoint_capable_agent(harness: &ServerHarness) {
    let mut config = test_config();
    config.agent.id = "opencode".to_owned();
    harness.rewrite_runtime_config(&config);
}

/// Codex declares `set_provider_base_url`, so it exercises the per-provider
/// check rather than the agent-level one.
fn use_codex_agent(harness: &ServerHarness) {
    let mut config = test_config();
    config.agent.id = "codex".to_owned();
    harness.rewrite_runtime_config(&config);
}

fn openrouter_selection_with_base_url(value: &str, base_url: &str) -> Value {
    json!({
        "provider_id": "openrouter",
        "values": { "OPENROUTER_API_KEY": value },
        "base_url": base_url,
    })
}

#[tokio::test]
async fn rejects_a_base_url_for_an_agent_without_an_endpoint_field() {
    let harness = ServerHarness::spawn().await;
    let response = harness
        .post_apply(
            NAMESPACE,
            ADMIN_KEY,
            apply_body(
                1,
                openai_selection_with_base_url("sk-a", "http://127.0.0.1:3129/openai"),
            ),
        )
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("error envelope");
    assert!(
        body["error"].to_string().contains("custom endpoint"),
        "{body}"
    );
    let store = harness.reopen_store();
    assert!(store.managed_state_record(NAMESPACE).is_none());
    assert!(store.provider_credential_set("openai").is_none());
}

#[tokio::test]
async fn rejects_a_base_url_for_codex_built_in_openai() {
    let harness = ServerHarness::spawn().await;
    use_codex_agent(&harness);
    let response = harness
        .post_apply(
            NAMESPACE,
            ADMIN_KEY,
            apply_body(
                1,
                openai_selection_with_base_url("sk-a", "http://127.0.0.1:3129/openai"),
            ),
        )
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("error envelope");
    assert_eq!(body["error"]["code"], "request.invalid_param");
    assert!(
        body["error"]
            .to_string()
            .contains("invalid parameter `desired.selection.base_url`"),
        "{body}"
    );
    assert!(body["error"].to_string().contains("openrouter"), "{body}");
    let store = harness.reopen_store();
    assert!(store.managed_state_record(NAMESPACE).is_none());
    assert!(store.provider_credential_set("openai").is_none());
}

/// The endpoint refusal is scoped to routing, not to the key: a raw key for
/// Codex's built-in openai provider is an ordinary managed credential.
#[tokio::test]
async fn codex_accepts_a_keyed_openai_selection_without_a_base_url() {
    let harness = ServerHarness::spawn().await;
    use_codex_agent(&harness);
    let response = harness
        .post_apply(
            NAMESPACE,
            ADMIN_KEY,
            apply_body(1, openai_selection("sk-a")),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let store = harness.reopen_store();
    let credential = store
        .provider_credential_set("openai")
        .and_then(|set| set.sole.as_ref())
        .expect("openai credential");
    assert!(credential.base_url.is_none());
}

#[tokio::test]
async fn a_rejected_endpoint_revision_stays_reusable() {
    let harness = ServerHarness::spawn().await;
    use_codex_agent(&harness);
    let response = harness
        .post_apply(
            NAMESPACE,
            ADMIN_KEY,
            apply_body(
                5,
                openai_selection_with_base_url("sk-a", "http://127.0.0.1:3129/openai"),
            ),
        )
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // Nothing persisted, so the orchestrator can retry the same revision.
    let response = harness
        .post_apply(
            NAMESPACE,
            ADMIN_KEY,
            apply_body(5, openai_selection("sk-a")),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("envelope");
    assert_eq!(body["data"]["outcome"], "applied");
    assert_eq!(body["data"]["applied_revision"], 5);
    let store = harness.reopen_store();
    assert!(store.provider_credential_set("openai").is_some());
    assert_eq!(
        store
            .managed_state_record(NAMESPACE)
            .expect("watermark")
            .revision,
        5
    );
}

#[tokio::test]
async fn codex_accepts_a_base_url_for_openrouter() {
    let harness = ServerHarness::spawn().await;
    use_codex_agent(&harness);
    let base_url = "http://127.0.0.1:3129/openrouter";
    let response = harness
        .post_apply(
            NAMESPACE,
            ADMIN_KEY,
            apply_body(1, openrouter_selection_with_base_url("sk-a", base_url)),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let store = harness.reopen_store();
    let credential = store
        .provider_credential_set("openrouter")
        .and_then(|set| set.sole.as_ref())
        .expect("stored credential");
    assert_eq!(credential.base_url.as_deref(), Some(base_url));
}

#[tokio::test]
async fn accepts_https_and_loopback_base_urls() {
    for base_url in [
        "https://relay.example/anthropic",
        "http://127.0.0.1:3129/anthropic",
        "http://localhost:3129/openai",
        "http://[::1]:3129/openai",
    ] {
        let harness = ServerHarness::spawn().await;
        use_endpoint_capable_agent(&harness);
        let response = harness
            .post_apply(
                NAMESPACE,
                ADMIN_KEY,
                apply_body(1, openai_selection_with_base_url("sk-a", base_url)),
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK, "base_url {base_url}");
        let store = harness.reopen_store();
        let credential = store
            .provider_credential_set("openai")
            .and_then(|set| set.sole.as_ref())
            .expect("stored credential");
        // Stored verbatim: each agent module appends per its own convention.
        assert_eq!(credential.base_url.as_deref(), Some(base_url));
    }
}

#[tokio::test]
async fn rejects_malformed_base_urls() {
    for (base_url, expected) in [
        ("http://relay.example/anthropic", "loopback"),
        ("ftp://relay.example/anthropic", "loopback"),
        ("not-a-url", "not a valid URL"),
        ("https://user:pw@relay.example/v1", "credentials"),
        ("https://relay.example/v1?key=leak", "query string"),
        ("https://relay.example/v1#frag", "query string"),
    ] {
        let harness = ServerHarness::spawn().await;
        use_endpoint_capable_agent(&harness);
        let response = harness
            .post_apply(
                NAMESPACE,
                ADMIN_KEY,
                apply_body(1, openai_selection_with_base_url("sk-a", base_url)),
            )
            .await;
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "base_url {base_url}"
        );
        let body: Value = response.json().await.expect("error envelope");
        let message = body["error"].to_string();
        assert!(
            message.contains(expected),
            "base_url {base_url} got: {message}"
        );
    }
}

#[tokio::test]
async fn replay_at_the_same_revision_with_a_changed_base_url_conflicts() {
    let harness = ServerHarness::spawn().await;
    use_endpoint_capable_agent(&harness);
    let response = harness
        .post_apply(
            NAMESPACE,
            ADMIN_KEY,
            apply_body(
                4,
                openai_selection_with_base_url("sk-a", "http://127.0.0.1:3129/openai"),
            ),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);

    let response = harness
        .post_apply(
            NAMESPACE,
            ADMIN_KEY,
            apply_body(
                4,
                openai_selection_with_base_url("sk-a", "http://127.0.0.1:3129/openai"),
            ),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("envelope");
    assert_eq!(body["data"]["outcome"], "noop");

    // Same values, different endpoint: routing changed, so the revision must
    // advance rather than silently no-op.
    let response = harness
        .post_apply(
            NAMESPACE,
            ADMIN_KEY,
            apply_body(
                4,
                openai_selection_with_base_url("sk-a", "http://127.0.0.1:3129/other"),
            ),
        )
        .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body: Value = response.json().await.expect("envelope");
    assert_eq!(body["error"]["code"], "extensions.revision_conflict");

    let response = harness
        .post_apply(
            NAMESPACE,
            ADMIN_KEY,
            apply_body(4, openai_selection("sk-a")),
        )
        .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn clearing_the_selection_drops_the_stored_base_url() {
    let harness = ServerHarness::spawn().await;
    use_endpoint_capable_agent(&harness);
    let response = harness
        .post_apply(
            NAMESPACE,
            ADMIN_KEY,
            apply_body(
                2,
                openai_selection_with_base_url("sk-a", "http://127.0.0.1:3129/openai"),
            ),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);

    let response = harness
        .post_apply(NAMESPACE, ADMIN_KEY, apply_body(3, Value::Null))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let store = harness.reopen_store();
    assert!(store.provider_credential_set("openai").is_none());
    assert!(
        store
            .managed_provider_endpoint_override()
            .expect("override lookup")
            .is_none()
    );
}

#[tokio::test]
async fn refuses_operator_owned_and_foreign_namespace_entries() {
    let harness = ServerHarness::spawn().await;
    {
        let mut store = harness.reopen_store();
        store
            .replace_provider_credentials(
                BTreeMap::from([(
                    "openai".to_owned(),
                    ProviderCredentialSet::aliasless(ProviderCredential::new(
                        BTreeMap::from([("OPENAI_API_KEY".to_owned(), "operator".to_owned())]),
                        BTreeMap::new(),
                    )),
                )]),
                &[],
            )
            .expect("seed operator credential");
    }
    let response = harness
        .post_apply(
            NAMESPACE,
            ADMIN_KEY,
            apply_body(7, openai_selection("sk-a")),
        )
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("envelope");
    assert_eq!(body["error"]["code"], "extensions.state_ownership");

    let response = harness
        .post_apply(
            NAMESPACE,
            ADMIN_KEY,
            apply_body(
                7,
                json!({
                    "provider_id": "groq",
                    "values": { "GROQ_API_KEY": "gk-a" },
                }),
            ),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let response = harness
        .post_apply(
            PEER_NAMESPACE,
            ADMIN_KEY,
            apply_body(
                1,
                json!({
                    "provider_id": "groq",
                    "values": { "GROQ_API_KEY": "gk-b" },
                }),
            ),
        )
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("envelope");
    assert_eq!(body["error"]["code"], "extensions.state_ownership");
}

#[tokio::test]
async fn resolves_source_refs_from_secret_store() {
    let harness = ServerHarness::spawn().await;
    {
        let mut store = harness.reopen_store();
        store
            .set("PLATFORM_OPENAI_KEY", "sk-from-ref")
            .expect("seed ref");
    }
    let response = harness
        .post_apply(
            NAMESPACE,
            ADMIN_KEY,
            apply_body(
                7,
                json!({
                    "provider_id": "openai",
                    "source_refs": { "OPENAI_API_KEY": "PLATFORM_OPENAI_KEY" },
                }),
            ),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let store = harness.reopen_store();
    let credential = store
        .provider_credential_set("openai")
        .and_then(|set| set.sole.as_ref())
        .expect("stored credential");
    assert_eq!(credential.values["OPENAI_API_KEY"], "sk-from-ref");
    assert_eq!(
        credential.source_refs["OPENAI_API_KEY"],
        "PLATFORM_OPENAI_KEY"
    );

    let response = harness
        .post_apply(
            NAMESPACE,
            ADMIN_KEY,
            apply_body(
                8,
                json!({
                    "provider_id": "openai",
                    "source_refs": { "OPENAI_API_KEY": "NO_SUCH_REF" },
                }),
            ),
        )
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn audit_event_records_outcome_without_values() {
    let harness = ServerHarness::spawn().await;
    let secret_value = "sk-audit-secret";
    let response = harness
        .post_apply(
            NAMESPACE,
            ADMIN_KEY,
            apply_body(7, openai_selection(secret_value)),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);

    let store = harness.state.lock().await;
    let events = store
        .query_events(EventFilter {
            kind: Some("server.extension_managed_state_applied"),
            limit: 10,
            ..Default::default()
        })
        .expect("query events");
    assert_eq!(events.len(), 1);
    let payload = events[0].payload_json.as_str();
    assert!(payload.contains(NAMESPACE));
    assert!(payload.contains("\"outcome\":\"applied\""));
    assert!(payload.contains("openai"));
    assert!(
        !payload.contains(secret_value),
        "audit payload must never carry credential values"
    );
}

/// At most one provider may be rerouted at a time: a second namespace's
/// `base_url` for a different provider is rejected until the first clears.
#[tokio::test]
async fn a_second_provider_endpoint_override_is_rejected_until_the_first_is_cleared() {
    let harness = ServerHarness::spawn().await;
    use_endpoint_capable_agent(&harness);
    let response = harness
        .post_apply(
            NAMESPACE,
            ADMIN_KEY,
            apply_body(
                1,
                openai_selection_with_base_url("sk-a", "http://127.0.0.1:3129/openai"),
            ),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);

    let response = harness
        .post_apply(
            PEER_NAMESPACE,
            ADMIN_KEY,
            apply_body(
                1,
                openrouter_selection_with_base_url("sk-b", "http://127.0.0.1:3129/openrouter"),
            ),
        )
        .await;
    let status = response.status();
    let body: Value = response.json().await.expect("error envelope");
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert_eq!(body["error"]["code"], "request.invalid_param");
    assert!(
        body["error"]
            .to_string()
            .contains("only one provider may be rerouted at a time"),
        "{body}"
    );
    assert!(
        harness
            .reopen_store()
            .managed_state_record(PEER_NAMESPACE)
            .is_none(),
        "rejected before the watermark persists, so the revision stays reusable"
    );

    let response = harness
        .post_apply(NAMESPACE, ADMIN_KEY, apply_body(2, Value::Null))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let response = harness
        .post_apply(
            PEER_NAMESPACE,
            ADMIN_KEY,
            apply_body(
                1,
                openrouter_selection_with_base_url("sk-b", "http://127.0.0.1:3129/openrouter"),
            ),
        )
        .await;
    let status = response.status();
    let body: Value = response.json().await.expect("envelope");
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["outcome"], "applied");
    assert_eq!(body["data"]["applied_revision"], 1);
}

/// The singleton rule is scoped to distinct providers: advancing the same
/// provider's endpoint under one namespace is an ordinary rotation.
#[tokio::test]
async fn re_applying_an_endpoint_for_the_same_provider_is_allowed() {
    let harness = ServerHarness::spawn().await;
    use_endpoint_capable_agent(&harness);
    let response = harness
        .post_apply(
            NAMESPACE,
            ADMIN_KEY,
            apply_body(
                1,
                openai_selection_with_base_url("sk-a", "http://127.0.0.1:3129/openai"),
            ),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);

    let response = harness
        .post_apply(
            NAMESPACE,
            ADMIN_KEY,
            apply_body(
                2,
                openai_selection_with_base_url("sk-b", "http://127.0.0.1:3129/openai-2"),
            ),
        )
        .await;
    let status = response.status();
    let body: Value = response.json().await.expect("envelope");
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["outcome"], "applied");
}

/// An applied override changes where the provider's catalog is fetched from,
/// so the cached listing for that provider must be dropped.
#[tokio::test]
async fn applying_an_override_invalidates_the_provider_model_cache() {
    let harness = ServerHarness::spawn().await;
    use_endpoint_capable_agent(&harness);
    let cache_path = acp_stack::runtime::agent::provider_model_catalog::cache_path(&harness.home);
    std::fs::create_dir_all(cache_path.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &cache_path,
        br#"{"version": 1, "providers": {"openai": {"fetched_at": 100, "models": [{"value": "openai/gpt-5.5"}]}}}"#,
    )
    .expect("prime cache");
    assert!(
        acp_stack::runtime::agent::provider_model_catalog::cached_models(&harness.home, "openai")
            .is_some()
    );

    let response = harness
        .post_apply(
            NAMESPACE,
            ADMIN_KEY,
            apply_body(
                1,
                openai_selection_with_base_url("sk-a", "http://127.0.0.1:3129/openai"),
            ),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        acp_stack::runtime::agent::provider_model_catalog::cached_models(&harness.home, "openai")
            .is_none(),
        "an applied override must invalidate the provider's cached catalog"
    );
}
