//! `POST /v1/init/credential` tests: the init-tier credential deposit,
//! its revision/replay/ownership semantics, and the shared store handle's
//! visibility contract with the session wizard.

use super::super::*;
use super::support::*;

#[cfg(feature = "test-fixtures")]
use super::super::super::provider::{
    collect_missing_provider_refs, pending_deferred_provider_credential,
};
#[cfg(feature = "test-fixtures")]
use crate::secrets::{SecretStore, new_shared_secret_store};

use http::Method;
use serde_json::json;

#[cfg(feature = "test-fixtures")]
const DEPOSIT_CONFIG_TOML: &str = r#"
[api]
bind = "127.0.0.1:7700"
public_url = "https://agent.example.com"
max_request_bytes = 104857600

[security.http]
max_request_bytes = 104857600
rate_limit_per_minute = 120
burst = 30
auth_failures_per_minute = 5
auth_block_duration = "15m"
allowed_origins = ["https://agent.example.com"]
trust_proxy_headers = false

[workspace]
root = "/workspace"
uploads = "/workspace/uploads"
default_shell = "/bin/bash"
runtime_user = "acp"
max_file_bytes = 8388608

[logging]
level = "info"
local_retention_days = 30

[logging.supabase]
enabled = false
url = "https://example.supabase.co"
api_key_ref = "SUPABASE_SECRET_KEY"
schema = "acp_stack"

[agent]
id = "opencode"
name = "OpenCode"
command = "opencode"
args = ["acp"]
cwd = "/workspace"
env = []
restart = "on-crash"

[extensions.credential-state]
type = "managed-state"
capability = "provider-credential"

[extensions.peer-state]
type = "managed-state"
capability = "provider-credential"
"#;

/// The production topology in miniature: the shared store lives under the
/// same HOME the runtime config and fresh-from-disk discovery reads use.
#[cfg(feature = "test-fixtures")]
struct DepositHarness {
    app: Router,
    store: SharedSecretStore,
    home: std::path::PathBuf,
    _guard: TestEnvGuard,
    _tempdir: tempfile::TempDir,
}

#[cfg(feature = "test-fixtures")]
impl DepositHarness {
    fn new(config_toml: Option<&str>) -> Self {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let home = tempdir.path().join("home");
        if let Some(config_toml) = config_toml {
            let config_dir = home.join(".config").join("acp-stack");
            std::fs::create_dir_all(&config_dir).expect("config dir");
            std::fs::write(config_dir.join("acps-config.toml"), config_toml).expect("write config");
        }
        let guard = TestEnvGuard::set(&[("HOME", home.as_path())]);
        let store = new_shared_secret_store(
            SecretStore::open_or_create(&home).expect("create secret store"),
        );
        let app = app_with_manager_and_store(HostedInitManager::new(store.clone()), store.clone());
        Self {
            app,
            store,
            home,
            _guard: guard,
            _tempdir: tempdir,
        }
    }

    async fn post_deposit(&self, body: Value) -> (StatusCode, Value) {
        request_json(
            self.app.clone(),
            Method::POST,
            "/v1/init/credential",
            Some(body),
            Some(TEST_TOKEN),
        )
        .await
    }
}

fn deposit_body(revision: i64, secrets: Value, selection: Value) -> Value {
    json!({
        "secrets": secrets,
        "namespace": "credential-state",
        "apply": {
            "schema_version": 1,
            "revision": revision,
            "desired": {
                "kind": "provider-credential",
                "selection": selection,
            }
        }
    })
}

fn openrouter_deposit(revision: i64) -> Value {
    deposit_body(
        revision,
        json!([{ "name": "PROVIDER_CAPSULE", "value": "sealed-capsule-string" }]),
        json!({
            "provider_id": "openrouter",
            "source_refs": { "OPENROUTER_API_KEY": "PROVIDER_CAPSULE" },
            "base_url": "http://127.0.0.1:8787",
        }),
    )
}

