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
const OPENCODE: (&str, &str, &str) = (
    "opencode",
    "OPENCODE_API_KEY",
    "sk-oc-example-not-a-real-key",
);

/// Local model listings that refuse the OpenCode key at `opencode` and serve it at `opencode-go`;
/// the guard points provider listing fetches at them.
async fn serve_opencode_listings() -> EnvVarGuard<'static> {
    use axum::routing::get;
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
    EnvVarGuard::set_many(vec![("ACP_STACK_PROVIDER_MODELS_BASE", base.into())])
}

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

/// An adapter advertising the launch-env extension takes a set in place: the same process
/// receives the set's provider variables and the provider names it leaves out, except names
/// another spawn source still sets.
#[tokio::test]
async fn a_credential_set_reaches_an_advertising_adapter_without_a_restart() {
    let record = tempfile::NamedTempFile::new().expect("record file");
    let mut config = credential_set_config(OPENROUTER.0);
    config.agent.args.extend([
        "--launch-env-record".to_owned(),
        record.path().to_string_lossy().into_owned(),
    ]);
    config.agent.env.push("EXAMPLE_TOOL_TOKEN".to_owned());
    let sandbox_env = &mut config.workspace.sandbox.workload_env;
    sandbox_env.insert(
        "OPENAI_API_KEY".to_owned(),
        "sk-example-not-a-real-key".to_owned(),
    );
    let harness = spawn_for_credential_sets(config).await;
    let mut store = SecretStore::open(&harness.home).expect("secret store");
    store
        .set("EXAMPLE_TOOL_TOKEN", "example-not-a-real-token")
        .expect("tool token");
    apply_set(&harness, 1, &[OPENROUTER]).await;
    agent_lifecycle(&harness, "start").await;
    let pid = agent_status(&harness).await["pid"].clone();

    let data = apply_set(&harness, 2, &[OPENROUTER, DEEPSEEK]).await;

    assert_eq!(data["agent_environment"], "live", "{data}");
    let pushed: Value =
        serde_json::from_slice(&std::fs::read(record.path()).expect("recorded launch env"))
            .expect("launch env json");
    assert_eq!(pushed["env"][OPENROUTER.1], OPENROUTER.2, "{pushed}");
    assert_eq!(pushed["env"][DEEPSEEK.1], DEEPSEEK.2, "{pushed}");
    assert!(
        pushed["env"].get("EXAMPLE_TOOL_TOKEN").is_none(),
        "{pushed}"
    );
    let unset = pushed["unset"].as_array().expect("unset names");
    assert!(unset.contains(&json!("ANTHROPIC_API_KEY")), "{pushed}");
    assert!(!unset.contains(&json!("OPENAI_API_KEY")), "{pushed}");
    assert!(!unset.contains(&json!(DEEPSEEK.1)), "{pushed}");
    assert_eq!(pushed["hideProviders"], json!([]));
    let status = agent_status(&harness).await;
    assert_eq!(status["pid"], pid, "{status}");
    assert_eq!(status["provider_restart_required"], false, "{status}");

    std::fs::write(record.path(), "").expect("clear record");
    let unchanged = apply_set(&harness, 3, &[OPENROUTER, DEEPSEEK]).await;
    assert_eq!(unchanged["outcome"], "applied", "{unchanged}");
    assert_eq!(unchanged["agent_environment"], "live", "{unchanged}");
    assert!(std::fs::read(record.path()).expect("record").is_empty());
}

/// After a swap onto a row whose new key is refused at its first endpoint, the next start points
/// Pi's model-less default at the endpoint the key can use and hides the refused one, from the
/// listings that start probes. A key rotation under the running adapter leaves that default where
/// the adapter's hide list has it.
#[tokio::test]
async fn a_swapped_in_row_defaults_to_the_endpoint_its_key_can_use() {
    let _listings_gate = serve_opencode_listings().await;
    let mut config = credential_set_config(DEEPSEEK.0);
    config.agent.id = "pi".to_owned();
    config
        .agent
        .args
        .extend(["--assert-env-equals", "PI_ACP_HIDE_PROVIDERS", "opencode"].map(str::to_owned));
    let harness = spawn_for_credential_sets(config).await;
    apply_set(&harness, 1, &[DEEPSEEK]).await;

    apply_set(&harness, 2, &[OPENCODE]).await;

    let bin_dir = harness.home.join(".local/bin");
    std::fs::create_dir_all(&bin_dir).expect("bin dir");
    write_placebo_shim(&bin_dir.join("pi"));
    let started = agent_lifecycle(&harness, "start").await;
    assert_eq!(
        started["capabilities"]["agent_title"], "env assertions passed",
        "{started}"
    );
    let default_provider = || {
        let settings = std::fs::read(harness.home.join(".pi/agent/settings.json"));
        serde_json::from_slice::<Value>(&settings.expect("settings")).expect("settings json")
            ["defaultProvider"]
            .clone()
    };
    assert_eq!(default_provider(), "opencode-go");

    // The rotation drops OpenCode's verdicts, from which a rewrite would name `opencode`.
    let rotated = (
        OPENCODE.0,
        OPENCODE.1,
        "sk-oc-example-rotated-not-a-real-key",
    );
    let data = apply_set(&harness, 3, &[rotated]).await;
    assert_eq!(data["agent_environment"], "restart_required", "{data}");
    assert_eq!(default_provider(), "opencode-go");
}

