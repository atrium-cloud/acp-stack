//! Coverage for the `model` field of `POST /v1/agent/switch`: a model committed with a harness or
//! provider change, a model-only body on the current target, the same-provider keep, journal
//! retries naming a model, and the agent-owned config restore on a pre-commit failure.

use reqwest::StatusCode;
use serde_json::{Value, json};
use tempfile::TempDir;

use acp_stack::config::{AgentProviderConfig, Config};
use acp_stack::runtime::agent::switch_journal::{
    SwitchJournalPhase, candidate_fingerprint, load_switch_journal, persist_switch_journal,
    switch_journal_path,
};
use acp_stack::secrets::SecretStore;

use crate::common::agent::{
    AgentHarness, EnvVarGuard, add_codex_placebo_target, add_kimi_placebo_target, admin_bearer,
    http, session_bearer, test_config, write_amp_registry_override, write_config_options_fixture,
    write_gated_placebo_shim, write_kimi_registry_override_with_command,
    write_pi_registry_override_with_command,
};

const CONFIG_OPTIONS_FIXTURE_ENV: &str = "ACP_STACK_AGENT_CONFIG_OPTIONS_PATH";
const MODEL_FOLLOW_UP: &str = "acps agent set --model <model-id>";
/// Every model the discovery fixture advertises, across the providers these tests select.
const ADVERTISED_MODELS: &[&str] = &[
    "anthropic/claude-sonnet-4-5",
    "anthropic/claude-haiku-4-5",
    "anthropic/claude-opus-4-8",
    "anthropic/claude-opus-4-5",
    "openai/gpt-5.5",
    "opencode-go/glm-5.3-flash",
    "opencode-go/kimi-k3",
];
/// A value no fixture advertises, distinctive enough that an echo in an error is detectable.
const UNLISTED_MODEL: &str = "unlisted-zeta-9";

fn seed_secrets(home: &std::path::Path) {
    let mut secrets = SecretStore::open_or_create(home).expect("secret store");
    secrets
        .set_many([
            ("ANTHROPIC_API_KEY", "anthropic-secret"),
            ("ANTHROPIC_WORK_KEY", "anthropic-work-secret"),
            ("OPENAI_API_KEY", "openai-secret"),
            ("OPENCODE_API_KEY", "opencode-secret"),
            ("KIMI_API_KEY", "kimi-secret"),
            ("MY_GATEWAY_KEY", "gateway-secret"),
        ])
        .expect("provider secrets");
}

/// The standard opencode fixture with its workspace inside `tempdir`, so `cwd` exists for probes.
fn workspace_config(tempdir: &TempDir) -> Config {
    let mut config = test_config();
    let workspace = tempdir.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    config.workspace.root = workspace.to_string_lossy().into_owned();
    config.workspace.uploads = workspace.join("uploads").to_string_lossy().into_owned();
    config.agent.cwd = Some(config.workspace.root.clone());
    config
}

async fn spawn(tempdir: &TempDir, config: Config) -> AgentHarness {
    seed_secrets(tempdir.path());
    AgentHarness::spawn_with_config_and_home(config, tempdir.path().to_path_buf()).await
}

/// The file that lets the synthetic pi shim start; without it every launch and probe fails.
fn pi_ready_marker(tempdir: &TempDir) -> std::path::PathBuf {
    tempdir.path().join("pi-ready")
}

/// A harness whose registry carries the synthetic pi entry, for different-target switches.
async fn spawn_with_pi_registry(tempdir: &TempDir) -> AgentHarness {
    let config_dir = tempdir.path().join(".config/acp-stack");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    // One shim is the pi adapter command and the `pi` binary the bridge resolves for `PI_ACP_PI_BIN`.
    let bin_dir = tempdir.path().join(".local").join("bin");
    std::fs::create_dir_all(&bin_dir).expect("local bin dir");
    let shim_path = bin_dir.join("pi");
    write_gated_placebo_shim(&shim_path, &pi_ready_marker(tempdir));
    std::fs::write(pi_ready_marker(tempdir), b"ready\n").expect("write pi marker");
    write_pi_registry_override_with_command(&config_dir, &shim_path.to_string_lossy());
    spawn(tempdir, workspace_config(tempdir)).await
}

fn advertised_models_fixture(tempdir: &TempDir) -> EnvVarGuard<'static> {
    let fixture_path = write_config_options_fixture(tempdir.path(), ADVERTISED_MODELS);
    EnvVarGuard::set(CONFIG_OPTIONS_FIXTURE_ENV, &fixture_path)
}