#[cfg(feature = "test-fixtures")]
#[tokio::test]
async fn deposit_writes_secrets_and_applies_selection_atomically() {
    let harness = DepositHarness::new(Some(DEPOSIT_CONFIG_TOML));
    let (status, body) = harness.post_deposit(openrouter_deposit(7)).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["outcome"], "applied");
    assert_eq!(body["data"]["applied_revision"], 7);
    assert_eq!(body["data"]["secrets_written"], 1);

    // The wizard's own handle observes the deposit without reopening.
    {
        let store = lock_shared_secret_store(&harness.store);
        assert_eq!(
            store.get("PROVIDER_CAPSULE").expect("deposited secret"),
            "sealed-capsule-string"
        );
        let credential = store
            .provider_credential_set("openrouter")
            .and_then(|set| set.sole.as_ref())
            .expect("managed credential");
        assert_eq!(
            credential.base_url.as_deref(),
            Some("http://127.0.0.1:8787")
        );
        assert_eq!(
            credential
                .source_refs
                .get("OPENROUTER_API_KEY")
                .map(String::as_str),
            Some("PROVIDER_CAPSULE")
        );
        // The ref resolved into a value at apply time, so the runtime never
        // needs the flat name again.
        assert_eq!(
            credential
                .values
                .get("OPENROUTER_API_KEY")
                .map(String::as_str),
            Some("sealed-capsule-string")
        );
    }

    // Fresh-from-disk readers (model discovery) see the capsule ref and the
    // managed endpoint override the deposit published.
    let override_ = crate::secrets::managed_provider_endpoint_override_for_home(&harness.home)
        .expect("endpoint override read")
        .expect("endpoint override present");
    assert_eq!(override_.provider_id, "openrouter");
    assert_eq!(override_.base_url, "http://127.0.0.1:8787");
    let reopened = SecretStore::open_read_only(&harness.home).expect("reopen store");
    assert_eq!(
        reopened.get("PROVIDER_CAPSULE").expect("durable secret"),
        "sealed-capsule-string"
    );
}

#[cfg(feature = "test-fixtures")]
#[tokio::test]
async fn identical_replay_at_the_same_revision_is_a_noop() {
    let harness = DepositHarness::new(Some(DEPOSIT_CONFIG_TOML));
    let (status, _) = harness.post_deposit(openrouter_deposit(7)).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = harness.post_deposit(openrouter_deposit(7)).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["outcome"], "noop");
    assert_eq!(body["data"]["applied_revision"], 7);
}

#[cfg(feature = "test-fixtures")]
#[tokio::test]
async fn stale_revision_conflicts() {
    let harness = DepositHarness::new(Some(DEPOSIT_CONFIG_TOML));
    let (status, _) = harness.post_deposit(openrouter_deposit(7)).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = harness.post_deposit(openrouter_deposit(6)).await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {body}");
    assert_eq!(body["error"]["code"], "extensions.revision_conflict");
}

#[cfg(feature = "test-fixtures")]
#[tokio::test]
async fn foreign_namespace_cannot_take_an_owned_provider() {
    let harness = DepositHarness::new(Some(DEPOSIT_CONFIG_TOML));
    let (status, _) = harness.post_deposit(openrouter_deposit(7)).await;
    assert_eq!(status, StatusCode::OK);

    let mut body = openrouter_deposit(1);
    body["namespace"] = json!("peer-state");
    let (status, body) = harness.post_deposit(body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert_eq!(body["error"]["code"], "extensions.state_ownership");
}

#[cfg(feature = "test-fixtures")]
#[tokio::test]
async fn undeclared_namespace_is_not_found() {
    let harness = DepositHarness::new(Some(DEPOSIT_CONFIG_TOML));
    let mut body = openrouter_deposit(7);
    body["namespace"] = json!("ghost-state");
    let (status, body) = harness.post_deposit(body).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "body: {body}");
    assert_eq!(body["error"]["code"], "extensions.not_found");
}

#[cfg(feature = "test-fixtures")]
#[tokio::test]
async fn deposit_before_config_staging_reports_not_ready() {
    let harness = DepositHarness::new(None);
    let (status, body) = harness.post_deposit(openrouter_deposit(7)).await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {body}");
    assert_eq!(body["error"]["code"], "init.config_not_ready");
}

