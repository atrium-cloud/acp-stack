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

/// Serializes HOME mutations across the parallel-by-default `#[tokio::test]`
/// functions in this file (same pattern as tests/agent_api_tests.rs). The
/// handler resolves the secret store through `$HOME`, so each test repoints
/// HOME at its own tempdir for the full test body.
///
/// WARNING: this is sound only while every HOME read in this test binary goes
/// through a test holding this guard. A new test (or helper) that reads HOME
/// without taking `HomeEnvGuard::set` races the unsafe `set_var` below and is
/// undefined behavior on multi-threaded runs — route it through the guard.
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
        // SAFETY: HOME_LOCK serializes tests that mutate HOME via this guard.
        // Tests in this binary that depend on HOME route through here, so
        // there's no read racing the mutation.
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
        // SAFETY: lock still held; restore the prior HOME before releasing it
        // so the next test sees a clean slate.
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
        // Initialize the age key + encrypted store the way `acps init` would,
        // so the handler's SecretStore::open finds an existing store.
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
    // Absent `desired` must be a parse error.
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

#[tokio::test]
async fn apply_replay_conflict_stale_and_clear_lifecycle() {
    let harness = ServerHarness::spawn().await;

    // Apply at revision 7.
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

    // Identical replay at revision 7 is a noop.
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

    // Different content at revision 7 conflicts.
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

    // A stale revision conflicts.
    let response = harness
        .post_apply(NAMESPACE, ADMIN_KEY, apply_body(6, Value::Null))
        .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);

    // Clear at revision 8 removes the credential, retains the watermark.
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

    // A different namespace cannot take over another namespace's provider.
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

    // Unknown refs are a payload error, not a 404.
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