async fn switch_request(harness: &AgentHarness, body: Value) -> (StatusCode, Value) {
    let response = http()
        .await
        .post(format!("{}/v1/agent/switch", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&body)
        .send()
        .await
        .expect("send switch");
    let status = response.status();
    let body: Value = response.json().await.expect("switch json");
    (status, body)
}

async fn start_primary(harness: &AgentHarness) {
    let response = http()
        .await
        .post(format!("{}/v1/agent/start", harness.base_url))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("start primary");
    assert_eq!(response.status(), StatusCode::OK);
}

async fn stop_primary(harness: &AgentHarness) {
    let response = http()
        .await
        .post(format!("{}/v1/agent/stop", harness.base_url))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("stop primary");
    assert_eq!(response.status(), StatusCode::OK);
}

async fn target_field(harness: &AgentHarness, target_id: &str, field: &str) -> Value {
    let body: Value = http()
        .await
        .get(format!("{}/v1/array/status", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("array status")
        .json()
        .await
        .expect("array status json");
    body["data"]["targets"]
        .as_array()
        .expect("targets array")
        .iter()
        .find(|target| target["id"] == target_id)
        .unwrap_or_else(|| panic!("target `{target_id}` should be reported"))
        .get(field)
        .cloned()
        .unwrap_or(Value::Null)
}

fn committed_config(harness: &AgentHarness) -> Config {
    Config::load_from_path(&harness.config_path).expect("committed config loads")
}

fn committed_provider_model(harness: &AgentHarness) -> Option<String> {
    committed_config(harness)
        .agent
        .provider
        .and_then(|provider| provider.model)
}

fn config_bytes(harness: &AgentHarness) -> Vec<u8> {
    std::fs::read(&harness.config_path).expect("config bytes")
}

fn journal_bytes(harness: &AgentHarness) -> Option<Vec<u8>> {
    std::fs::read(switch_journal_path(&harness.config_path).expect("journal path")).ok()
}

fn set_journal_phase(harness: &AgentHarness, phase: SwitchJournalPhase) {
    let mut journal = load_switch_journal(&harness.config_path)
        .expect("journal load")
        .expect("journal present");
    journal.phase = phase;
    persist_switch_journal(&harness.config_path, &journal).expect("persist journal");
}

fn json_file(path: &std::path::Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).expect("json file")).expect("json parses")
}

fn opencode_json_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".config").join("opencode").join("opencode.json")
}

fn pi_settings_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".pi").join("agent").join("settings.json")
}

fn assert_model_rejected(status: StatusCode, body: &Value, value: &str) {
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert_eq!(body["error"]["code"], "request.invalid_param", "{body}");
    let error = body["error"].to_string();
    assert!(
        error.contains("`model`"),
        "the error names the field: {body}"
    );
    assert!(
        !error.contains(value),
        "the error must not echo the value: {body}"
    );
}

#[tokio::test]
async fn different_target_switch_commits_the_resolved_model() {
    let tempdir = TempDir::new().expect("tempdir");
    let _fixture = advertised_models_fixture(&tempdir);
    let harness = spawn_with_pi_registry(&tempdir).await;

    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "pi", "provider": "opencode-go", "model": "glm-5.3-flash" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["agent_id"], "pi");
    assert_eq!(body["data"]["model"], "opencode-go/glm-5.3-flash");
    assert_eq!(body["data"]["set_model"], false);
    assert!(body["data"].get("follow_up").is_none(), "{body}");
    // A provider body starts the new harness even though the source agent was stopped.
    assert_eq!(body["data"]["restarted"], true);
    assert_eq!(body["data"]["restart_started"], true);
    assert_eq!(
        target_field(&harness, "pi", "process_state").await,
        "running"
    );
    assert!(
        !body["data"]["models"]
            .as_array()
            .expect("models array")
            .is_empty(),
        "the discovery probe's advertisement is still reported: {body}"
    );

    let committed = committed_config(&harness);
    let provider = committed.agent.provider.expect("provider committed");
    assert_eq!(provider.id, "opencode-go");
    assert_eq!(provider.model.as_deref(), Some("opencode-go/glm-5.3-flash"));
    assert_eq!(committed.agent.model, None);

    let settings = json_file(&pi_settings_path(tempdir.path()));
    assert_eq!(settings["defaultProvider"], "opencode-go");
    assert_eq!(settings["defaultModel"], "glm-5.3-flash");

    let on_disk = std::fs::read_to_string(&harness.config_path).expect("config text");
    let journal = load_switch_journal(&harness.config_path)
        .expect("journal load")
        .expect("journal present");
    assert_eq!(
        journal.candidate_fingerprint,
        candidate_fingerprint(&on_disk)
    );
    assert_eq!(journal.requested_model.as_deref(), Some("glm-5.3-flash"));
}

/// A model the target does not list is refused before commit. The source harness's files are
/// untouched; the target's own provisioning stays, and a later switch to it rewrites that.
#[tokio::test]
async fn different_target_unadvertised_model_is_refused_before_commit() {
    let tempdir = TempDir::new().expect("tempdir");
    let _fixture = advertised_models_fixture(&tempdir);
    let harness = spawn_with_pi_registry(&tempdir).await;
    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "opencode", "provider": "opencode-go" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let config_before = config_bytes(&harness);
    let journal_before = journal_bytes(&harness);
    let source_before =
        std::fs::read(opencode_json_path(tempdir.path())).expect("source agent-owned config");

    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "pi", "provider": "opencode-go", "model": UNLISTED_MODEL }),
    )
    .await;
    assert_model_rejected(status, &body, UNLISTED_MODEL);
    assert_eq!(config_bytes(&harness), config_before);
    assert_eq!(journal_bytes(&harness), journal_before);
    assert_eq!(
        std::fs::read(opencode_json_path(tempdir.path())).expect("source config after"),
        source_before
    );
    let settings = json_file(&pi_settings_path(tempdir.path()));
    assert_eq!(settings["defaultProvider"], "opencode-go");
    assert!(settings.get("defaultModel").is_none(), "{settings}");

    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "pi", "provider": "anthropic", "model": "claude-sonnet-4-5" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let settings = json_file(&pi_settings_path(tempdir.path()));
    assert_eq!(settings["defaultProvider"], "anthropic");
    assert_eq!(settings["defaultModel"], "claude-sonnet-4-5");
}

