use acp_stack::config::{AgentProviderConfig, Config, ExtensionConfig, ExtensionType};
use acp_stack::secrets::SecretStore;
use reqwest::StatusCode;
use serde_json::{Value, json};

use crate::common::agent::{
    AgentHarness, EnvVarGuard, admin_bearer, http, session_bearer, test_config, write_placebo_shim,
};

const NAMESPACE: &str = "provider-state";
const OPENROUTER: (&str, &str, &str) = (
    "openrouter",
    "OPENROUTER_API_KEY",
    "sk-or-example-not-a-real-key",
);
const DEEPSEEK: (&str, &str, &str) = ("deepseek", "DEEPSEEK_API_KEY", "sk-example-not-a-real-key");

/// A `provider_id` default slot and a managed credential namespace.
fn credential_set_config(provider_id: &str) -> Config {
    let mut config: Config = test_config();
    config.agent.provider = Some(AgentProviderConfig {
        id: provider_id.to_owned(),
        model: None,
        api_key_ref: None,
        custom: None,
    });
    config.extensions.insert(
        NAMESPACE.to_owned(),
        ExtensionConfig {
            extension_type: ExtensionType::ManagedState,
            provider: Vec::new(),
            provider_timeout: None,
            provider_stderr: Default::default(),
            workload_env: Default::default(),
            capability: Some("provider-credential".to_owned()),
        },
    );
    config
}

async fn spawn_for_credential_sets(config: Config) -> AgentHarness {
    let harness = AgentHarness::spawn_with_config(config).await;
    SecretStore::open_or_create(&harness.home).expect("secret store");
    harness
}

async fn apply_set(harness: &AgentHarness, revision: i64, set: &[(&str, &str, &str)]) -> Value {
    let selections = set
        .iter()
        .map(|(provider_id, env_var, value)| {
            json!({ "provider_id": provider_id, "values": { *env_var: value }, "source_refs": {} })
        })
        .collect::<Vec<_>>();
    let response = http()
        .await
        .post(format!(
            "{}/v1/admin/extensions/{NAMESPACE}/apply",
            harness.base_url
        ))
        .header("Authorization", admin_bearer())
        .json(&json!({
            "schema_version": 1,
            "revision": revision,
            "desired": { "kind": "provider-credential-set", "selections": selections },
        }))
        .send()
        .await
        .expect("send apply");
    let status = response.status();
    let body: Value = response.json().await.expect("apply json");
    assert_eq!(status, StatusCode::OK, "body: {body}");
    body["data"].clone()
}

async fn agent_lifecycle(harness: &AgentHarness, action: &str) -> Value {
    let response = http()
        .await
        .post(format!("{}/v1/agent/{action}", harness.base_url))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("send agent lifecycle request");
    let status = response.status();
    let body: Value = response.json().await.expect("agent lifecycle json");
    assert_eq!(status, StatusCode::OK, "{action}: {body}");
    body["data"].clone()
}

