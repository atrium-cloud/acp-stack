//! Integration tests for `POST /v1/admin/extensions/{name}/apply` (the
//! managed-state extension seam).

use std::collections::BTreeMap;
use std::path::PathBuf;

use acp_stack::api::{self, AppState, RuntimePaths};
use acp_stack::auth::AuthVerifierSet;
use acp_stack::config::{Config, ExtensionConfig, ExtensionType, load_config_from_str};
use acp_stack::secrets::{
    CredentialSource, ProviderCredential, ProviderCredentialSet, SecretStore,
};
use acp_stack::state::{EventFilter, StateHandle, StateStore};
use reqwest::StatusCode;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

const SESSION_KEY: &str = "acps_session_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const ADMIN_KEY: &str = "acps_admin_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const NAMESPACE: &str = "platform-state";
const PEER_NAMESPACE: &str = "peer-state";

struct ServerHarness {
    base_url: String,
    home: PathBuf,
    client: reqwest::Client,
    join: JoinHandle<acp_stack::error::Result<()>>,
    _tempdir: TempDir,
    state: StateHandle,
}

impl ServerHarness {
    async fn spawn() -> Self {
        Self::spawn_with(test_config()).await
    }

    async fn spawn_with(config: Config) -> Self {
        let tempdir = tempfile::tempdir().expect("tempdir");
        // The handler's SecretStore::open requires an existing store.
        SecretStore::open_or_create(tempdir.path()).expect("create secret store");

        let state_path = tempdir.path().join("state.sqlite");
        let store = StateStore::open(&state_path).expect("state open");
        store.migrate().expect("migrate");
        store
            .insert_auth_key_pair(&AuthVerifierSet::create(SESSION_KEY, ADMIN_KEY))
            .expect("seed auth verifiers");

        let config_path = tempdir.path().join("acps-config.toml");
        std::fs::write(
            &config_path,
            config.to_canonical_toml().expect("canonical test config"),
        )
        .expect("write runtime config");

        let runtime_paths =
            RuntimePaths::new(config_path, state_path, tempdir.path().to_path_buf());
        let app_state = AppState::with_effective_bind_and_runtime_paths(
            config,
            store,
            SESSION_KEY.to_owned(),
            ADMIN_KEY.to_owned(),
            "127.0.0.1:7700".to_owned(),
            runtime_paths,
        )
        .expect("app state");
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

    fn runtime_config(&self) -> Config {
        Config::load_from_path(self.home.join("acps-config.toml")).expect("runtime config")
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
    assert!(body["data"].get("agent_environment").is_none());
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

/// A sender still pointing the Agent at a relay is refused, so the mismatch surfaces.
#[tokio::test]
async fn rejects_a_selection_carrying_a_base_url() {
    let harness = ServerHarness::spawn().await;
    let response = harness
        .post_apply(
            NAMESPACE,
            ADMIN_KEY,
            apply_body(
                1,
                json!({
                    "provider_id": "openai",
                    "values": { "OPENAI_API_KEY": "sk-a" },
                    "base_url": "http://127.0.0.1:3129",
                }),
            ),
        )
        .await;
    assert!(response.status().is_client_error(), "{}", response.status());
    let store = harness.reopen_store();
    assert!(store.managed_state_record(NAMESPACE).is_none());
    assert!(store.provider_credential_set("openai").is_none());
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

    let events = harness
        .state
        .read(|store| {
            store.query_events(EventFilter {
                kind: Some("server.extension_managed_state_applied"),
                limit: 10,
                ..Default::default()
            })
        })
        .await
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

/// An applied credential changes what the provider's listing answers, so the cached listing
/// for that provider must be dropped.
#[tokio::test]
async fn applying_a_credential_invalidates_the_provider_model_cache() {
    let harness = ServerHarness::spawn().await;
    let cache_path = acp_stack::runtime::agent::provider_model_catalog::cache_path(&harness.home);
    std::fs::create_dir_all(cache_path.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &cache_path,
        br#"{"version": 2, "providers": {"openai": {"fetched_at": 100, "models": [{"value": "openai/gpt-5.5"}]}}}"#,
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
            apply_body(1, openai_selection("sk-a")),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        acp_stack::runtime::agent::provider_model_catalog::cached_models(&harness.home, "openai")
            .is_none(),
        "an applied credential must invalidate the provider's cached catalog"
    );
}

const OPENROUTER: (&str, &str, &str) = (
    "openrouter",
    "OPENROUTER_API_KEY",
    "sk-or-example-not-a-real-key",
);
const DEEPSEEK: (&str, &str, &str) = ("deepseek", "DEEPSEEK_API_KEY", "sk-example-not-a-real-key");

/// A Pi config whose default slot is DeepSeek, as a single-provider config has it.
fn pi_config() -> Config {
    let mut config = test_config();
    config.agent.id = "pi".to_owned();
    config.agent.provider = Some(acp_stack::config::AgentProviderConfig {
        id: "deepseek".to_owned(),
        model: Some("deepseek-v4.1-flash".to_owned()),
        api_key_ref: None,
        custom: None,
    });
    config
}

fn selection((provider_id, env_var, value): (&str, &str, &str)) -> Value {
    json!({ "provider_id": provider_id, "values": { env_var: value }, "source_refs": {} })
}

fn set_body(revision: i64, selections: &[(&str, &str, &str)]) -> Value {
    let selections = selections
        .iter()
        .copied()
        .map(selection)
        .collect::<Vec<_>>();
    json!({
        "schema_version": 1,
        "revision": revision,
        "desired": { "kind": "provider-credential-set", "selections": selections },
    })
}

async fn apply_set(harness: &ServerHarness, body: Value) -> Value {
    let response = harness.post_apply(NAMESPACE, ADMIN_KEY, body).await;
    let status = response.status();
    let body: Value = response.json().await.expect("envelope");
    assert_eq!(status, StatusCode::OK, "body: {body}");
    body["data"].clone()
}

fn default_slot(config: &Config) -> (String, Option<String>) {
    let provider = config.agent.provider.as_ref().expect("default slot");
    (provider.id.clone(), provider.model.clone())
}

/// A setup deposit's `provider-credential` record is taken over by the set kind: a redrive of the
/// same selection at the same revision is a replay, and a larger set at the next one applies.
#[tokio::test]
async fn a_credential_set_takes_over_a_single_selection_and_resolves_every_key() {
    let harness = ServerHarness::spawn_with(pi_config()).await;
    apply_set(&harness, apply_body(1, selection(DEEPSEEK))).await;

    let data = apply_set(&harness, set_body(1, &[DEEPSEEK])).await;
    assert_eq!(data["outcome"], "noop", "{data}");
    let config = harness.runtime_config();
    assert_eq!(
        config.agent.providers.expect("active set").active,
        ["deepseek"]
    );

    let data = apply_set(&harness, set_body(2, &[OPENROUTER, DEEPSEEK])).await;

    assert_eq!(
        data,
        json!({ "applied_revision": 2, "outcome": "applied", "agent_environment": "live" })
    );
    let record = harness.reopen_store().managed_provider_ids(NAMESPACE);
    assert_eq!(
        record.into_iter().collect::<Vec<_>>(),
        ["deepseek", "openrouter"]
    );
    let config = harness.runtime_config();
    let active = &config.agent.providers.as_ref().expect("active set").active;
    assert_eq!(active, &["openrouter", "deepseek"]);
    assert_eq!(
        default_slot(&config),
        (
            "deepseek".to_owned(),
            Some("deepseek-v4.1-flash".to_owned())
        )
    );
    let environment = acp_stack::runtime::agent::provider_keys::resolve_agent_environment(
        &config,
        &harness.reopen_store(),
    )
    .expect("resolve launch env");
    assert_eq!(environment.env["OPENROUTER_API_KEY"], OPENROUTER.2);
    assert_eq!(environment.env["DEEPSEEK_API_KEY"], DEEPSEEK.2);

    // A `provider-credential` body over the set leaves exactly its own provider in both places.
    let data = apply_set(&harness, apply_body(3, selection(OPENROUTER))).await;

    assert_eq!(data["outcome"], "applied", "{data}");
    let record = harness.reopen_store().managed_provider_ids(NAMESPACE);
    assert_eq!(record.into_iter().collect::<Vec<_>>(), ["openrouter"]);
    let config = harness.runtime_config();
    assert_eq!(
        config.agent.providers.as_ref().expect("active set").active,
        ["openrouter"]
    );
    assert_eq!(default_slot(&config), ("openrouter".to_owned(), None));
}

#[tokio::test]
async fn a_shrunk_set_drops_the_provider_and_its_replay_converges() {
    let mut config = pi_config();
    config.agent.subagent = Some(acp_stack::config::AgentSubagentConfig {
        disabled: false,
        provider: config.agent.provider.clone(),
    });
    let harness = ServerHarness::spawn_with(config).await;
    apply_set(&harness, set_body(1, &[OPENROUTER, DEEPSEEK])).await;

    let data = apply_set(&harness, set_body(2, &[OPENROUTER])).await;

    assert_eq!(data["outcome"], "applied");
    // The subagent's provider left the set, so the subagent runs on the default slot.
    let subagent = harness.runtime_config().agent.subagent.expect("subagent");
    assert!(subagent.provider.is_none(), "{subagent:?}");
    assert!(
        harness
            .reopen_store()
            .provider_credential_set("deepseek")
            .is_none()
    );
    let config = harness.runtime_config();
    assert_eq!(
        config.agent.providers.as_ref().expect("active set").active,
        ["openrouter"]
    );
    assert_eq!(default_slot(&config), ("openrouter".to_owned(), None));

    // A config write that never landed is redone by the replay of the same revision.
    harness.rewrite_runtime_config(&pi_config());
    let data = apply_set(&harness, set_body(2, &[OPENROUTER])).await;

    assert_eq!(data["outcome"], "noop");
    assert_eq!(data["agent_environment"], "live");
    let config = harness.runtime_config();
    assert_eq!(
        config.agent.providers.as_ref().expect("active set").active,
        ["openrouter"]
    );
    assert_eq!(default_slot(&config), ("openrouter".to_owned(), None));

    // A `provider-credential` body outside the set takes its place.
    let data = apply_set(&harness, apply_body(3, selection(DEEPSEEK))).await;

    assert_eq!(data["outcome"], "applied", "{data}");
    let record = harness.reopen_store().managed_provider_ids(NAMESPACE);
    assert_eq!(record.into_iter().collect::<Vec<_>>(), ["deepseek"]);
    let config = harness.runtime_config();
    assert_eq!(
        config.agent.providers.as_ref().expect("active set").active,
        ["deepseek"]
    );
    assert_eq!(default_slot(&config), ("deepseek".to_owned(), None));

    // A `provider-credential` clear removes the namespace's providers from both places.
    let data = apply_set(&harness, apply_body(4, Value::Null)).await;

    assert_eq!(data["outcome"], "cleared");
    assert!(
        harness
            .reopen_store()
            .managed_provider_ids(NAMESPACE)
            .is_empty()
    );
    assert!(harness.runtime_config().agent.providers.is_none());
}

/// A set the Agent could never launch is refused unstored, and one whose launch env fails only
/// after the store write keeps its applied answer.
#[tokio::test]
async fn a_set_answers_for_what_it_stored() {
    let mut config = pi_config();
    config.agent.env.push("UNSET_EXAMPLE_TOKEN".to_owned());
    let harness = ServerHarness::spawn_with(config).await;

    let data = apply_set(&harness, set_body(1, &[DEEPSEEK])).await;

    assert_eq!(
        data,
        json!({ "applied_revision": 1, "outcome": "applied", "agent_environment": "restart_required" })
    );
    let response = harness
        .post_apply(
            NAMESPACE,
            ADMIN_KEY,
            set_body(
                2,
                &[
                    (
                        "opencode",
                        "OPENCODE_API_KEY",
                        "sk-oc-example-a-not-a-real-key",
                    ),
                    (
                        "opencode-go",
                        "OPENCODE_API_KEY",
                        "sk-oc-example-b-not-a-real-key",
                    ),
                ],
            ),
        )
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("envelope");
    assert_eq!(body["error"]["code"], "request.invalid_param");
    assert!(body.to_string().contains("different values"), "{body}");
    let store = harness.reopen_store();
    let revision = store
        .managed_state_record(NAMESPACE)
        .map(|record| record.revision);
    assert_eq!(revision, Some(1));
}