#[tokio::test]
async fn same_target_provider_change_commits_the_model_and_restarts() {
    let tempdir = TempDir::new().expect("tempdir");
    let _fixture = advertised_models_fixture(&tempdir);
    let harness = spawn(&tempdir, workspace_config(&tempdir)).await;
    start_primary(&harness).await;

    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "opencode", "provider": "anthropic", "model": "claude-sonnet-4-5" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["provider_status"], "set");
    assert_eq!(body["data"]["provider"], "anthropic");
    assert_eq!(body["data"]["model"], "anthropic/claude-sonnet-4-5");
    assert_eq!(body["data"]["set_model"], false);
    assert_eq!(body["data"]["restarted"], true);
    assert_eq!(body["data"]["restart_started"], true);
    assert_eq!(
        committed_provider_model(&harness).as_deref(),
        Some("anthropic/claude-sonnet-4-5")
    );
    assert_eq!(
        json_file(&opencode_json_path(tempdir.path()))["model"],
        "anthropic/claude-sonnet-4-5"
    );
    assert_eq!(
        target_field(&harness, "opencode", "process_state").await,
        "running"
    );
}

/// A provider change that names no model clears the configured one, and `"model": null` reads as
/// an absent field.
#[tokio::test]
async fn provider_change_without_a_model_clears_it() {
    let tempdir = TempDir::new().expect("tempdir");
    let _fixture = advertised_models_fixture(&tempdir);
    let harness = spawn(&tempdir, workspace_config(&tempdir)).await;
    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "opencode", "provider": "anthropic", "model": "claude-sonnet-4-5" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");

    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "opencode", "provider": "openai", "model": null }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["provider_status"], "set");
    assert!(body["data"].get("model").is_none(), "{body}");
    assert_eq!(body["data"]["set_model"], true);
    assert_eq!(body["data"]["follow_up"], MODEL_FOLLOW_UP);
    assert_eq!(committed_provider_model(&harness), None);
    assert!(
        json_file(&opencode_json_path(tempdir.path()))
            .get("model")
            .is_none()
    );
}

/// After a completed switch, a model-only body commits the model with the provider untouched;
/// the same model again, bare or prefixed, is a no-op that writes nothing.
#[tokio::test]
async fn model_only_body_commits_the_model_with_the_provider_unchanged() {
    let tempdir = TempDir::new().expect("tempdir");
    let _fixture = advertised_models_fixture(&tempdir);
    let harness = spawn(&tempdir, workspace_config(&tempdir)).await;
    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "opencode", "provider": "opencode-go" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(
        load_switch_journal(&harness.config_path)
            .expect("journal load")
            .expect("journal present")
            .phase,
        SwitchJournalPhase::Completed
    );
    let env_before = committed_config(&harness).agent.env;

    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "opencode", "model": "glm-5.3-flash" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["provider_status"], "unchanged");
    assert_eq!(body["data"]["provider"], "opencode-go");
    assert_eq!(body["data"]["api_key_ref"], "OPENCODE_API_KEY");
    assert_eq!(body["data"]["model"], "opencode-go/glm-5.3-flash");
    assert_eq!(body["data"]["set_model"], false);
    assert_eq!(body["data"]["restarted"], true);
    assert_eq!(body["data"]["restart_started"], true);
    let committed = committed_config(&harness);
    assert_eq!(committed.agent.env, env_before);
    let provider = committed.agent.provider.expect("provider kept");
    assert_eq!(provider.id, "opencode-go");
    assert_eq!(provider.model.as_deref(), Some("opencode-go/glm-5.3-flash"));
    assert_eq!(
        json_file(&opencode_json_path(tempdir.path()))["model"],
        "opencode-go/glm-5.3-flash"
    );

    // The no-op must not write agent-owned config, so a removed file stays removed.
    std::fs::remove_file(opencode_json_path(tempdir.path())).expect("remove opencode.json");
    let config_before = config_bytes(&harness);
    for spelling in ["glm-5.3-flash", "opencode-go/glm-5.3-flash"] {
        let (status, body) = switch_request(
            &harness,
            json!({ "agent_id": "opencode", "model": spelling }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "body: {body}");
        assert_eq!(
            body["data"]["provider_status"], "no_op",
            "{spelling}: {body}"
        );
        assert_eq!(body["data"]["model"], "opencode-go/glm-5.3-flash");
        assert_eq!(body["data"]["set_model"], false);
    }
    assert_eq!(config_bytes(&harness), config_before);
    assert!(!opencode_json_path(tempdir.path()).exists());
}