async fn agent_status(harness: &AgentHarness) -> Value {
    let body: Value = http()
        .await
        .get(format!("{}/v1/status/agent", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("agent status")
        .json()
        .await
        .expect("agent status json");
    body["data"].clone()
}

/// A set applied under a running Agent waits for its restart, after which the Agent's launch env
/// carries every provider of the set.
#[tokio::test]
async fn a_credential_set_reaches_a_running_agent_through_its_restart() {
    let harness = spawn_for_credential_sets(credential_set_config(OPENROUTER.0)).await;
    apply_set(&harness, 1, &[OPENROUTER]).await;
    agent_lifecycle(&harness, "start").await;

    let data = apply_set(&harness, 2, &[OPENROUTER, DEEPSEEK]).await;
    assert_eq!(data["agent_environment"], "restart_required", "{data}");

    agent_lifecycle(&harness, "restart").await;
    let status = agent_status(&harness).await;
    let loaded = (status["loaded_providers"].as_array().into_iter().flatten())
        .flat_map(|provider| provider["env_names"].as_array().into_iter().flatten())
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    assert!(loaded.contains(&OPENROUTER.1), "{status}");
    assert!(loaded.contains(&DEEPSEEK.1), "{status}");
    assert_eq!(status["provider_restart_required"], false, "{status}");
}

/// After a swap onto a row whose new key is refused at its first endpoint, the next start points
/// Pi's model-less default at the endpoint the key can use and hides the refused one, from the
/// listings that start probes.
#[tokio::test]
async fn a_swapped_in_row_defaults_to_the_endpoint_its_key_can_use() {
    use axum::routing::get;
    const OPENCODE: (&str, &str, &str) = (
        "opencode",
        "OPENCODE_API_KEY",
        "sk-oc-example-not-a-real-key",
    );
    let listings = axum::Router::new()
        .route("/opencode/models", get(|| async { r#"{"data":[]}"# }))
        .route(
            "/opencode-go/models",
            get(|| async { r#"{"data":[{"id":"glm-5.2"}]}"# }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind listings");
    let base = format!(
        "http://{}",
        listener.local_addr().expect("listings address")
    );
    tokio::spawn(async move { axum::serve(listener, listings).await });
    let _listings_gate =
        EnvVarGuard::set_many(vec![("ACP_STACK_PROVIDER_MODELS_BASE", base.into())]);
    let mut config = credential_set_config(DEEPSEEK.0);
    config.agent.id = "pi".to_owned();
    config
        .agent
        .args
        .extend(["--assert-env-equals", "PI_ACP_HIDE_PROVIDERS", "opencode"].map(str::to_owned));
    let harness = spawn_for_credential_sets(config).await;
    apply_set(&harness, 1, &[DEEPSEEK]).await;

    // The apply writes Pi's settings from cached verdicts, of which there are none yet.
    apply_set(&harness, 2, &[OPENCODE]).await;

    let bin_dir = harness.home.join(".local/bin");
    std::fs::create_dir_all(&bin_dir).expect("bin dir");
    write_placebo_shim(&bin_dir.join("pi"));
    let started = agent_lifecycle(&harness, "start").await;
    assert_eq!(
        started["capabilities"]["agent_title"], "env assertions passed",
        "{started}"
    );
    let settings = std::fs::read(harness.home.join(".pi/agent/settings.json")).expect("settings");
    let settings: Value = serde_json::from_slice(&settings).expect("settings json");
    assert_eq!(settings["defaultProvider"], "opencode-go", "{settings}");
}

#[tokio::test]
async fn agent_restart_starts_when_not_running() {
    // Restart on a stopped supervisor degenerates into a plain start.
    let harness = AgentHarness::spawn().await;
    let client = http().await;
    let response = client
        .post(format!("{}/v1/agent/restart", harness.base_url))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("send restart");
    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();
    assert_eq!(status, StatusCode::OK, "body: {body_text}");
    let body: Value = serde_json::from_str(&body_text).expect("restart json");
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["outcome"], "restarted");
    assert!(body["data"]["started_at"].as_str().is_some());
    assert!(body["data"]["stopped_at"].as_str().is_some());
    assert!(body["data"]["capabilities"].is_object());
    // Prior process didn't exist, so prior_exit_status is null.
    assert!(body["data"]["prior_exit_status"].is_null());
}

#[tokio::test]
async fn agent_restart_picks_up_config_written_after_daemon_start() {
    // Regression: the restart handler must re-read config from disk, or the
    // in-memory `state.config` cache hands the supervisor a stale config.
    use serde_json::Value as JsonValue;

    let harness = AgentHarness::spawn().await;
    let client = http().await;
    let initial = std::fs::read_to_string(&harness.config_path).expect("read initial config");

    // Mutate the on-disk config AFTER the daemon cached its copy, pointing
    // `command` at an unresolvable path: reading from disk fails the spawn,
    // while a regression to the cached config would succeed.
    let mutated = initial.replace(
        &format!("command = \"{}\"", env!("CARGO_BIN_EXE_placebo-agent")),
        "command = \"/nonexistent/absolutely-not-a-binary\"",
    );
    std::fs::write(&harness.config_path, &mutated).expect("write mutated config");

    let response = client
        .post(format!("{}/v1/agent/restart", harness.base_url))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("send restart");
    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();
    assert!(
        status.is_server_error() || status == StatusCode::BAD_GATEWAY,
        "restart must fail when on-disk command no longer exists; got {status} body={body_text}",
    );
    let body: JsonValue = serde_json::from_str(&body_text).expect("restart err json");
    let code = body["error"]["code"].as_str().expect("error code present");
    // Either failure proves the on-disk command was honored; a fallback to
    // the cached config would return 200 instead.
    assert!(
        matches!(code, "agent.spawn_failed" | "agent.initialize_failed"),
        "unexpected error code `{code}`; expected agent.spawn_failed or agent.initialize_failed",
    );
}

#[tokio::test]
async fn agent_start_picks_up_config_written_after_daemon_start() {
    use serde_json::Value as JsonValue;

    let harness = AgentHarness::spawn().await;
    let client = http().await;
    let initial = std::fs::read_to_string(&harness.config_path).expect("read initial config");

    let mutated = initial.replace(
        &format!("command = \"{}\"", env!("CARGO_BIN_EXE_placebo-agent")),
        "command = \"/nonexistent/absolutely-not-a-binary\"",
    );
    std::fs::write(&harness.config_path, &mutated).expect("write mutated config");

    let response = client
        .post(format!("{}/v1/agent/start", harness.base_url))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("send start");
    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();
    assert!(
        status.is_server_error() || status == StatusCode::BAD_GATEWAY,
        "start must fail when on-disk command no longer exists; got {status} body={body_text}",
    );
    let body: JsonValue = serde_json::from_str(&body_text).expect("start err json");
    let code = body["error"]["code"].as_str().expect("error code present");
    assert!(
        matches!(code, "agent.spawn_failed" | "agent.initialize_failed"),
        "unexpected error code `{code}`; expected agent.spawn_failed or agent.initialize_failed",
    );
}