/// A custom provider's key is not among the provider variables a push carries, so rotating it
/// under an advertising adapter needs a restart.
#[tokio::test]
async fn a_custom_provider_rotation_needs_a_restart_under_an_advertising_adapter() {
    use acp_stack::config::{AgentCustomProviderConfig, CustomProviderApi};
    let record = tempfile::NamedTempFile::new().expect("record file");
    let mut config = credential_set_config("my-custom");
    config.agent.env.push("MY_CUSTOM_KEY".to_owned());
    let slot = config.agent.provider.as_mut().expect("default slot");
    slot.model = Some("my-model".to_owned());
    slot.api_key_ref = Some("MY_CUSTOM_KEY".to_owned());
    slot.custom = Some(AgentCustomProviderConfig {
        name: "My Custom".to_owned(),
        base_url: "https://example.test/v1".to_owned(),
        api: CustomProviderApi::default(),
        model_name: None,
        context: 128_000,
        output_max_tokens: 8_192,
    });
    let record_path = record.path().to_string_lossy().into_owned();
    config
        .agent
        .args
        .extend(["--launch-env-record".to_owned(), record_path]);
    let harness = spawn_for_credential_sets(config).await;
    let custom = |value| ("my-custom", "MY_CUSTOM_KEY", value);
    apply_set(&harness, 1, &[custom("example-custom-key-a")]).await;
    agent_lifecycle(&harness, "start").await;

    let data = apply_set(&harness, 2, &[custom("example-custom-key-b")]).await;

    assert_eq!(data["agent_environment"], "restart_required", "{data}");
}

/// A crash after a push that added a row respawns with the pushed set's hide list, which the
/// start on the smaller set never carried.
#[cfg(unix)]
#[tokio::test]
async fn a_respawn_after_a_push_hides_what_the_pushed_set_hides() {
    use super::crash_restart::{kill_process, wait_for_agent_status};
    let _listings_gate = serve_opencode_listings().await;
    let record = tempfile::NamedTempFile::new().expect("record file");
    let mut config = credential_set_config(DEEPSEEK.0);
    config.agent.id = "pi".to_owned();
    config.agent.restart = "on-crash".to_owned();
    let record_path = record.path().to_string_lossy().into_owned();
    config.agent.args.extend([
        "--launch-env-record".to_owned(),
        record_path,
        "--assert-env-equals".to_owned(),
        "PI_ACP_HIDE_PROVIDERS".to_owned(),
        "opencode".to_owned(),
    ]);
    let harness = spawn_for_credential_sets(config).await;
    let bin_dir = harness.home.join(".local/bin");
    std::fs::create_dir_all(&bin_dir).expect("bin dir");
    write_placebo_shim(&bin_dir.join("pi"));
    apply_set(&harness, 1, &[DEEPSEEK]).await;
    let first_pid = agent_lifecycle(&harness, "start").await["pid"].clone();

    let data = apply_set(&harness, 2, &[DEEPSEEK, OPENCODE]).await;
    assert_eq!(data["agent_environment"], "live", "{data}");
    let pid = first_pid.as_u64().and_then(|pid| u32::try_from(pid).ok());
    kill_process(pid.expect("pid"));

    let client = http().await;
    wait_for_agent_status(&client, &harness.base_url, |status| {
        status["process_state"] == "running" && status["pid"] != first_pid
    })
    .await;
    let capabilities: Value = (client.get(format!("{}/v1/agent/capabilities", harness.base_url)))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("capabilities")
        .json()
        .await
        .expect("capabilities json");
    let title = &capabilities["data"]["capabilities"]["agent_title"];
    assert_eq!(title, "env assertions passed", "{capabilities}");
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