#[tokio::test]
async fn model_only_body_leaves_a_stopped_agent_stopped() {
    let tempdir = TempDir::new().expect("tempdir");
    let _fixture = advertised_models_fixture(&tempdir);
    let harness = spawn(&tempdir, workspace_config(&tempdir)).await;
    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "opencode", "provider": "anthropic" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    stop_primary(&harness).await;

    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "opencode", "model": "claude-haiku-4-5" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["provider_status"], "unchanged");
    assert_eq!(body["data"]["model"], "anthropic/claude-haiku-4-5");
    assert_eq!(body["data"]["restarted"], false);
    assert_eq!(body["data"]["restart_started"], false);
    assert_eq!(
        target_field(&harness, "opencode", "process_state").await,
        "stopped"
    );
}

#[tokio::test]
async fn model_only_body_restarts_a_running_agent_once() {
    let tempdir = TempDir::new().expect("tempdir");
    let _fixture = advertised_models_fixture(&tempdir);
    let harness = spawn(&tempdir, workspace_config(&tempdir)).await;
    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "opencode", "provider": "anthropic" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let pid_before = target_field(&harness, "opencode", "pid").await;
    assert!(pid_before.is_number(), "{pid_before}");

    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "opencode", "model": "claude-haiku-4-5" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["provider_status"], "unchanged");
    assert_eq!(body["data"]["restarted"], true);
    assert_eq!(body["data"]["restart_started"], true);
    let pid_after = target_field(&harness, "opencode", "pid").await;
    assert!(pid_after.is_number(), "{pid_after}");
    assert_ne!(pid_before, pid_after, "the agent restarts with the model");
    assert_eq!(
        target_field(&harness, "opencode", "process_state").await,
        "running"
    );
}

/// Each same-target order (provider change, same provider, model only) refuses an unlisted
/// model with the committed config, journal, and agent-owned files as they were.
#[tokio::test]
async fn same_target_unadvertised_model_restores_agent_owned_config() {
    let tempdir = TempDir::new().expect("tempdir");
    let _fixture = advertised_models_fixture(&tempdir);
    let harness = spawn_with_pi_registry(&tempdir).await;
    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "pi", "provider": "opencode-go", "model": "glm-5.3-flash" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let config_before = config_bytes(&harness);
    let journal_before = journal_bytes(&harness);
    let settings_before = json_file(&pi_settings_path(tempdir.path()));
    assert_eq!(settings_before["defaultProvider"], "opencode-go");
    assert_eq!(settings_before["defaultModel"], "glm-5.3-flash");

    for request in [
        json!({ "agent_id": "pi", "provider": "anthropic", "model": UNLISTED_MODEL }),
        json!({ "agent_id": "pi", "provider": "opencode-go", "model": UNLISTED_MODEL }),
        json!({ "agent_id": "pi", "model": UNLISTED_MODEL }),
    ] {
        let (status, body) = switch_request(&harness, request.clone()).await;
        assert_model_rejected(status, &body, UNLISTED_MODEL);
        assert_eq!(config_bytes(&harness), config_before, "{request}");
        assert_eq!(journal_bytes(&harness), journal_before, "{request}");
        assert_eq!(
            json_file(&pi_settings_path(tempdir.path())),
            settings_before,
            "{request}"
        );
    }
}

/// A harness that takes its model verbatim gets it with no probe: the target command fails on
/// launch and no discovery fixture is set, so a probe would fail the switch before its config write.
#[tokio::test]
async fn explicit_harness_takes_the_model_verbatim_without_a_probe() {
    let _no_fixture = EnvVarGuard::unset(CONFIG_OPTIONS_FIXTURE_ENV);
    let tempdir = TempDir::new().expect("tempdir");
    let shim_path = tempdir.path().join("kimi-shim");
    write_gated_placebo_shim(&shim_path, &tempdir.path().join("never-created"));
    let config_dir = tempdir.path().join(".config/acp-stack");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    write_kimi_registry_override_with_command(&config_dir, &shim_path.to_string_lossy());
    let harness = spawn(&tempdir, workspace_config(&tempdir)).await;

    let (status, body) =
        switch_request(&harness, json!({ "agent_id": "kimi", "model": "kimi-k3" })).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["provider_status"], "not_applicable");
    assert_eq!(body["data"]["model"], "kimi-k3");
    assert_eq!(body["data"]["set_model"], false);
    assert!(
        body["data"]
            .get("models")
            .is_none_or(|models| models.as_array().is_some_and(Vec::is_empty)),
        "{body}"
    );
    assert_eq!(
        committed_config(&harness).agent.model.as_deref(),
        Some("kimi-k3")
    );

    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "kimi", "model": "kimi-k3-turbo" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["provider_status"], "unchanged");
    assert_eq!(body["data"]["model"], "kimi-k3-turbo");
    assert_eq!(
        committed_config(&harness).agent.model.as_deref(),
        Some("kimi-k3-turbo")
    );

    // A provider body starts the agent, which this shim refuses, so the request fails after the
    // config write; a probe would have failed before it and left the config as it was.
    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "kimi", "provider": "anthropic", "model": "claude-sonnet-4-5" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "body: {body}");
    assert_eq!(body["error"]["code"], "agent.initialize_failed");
    assert_eq!(
        load_switch_journal(&harness.config_path)
            .expect("journal load")
            .expect("journal present")
            .phase,
        SwitchJournalPhase::Committed
    );
    let committed = committed_config(&harness);
    assert_eq!(committed.agent.model, None);
    assert_eq!(
        committed
            .agent
            .provider
            .and_then(|provider| provider.model)
            .as_deref(),
        Some("claude-sonnet-4-5")
    );
}