#[tokio::test]
async fn deposit_requires_the_bootstrap_token() {
    let (app, _store_dir) = app_with_manager(HostedInitManager::new(test_shared_secret_store().0));
    let (status, body) = request_json(
        app,
        Method::POST,
        "/v1/init/credential",
        Some(openrouter_deposit(7)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "body: {body}");
    assert_eq!(body["error"]["code"], "auth.missing");
}

#[cfg(feature = "test-fixtures")]
#[tokio::test]
async fn wizard_mutation_after_a_deposit_clobbers_nothing() {
    let harness = DepositHarness::new(Some(DEPOSIT_CONFIG_TOML));
    let (status, _) = harness.post_deposit(openrouter_deposit(7)).await;
    assert_eq!(status, StatusCode::OK);

    // A later wizard-side mutation through the same handle must persist the
    // deposit's writes alongside its own, in memory and on disk.
    lock_shared_secret_store(&harness.store)
        .set("WIZARD_NOTE", "wizard-value")
        .expect("wizard mutation");
    {
        let store = lock_shared_secret_store(&harness.store);
        assert_eq!(
            store.get("PROVIDER_CAPSULE").expect("deposit survives"),
            "sealed-capsule-string"
        );
        assert_eq!(
            store.get("WIZARD_NOTE").expect("wizard write survives"),
            "wizard-value"
        );
        assert!(store.provider_credential_set("openrouter").is_some());
    }
    let reopened = SecretStore::open_read_only(&harness.home).expect("reopen store");
    assert_eq!(
        reopened.get("PROVIDER_CAPSULE").expect("durable deposit"),
        "sealed-capsule-string"
    );
    assert_eq!(
        reopened.get("WIZARD_NOTE").expect("durable wizard write"),
        "wizard-value"
    );
    assert!(
        reopened
            .provider_credential_set("openrouter")
            .and_then(|set| set.sole.as_ref())
            .is_some_and(|credential| credential.base_url.is_some())
    );
}

/// No `defer_provider_credentials` declaration: a missing ref hard-fails until
/// the deposit lands, then the same lane resolves live with no soft-pass.
#[cfg(feature = "test-fixtures")]
#[tokio::test]
async fn deposit_switches_the_provider_lane_to_live_resolution() {
    let harness = DepositHarness::new(Some(DEPOSIT_CONFIG_TOML));
    let config = config::load_config_from_str(DEPOSIT_CONFIG_TOML).expect("config parses");
    let required_refs = vec!["OPENROUTER_API_KEY".to_owned()];

    let outcome = collect_missing_provider_refs(
        false,
        &harness.store,
        &config,
        Some("openrouter"),
        &required_refs,
    );
    assert!(
        outcome.is_err(),
        "an undeposited ref must stay a hard failure without the defer flag"
    );

    let (status, _) = harness.post_deposit(openrouter_deposit(7)).await;
    assert_eq!(status, StatusCode::OK);

    collect_missing_provider_refs(
        false,
        &harness.store,
        &config,
        Some("openrouter"),
        &required_refs,
    )
    .expect("the deposited credential resolves the ref live");
    assert!(
        pending_deferred_provider_credential(&config, &lock_shared_secret_store(&harness.store))
            .is_none(),
        "nothing stays pending once the deposit landed, so discovery spawns live"
    );
}

#[test]
fn deposit_request_rejects_an_invalid_secret_name() {
    let mut request: DepositCredentialRequest =
        serde_json::from_value(openrouter_deposit(7)).expect("request parses");
    request.secrets[0].name = "not a ref name!".to_owned();
    let error = request.validate().expect_err("invalid name must fail");
    assert_eq!(error.error_code(), "request.invalid_param");
}

#[test]
fn deposit_request_rejects_duplicate_secret_names() {
    let mut body = openrouter_deposit(7);
    body["secrets"] = json!([
        { "name": "PROVIDER_CAPSULE", "value": "one" },
        { "name": "PROVIDER_CAPSULE", "value": "two" },
    ]);
    let request: DepositCredentialRequest = serde_json::from_value(body).expect("request parses");
    let error = request.validate().expect_err("duplicate names must fail");
    assert!(
        matches!(
            &error,
            StackError::InvalidParam { field, .. } if *field == "secrets"
        ),
        "the rejection must name the `secrets` field: {error}"
    );
}

#[test]
fn deposit_request_rejects_unknown_fields() {
    let mut body = openrouter_deposit(7);
    body["unexpected"] = json!(true);
    assert!(
        serde_json::from_value::<DepositCredentialRequest>(body).is_err(),
        "unknown request fields must be rejected"
    );
}

#[test]
fn deposit_request_requires_the_selection_key() {
    let body = json!({
        "namespace": "credential-state",
        "apply": {
            "schema_version": 1,
            "revision": 7,
            "desired": { "kind": "provider-credential" }
        }
    });
    assert!(
        serde_json::from_value::<DepositCredentialRequest>(body).is_err(),
        "an absent `selection` key must parse-error rather than read as a clear"
    );
}