/// A custom provider in force takes a model-only body verbatim where the harness reads custom
/// models without discovery, as `acps agent set --model` does.
#[tokio::test]
async fn custom_provider_model_only_body_is_taken_verbatim() {
    let _no_fixture = EnvVarGuard::unset(CONFIG_OPTIONS_FIXTURE_ENV);
    let tempdir = TempDir::new().expect("tempdir");
    let mut config = workspace_config(&tempdir);
    config.agent.env = vec!["MY_GATEWAY_KEY".to_owned()];
    config.agent.provider = Some(AgentProviderConfig {
        id: "my-gateway".to_owned(),
        model: Some("gateway-model-1".to_owned()),
        api_key_ref: Some("MY_GATEWAY_KEY".to_owned()),
        custom: Some(acp_stack::config::AgentCustomProviderConfig {
            name: "My Gateway".to_owned(),
            base_url: "https://gateway.example/v1".to_owned(),
            api: acp_stack::config::CustomProviderApi::Responses,
            model_name: None,
            context: 200_000,
            output_max_tokens: 64_000,
        }),
    });
    let config = codex_primary(config, tempdir.path());
    let harness = spawn(&tempdir, config).await;

    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "codex", "model": "gateway-model-2" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["provider_status"], "unchanged");
    assert_eq!(body["data"]["provider"], "my-gateway");
    assert_eq!(body["data"]["model"], "gateway-model-2");
    assert_eq!(
        committed_provider_model(&harness).as_deref(),
        Some("gateway-model-2")
    );
}

/// Rewrites the opencode fixture into a codex primary whose command fails on launch, so any
/// discovery probe against it would fail the request.
fn codex_primary(config: Config, root: &std::path::Path) -> Config {
    let mut text = config.to_canonical_toml().expect("canonical config");
    let shim_path = root.join("codex-shim");
    write_gated_placebo_shim(&shim_path, &root.join("never-created"));
    text = text
        .replace(r#"id = "opencode""#, r#"id = "codex""#)
        .replace(r#"name = "OpenCode""#, r#"name = "Codex""#)
        .replace(
            r#"primary_target = "opencode""#,
            r#"primary_target = "codex""#,
        );
    let mut config = acp_stack::config::load_config_from_str(&text).expect("codex config");
    config.agent.command = shim_path.to_string_lossy().into_owned();
    config.agent.args = Vec::new();
    for target in &mut config.array.targets {
        target.agent.command = config.agent.command.clone();
        target.agent.args = Vec::new();
    }
    config
}

#[tokio::test]
async fn model_is_refused_where_it_cannot_apply() {
    let tempdir = TempDir::new().expect("tempdir");
    let config_dir = tempdir.path().join(".config/acp-stack");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    write_amp_registry_override(&config_dir);
    let mut config = workspace_config(&tempdir);
    add_codex_placebo_target(&mut config);
    config.array.enabled = true;
    let harness = spawn(&tempdir, config).await;
    let config_before = config_bytes(&harness);

    let (status, body) =
        switch_request(&harness, json!({ "agent_id": "codex", "model": "gpt-5.5" })).await;
    assert_model_rejected(status, &body, "gpt-5.5");

    let (status, body) =
        switch_request(&harness, json!({ "agent_id": "amp", "model": "amp-smart" })).await;
    assert_model_rejected(status, &body, "amp-smart");

    // opencode selects a mapped provider before a model, and this config has none.
    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "opencode", "model": "glm-5.3-flash" }),
    )
    .await;
    assert_model_rejected(status, &body, "glm-5.3-flash");

    for value in ["", "   ", " glm-5.3-flash "] {
        let (status, body) =
            switch_request(&harness, json!({ "agent_id": "opencode", "model": value })).await;
        assert_model_rejected(status, &body, "glm-5.3-flash");
    }
    assert_eq!(config_bytes(&harness), config_before);
    assert_eq!(journal_bytes(&harness), None);
}

/// After commit only the journaled request spelling vouches for a retry naming a model; before
/// commit both spellings recompute the same candidate and converge.
#[tokio::test]
async fn interrupted_reconfigure_retries_follow_the_requested_model() {
    let tempdir = TempDir::new().expect("tempdir");
    let _fixture = advertised_models_fixture(&tempdir);
    let harness = spawn(&tempdir, workspace_config(&tempdir)).await;
    let original = config_bytes(&harness);
    let selection =
        json!({ "agent_id": "opencode", "provider": "opencode-go", "model": "glm-5.3-flash" });

    let (status, body) = switch_request(&harness, selection.clone()).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    set_journal_phase(&harness, SwitchJournalPhase::Committed);
    let committed = config_bytes(&harness);

    for request in [
        json!({ "agent_id": "opencode", "provider": "opencode-go", "model": "opencode-go/glm-5.3-flash" }),
        json!({ "agent_id": "opencode", "provider": "opencode-go", "model": "kimi-k3" }),
        json!({ "agent_id": "opencode", "model": "kimi-k3" }),
    ] {
        let (status, body) = switch_request(&harness, request.clone()).await;
        assert_eq!(status, StatusCode::CONFLICT, "{request}: {body}");
        assert_eq!(body["error"]["code"], "agent.switch_conflict");
        assert_eq!(config_bytes(&harness), committed, "{request}");
    }

    let (status, body) = switch_request(&harness, selection.clone()).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["provider_status"], "resumed");
    assert_eq!(body["data"]["model"], "opencode-go/glm-5.3-flash");
    assert_eq!(body["data"]["set_model"], false);

    // A retry naming no model resumes, as one naming no provider does.
    set_journal_phase(&harness, SwitchJournalPhase::Committed);
    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "opencode", "provider": "opencode-go" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["provider_status"], "resumed");

    for spelling in ["opencode-go/glm-5.3-flash", "glm-5.3-flash"] {
        std::fs::write(&harness.config_path, &original).expect("restore original config");
        set_journal_phase(&harness, SwitchJournalPhase::Planned);
        let (status, body) = switch_request(
            &harness,
            json!({ "agent_id": "opencode", "provider": "opencode-go", "model": spelling }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{spelling}: {body}");
        assert_eq!(body["data"]["provider_status"], "set");
        assert_eq!(config_bytes(&harness), committed, "{spelling}");
    }
}

#[tokio::test]
async fn interrupted_different_target_switch_conflicts_on_a_different_model() {
    let tempdir = TempDir::new().expect("tempdir");
    let _fixture = advertised_models_fixture(&tempdir);
    let harness = spawn_with_pi_registry(&tempdir).await;
    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "pi", "provider": "opencode-go", "model": "glm-5.3-flash" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    set_journal_phase(&harness, SwitchJournalPhase::Committed);

    let (status, body) =
        switch_request(&harness, json!({ "agent_id": "pi", "model": "kimi-k3" })).await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {body}");
    assert_eq!(body["error"]["code"], "agent.switch_conflict");

    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "pi", "model": "glm-5.3-flash" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["provider_status"], "resumed");
    assert_eq!(body["data"]["model"], "opencode-go/glm-5.3-flash");
}

/// Naming the configured provider keeps its model even when the API-key ref changes.
#[tokio::test]
async fn same_provider_new_api_key_ref_keeps_the_model() {
    let tempdir = TempDir::new().expect("tempdir");
    let _fixture = advertised_models_fixture(&tempdir);
    let harness = spawn(&tempdir, workspace_config(&tempdir)).await;
    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "opencode", "provider": "anthropic", "model": "claude-sonnet-4-5" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");

    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "opencode", "provider": "anthropic", "api_key_ref": "ANTHROPIC_WORK_KEY" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["provider_status"], "set");
    assert_eq!(body["data"]["api_key_ref"], "ANTHROPIC_WORK_KEY");
    assert_eq!(body["data"]["model"], "anthropic/claude-sonnet-4-5");
    assert_eq!(body["data"]["set_model"], false);
    let provider = committed_config(&harness)
        .agent
        .provider
        .expect("provider committed");
    assert_eq!(provider.api_key_ref.as_deref(), Some("ANTHROPIC_WORK_KEY"));
    assert_eq!(
        provider.model.as_deref(),
        Some("anthropic/claude-sonnet-4-5")
    );
}

/// A hand-written root `agent.model` beside a model-less provider slot survives naming that
/// provider again, moved into the slot.
#[tokio::test]
async fn same_provider_keeps_a_root_model_in_the_provider_slot() {
    let tempdir = TempDir::new().expect("tempdir");
    let _fixture = advertised_models_fixture(&tempdir);
    let mut config = workspace_config(&tempdir);
    config.agent.env = vec!["ANTHROPIC_API_KEY".to_owned()];
    config.agent.model = Some("anthropic/claude-sonnet-4-5".to_owned());
    config.agent.provider = Some(AgentProviderConfig {
        id: "anthropic".to_owned(),
        model: None,
        api_key_ref: Some("ANTHROPIC_API_KEY".to_owned()),
        custom: None,
    });
    let harness = spawn(&tempdir, config).await;
    assert_eq!(
        committed_config(&harness).agent.model.as_deref(),
        Some("anthropic/claude-sonnet-4-5")
    );

    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "opencode", "provider": "anthropic" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["provider_status"], "set");
    assert_eq!(body["data"]["model"], "anthropic/claude-sonnet-4-5");
    assert_eq!(body["data"]["set_model"], false);
    let committed = committed_config(&harness);
    assert_eq!(committed.agent.model, None);
    assert_eq!(
        committed
            .agent
            .provider
            .and_then(|provider| provider.model)
            .as_deref(),
        Some("anthropic/claude-sonnet-4-5")
    );
    assert_eq!(
        json_file(&opencode_json_path(tempdir.path()))["model"],
        "anthropic/claude-sonnet-4-5"
    );
}

/// A probe that cannot start fails the switch with 502 before anything commits: a different
/// target leaves the source untouched, and a same-target provider change restores its files.
#[tokio::test]
async fn probe_failure_commits_nothing() {
    let tempdir = TempDir::new().expect("tempdir");
    let fixture = advertised_models_fixture(&tempdir);
    let harness = spawn_with_pi_registry(&tempdir).await;
    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "opencode", "provider": "opencode-go" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    drop(fixture);
    // Without its marker the synthetic pi shim exits before answering `initialize`.
    std::fs::remove_file(pi_ready_marker(&tempdir)).expect("remove pi marker");
    let no_fixture = EnvVarGuard::unset(CONFIG_OPTIONS_FIXTURE_ENV);
    let config_before = config_bytes(&harness);
    let journal_before = journal_bytes(&harness);
    let source_before = std::fs::read(opencode_json_path(tempdir.path())).expect("source config");

    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "pi", "provider": "opencode-go", "model": "glm-5.3-flash" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "body: {body}");
    assert_eq!(body["error"]["code"], "agent.initialize_failed");
    assert_eq!(config_bytes(&harness), config_before);
    assert_eq!(journal_bytes(&harness), journal_before);
    assert_eq!(
        std::fs::read(opencode_json_path(tempdir.path())).expect("source config after"),
        source_before
    );
    drop(no_fixture);

    // The switch starts pi, so the shim must be able to launch.
    std::fs::write(pi_ready_marker(&tempdir), b"ready\n").expect("restore pi marker");
    let fixture = advertised_models_fixture(&tempdir);
    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "pi", "provider": "opencode-go", "model": "glm-5.3-flash" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    drop(fixture);
    std::fs::remove_file(pi_ready_marker(&tempdir)).expect("remove pi marker");
    let _no_fixture = EnvVarGuard::unset(CONFIG_OPTIONS_FIXTURE_ENV);
    let config_before = config_bytes(&harness);
    let journal_before = journal_bytes(&harness);
    let settings_before = json_file(&pi_settings_path(tempdir.path()));

    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "pi", "provider": "anthropic", "model": "claude-sonnet-4-5" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "body: {body}");
    assert_eq!(body["error"]["code"], "agent.initialize_failed");
    assert_eq!(config_bytes(&harness), config_before);
    assert_eq!(journal_bytes(&harness), journal_before);
    assert_eq!(
        json_file(&pi_settings_path(tempdir.path())),
        settings_before
    );
}

/// A commit refused before the config write (a pre-commit retry whose candidate no longer
/// matches the journal) restores the committed agent-owned config and keeps the Planned journal.
#[tokio::test]
async fn commit_failure_before_the_config_write_restores_agent_owned_config() {
    let tempdir = TempDir::new().expect("tempdir");
    let _fixture = advertised_models_fixture(&tempdir);
    let harness = spawn(&tempdir, workspace_config(&tempdir)).await;
    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "opencode", "provider": "anthropic", "model": "claude-sonnet-4-5" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let committed = config_bytes(&harness);

    // Interrupt a model change before its config write: its journal is Planned and its candidate
    // files are on disk, but the committed config is the previous one.
    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "opencode", "model": "claude-haiku-4-5" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    std::fs::write(&harness.config_path, &committed).expect("restore committed config");
    set_journal_phase(&harness, SwitchJournalPhase::Planned);
    let planned = journal_bytes(&harness);

    for model in ["claude-opus-4-8", "claude-opus-4-5"] {
        let (status, body) =
            switch_request(&harness, json!({ "agent_id": "opencode", "model": model })).await;
        assert_eq!(status, StatusCode::CONFLICT, "{model}: {body}");
        assert_eq!(body["error"]["code"], "agent.switch_conflict");
        assert_eq!(config_bytes(&harness), committed);
        assert_eq!(
            journal_bytes(&harness),
            planned,
            "the Planned journal stays"
        );
        assert_eq!(
            json_file(&opencode_json_path(tempdir.path()))["model"],
            "anthropic/claude-sonnet-4-5",
            "agent-owned config is back to the committed model"
        );
    }

    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "opencode", "model": "claude-haiku-4-5" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["model"], "anthropic/claude-haiku-4-5");
}

/// A failure after the config write (the restarted agent cannot start) leaves the agent-owned
/// files on the new config, and the retry resumes.
#[tokio::test]
async fn commit_failure_after_the_config_write_keeps_the_new_files_and_resumes() {
    let tempdir = TempDir::new().expect("tempdir");
    let _fixture = advertised_models_fixture(&tempdir);
    let shim_path = tempdir.path().join("opencode-shim");
    let marker_path = tempdir.path().join("opencode-ready");
    write_gated_placebo_shim(&shim_path, &marker_path);
    std::fs::write(&marker_path, b"ready\n").expect("write marker");
    let mut config = workspace_config(&tempdir);
    config.agent.command = shim_path.to_string_lossy().into_owned();
    config.agent.args = Vec::new();
    for target in &mut config.array.targets {
        target.agent.command = config.agent.command.clone();
        target.agent.args = Vec::new();
    }
    let harness = spawn(&tempdir, config).await;
    start_primary(&harness).await;
    std::fs::remove_file(&marker_path).expect("remove marker");

    let selection =
        json!({ "agent_id": "opencode", "provider": "anthropic", "model": "claude-sonnet-4-5" });
    let (status, body) = switch_request(&harness, selection.clone()).await;
    assert!(status.is_server_error(), "body: {body}");
    assert_eq!(
        load_switch_journal(&harness.config_path)
            .expect("journal load")
            .expect("journal present")
            .phase,
        SwitchJournalPhase::Committed
    );
    assert_eq!(
        committed_provider_model(&harness).as_deref(),
        Some("anthropic/claude-sonnet-4-5")
    );
    assert_eq!(
        json_file(&opencode_json_path(tempdir.path()))["model"],
        "anthropic/claude-sonnet-4-5"
    );

    std::fs::write(&marker_path, b"ready\n").expect("write marker");
    let (status, body) = switch_request(&harness, selection).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["provider_status"], "resumed");
    assert_eq!(body["data"]["model"], "anthropic/claude-sonnet-4-5");
    assert_eq!(body["data"]["restart_started"], true);
}

/// A model change keeps the configured effort, so a session on the new model lists an effort that
/// model does not offer as ignored.
#[tokio::test]
async fn model_change_keeps_the_configured_effort() {
    let _no_fixture = EnvVarGuard::unset(CONFIG_OPTIONS_FIXTURE_ENV);
    let tempdir = TempDir::new().expect("tempdir");
    let mut config = workspace_config(&tempdir);
    config.agent.args = vec![
        "acp".to_owned(),
        "--config-option-select".to_owned(),
        "model@model=anthropic/model-a:anthropic/model-a,anthropic/model-b".to_owned(),
        "--config-option-select".to_owned(),
        "reasoning_effort@thought_level=low:low,medium".to_owned(),
        "--config-option-select-for-model".to_owned(),
        "anthropic/model-b=reasoning_effort@thought_level=high:high,xhigh".to_owned(),
    ];
    config.agent.env = vec!["ANTHROPIC_API_KEY".to_owned()];
    config.agent.effort = Some("low".to_owned());
    config.agent.provider = Some(AgentProviderConfig {
        id: "anthropic".to_owned(),
        model: Some("anthropic/model-a".to_owned()),
        api_key_ref: Some("ANTHROPIC_API_KEY".to_owned()),
        custom: None,
    });
    for target in &mut config.array.targets {
        target.agent = config.agent.clone();
    }
    let harness = spawn(&tempdir, config).await;
    start_primary(&harness).await;

    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "opencode", "model": "model-b" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["model"], "anthropic/model-b");
    assert_eq!(
        committed_config(&harness).agent.effort.as_deref(),
        Some("low")
    );

    let response = http()
        .await
        .post(format!("{}/v1/sessions", harness.base_url))
        .header("Authorization", session_bearer())
        .json(&json!({}))
        .send()
        .await
        .expect("create session");
    assert_eq!(response.status(), StatusCode::OK);
    let session: Value = response.json().await.expect("session json");
    let ignored = session["data"]["ignored"]
        .as_array()
        .expect("ignored array");
    assert!(
        ignored
            .iter()
            .any(|entry| entry["feature"] == "agent.effort" && entry["value"] == "low"),
        "{session}"
    );
}

/// `model` and `set_model` describe the committed config on no-op, resumed, and existing-target
/// selections alike.
#[tokio::test]
async fn model_and_set_model_report_the_committed_config_on_every_path() {
    let tempdir = TempDir::new().expect("tempdir");
    let _fixture = advertised_models_fixture(&tempdir);
    let mut config = workspace_config(&tempdir);
    config.array.enabled = true;
    add_codex_placebo_target(&mut config);
    add_kimi_placebo_target(&mut config);
    if let Some(codex) = config
        .array
        .targets
        .iter_mut()
        .find(|target| target.id == "codex")
    {
        codex.agent.model = Some("gpt-5.5".to_owned());
    }
    let harness = spawn(&tempdir, config).await;

    let (status, body) = switch_request(&harness, json!({ "agent_id": "opencode" })).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["provider_status"], "no_op");
    assert!(body["data"].get("model").is_none(), "{body}");
    assert_eq!(body["data"]["set_model"], true);
    assert_eq!(body["data"]["follow_up"], MODEL_FOLLOW_UP);

    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "opencode", "provider": "anthropic" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    set_journal_phase(&harness, SwitchJournalPhase::Committed);
    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "opencode", "provider": "anthropic" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["provider_status"], "resumed");
    assert_eq!(body["data"]["set_model"], true);
    assert_eq!(body["data"]["follow_up"], MODEL_FOLLOW_UP);

    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "opencode", "model": "claude-sonnet-4-5" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let (status, body) = switch_request(&harness, json!({ "agent_id": "opencode" })).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["provider_status"], "no_op");
    assert_eq!(body["data"]["model"], "anthropic/claude-sonnet-4-5");
    assert_eq!(body["data"]["set_model"], false);
    assert!(body["data"].get("follow_up").is_none(), "{body}");

    let (status, body) = switch_request(&harness, json!({ "agent_id": "codex" })).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["provider_status"], "selected");
    assert_eq!(body["data"]["model"], "gpt-5.5");
    assert_eq!(body["data"]["set_model"], false);

    let (status, body) = switch_request(&harness, json!({ "agent_id": "kimi" })).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["provider_status"], "selected");
    assert!(body["data"].get("model").is_none(), "{body}");
    assert_eq!(body["data"]["set_model"], true);
    assert_eq!(body["data"]["follow_up"], MODEL_FOLLOW_UP);
}
