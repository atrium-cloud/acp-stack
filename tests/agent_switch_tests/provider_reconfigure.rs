//! Coverage for `POST /v1/agent/switch` bodies that name the current primary
//! target and carry provider flags: the harness stays, the provider moves.

use reqwest::StatusCode;
use serde_json::{Value, json};
use tempfile::TempDir;

use acp_stack::runtime::agent::switch_journal::{
    SwitchJournalPhase, load_switch_journal, persist_switch_journal, switch_journal_path,
};
use acp_stack::secrets::SecretStore;

use crate::common::agent::{AgentHarness, admin_bearer, http, session_bearer, test_config};

fn seed_provider_secrets(home: &std::path::Path) {
    let mut secrets = SecretStore::open_or_create(home).expect("secret store");
    secrets
        .set_many([
            ("ANTHROPIC_API_KEY", "anthropic-secret"),
            ("OPENAI_API_KEY", "openai-secret"),
        ])
        .expect("provider secrets");
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

async fn primary_process_state(harness: &AgentHarness) -> Value {
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
        .find(|target| target["id"] == "opencode")
        .expect("primary target reported")["process_state"]
        .clone()
}

#[tokio::test]
async fn agent_switch_same_target_sets_provider_and_restarts() {
    let tempdir = TempDir::new().expect("tempdir");
    seed_provider_secrets(tempdir.path());
    let harness =
        AgentHarness::spawn_with_config_and_home(test_config(), tempdir.path().to_path_buf()).await;
    start_primary(&harness).await;

    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "opencode", "provider": "anthropic" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["agent_id"], "opencode");
    assert_eq!(body["data"]["old_agent_id"], "opencode");
    assert_eq!(body["data"]["provider_status"], "set");
    assert_eq!(body["data"]["provider"], "anthropic");
    assert_eq!(body["data"]["api_key_ref"], "ANTHROPIC_API_KEY");
    assert_eq!(
        body["data"]["required_env_refs"],
        json!(["ANTHROPIC_API_KEY"])
    );
    assert_eq!(body["data"]["restarted"], true);
    assert_eq!(body["data"]["restart_started"], true);

    let on_disk = std::fs::read_to_string(&harness.config_path).expect("config after");
    assert!(on_disk.contains(r#"id = "anthropic""#), "config: {on_disk}");
    assert_eq!(primary_process_state(&harness).await, "running");
}

#[tokio::test]
async fn agent_switch_same_target_identical_provider_is_noop() {
    let tempdir = TempDir::new().expect("tempdir");
    seed_provider_secrets(tempdir.path());
    let harness =
        AgentHarness::spawn_with_config_and_home(test_config(), tempdir.path().to_path_buf()).await;

    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "opencode", "provider": "anthropic" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["provider_status"], "set");
    let committed = std::fs::read(&harness.config_path).expect("config after first request");

    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "opencode", "provider": "anthropic" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["provider_status"], "no_op");
    assert_eq!(body["data"]["provider"], "anthropic");
    assert_eq!(body["data"]["restarted"], false);
    assert_eq!(body["data"]["restart_started"], false);
    assert_eq!(
        std::fs::read(&harness.config_path).expect("config after retry"),
        committed,
        "an identical selection must not rewrite the config"
    );
}

/// The completed journal a reconfigure leaves behind must not swallow the next
/// provider change for the same target.
#[tokio::test]
async fn agent_switch_same_target_second_provider_change_applies() {
    let tempdir = TempDir::new().expect("tempdir");
    seed_provider_secrets(tempdir.path());
    let harness =
        AgentHarness::spawn_with_config_and_home(test_config(), tempdir.path().to_path_buf()).await;

    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "opencode", "provider": "anthropic" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");

    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "opencode", "provider": "openai" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["provider_status"], "set");
    assert_eq!(body["data"]["provider"], "openai");
    assert_eq!(body["data"]["api_key_ref"], "OPENAI_API_KEY");

    let on_disk = std::fs::read_to_string(&harness.config_path).expect("config after");
    assert!(on_disk.contains(r#"id = "openai""#), "config: {on_disk}");
}

#[tokio::test]
async fn agent_switch_same_target_unsupported_provider_is_rejected() {
    let tempdir = TempDir::new().expect("tempdir");
    seed_provider_secrets(tempdir.path());
    let harness =
        AgentHarness::spawn_with_config_and_home(test_config(), tempdir.path().to_path_buf()).await;
    let config_before = std::fs::read_to_string(&harness.config_path).expect("config before");

    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "opencode", "provider": "amp-code" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert_eq!(body["error"]["code"], "request.invalid_param");
    assert!(
        body["error"]
            .to_string()
            .contains("provider `amp-code` is not supported for agent `opencode`"),
        "{body}"
    );

    let config_after = std::fs::read_to_string(&harness.config_path).expect("config after");
    assert_eq!(
        config_after, config_before,
        "a rejected provider must not rewrite the config"
    );
    let journal_path = switch_journal_path(&harness.config_path).expect("journal path");
    assert!(
        !journal_path.exists(),
        "a rejected provider must not journal a switch"
    );
}

#[tokio::test]
async fn agent_switch_same_target_with_drop_is_rejected() {
    let tempdir = TempDir::new().expect("tempdir");
    seed_provider_secrets(tempdir.path());
    let harness =
        AgentHarness::spawn_with_config_and_home(test_config(), tempdir.path().to_path_buf()).await;
    let config_before = std::fs::read_to_string(&harness.config_path).expect("config before");

    let (status, body) =
        switch_request(&harness, json!({ "agent_id": "opencode", "drop": true })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert_eq!(body["error"]["code"], "request.invalid_param");

    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "opencode", "provider": "anthropic", "drop": true }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert_eq!(body["error"]["code"], "request.invalid_param");

    let config_after = std::fs::read_to_string(&harness.config_path).expect("config after");
    assert_eq!(
        config_after, config_before,
        "a rejected drop must not rewrite the config"
    );
}

#[tokio::test]
async fn agent_switch_same_target_api_key_ref_without_provider_is_rejected() {
    let tempdir = TempDir::new().expect("tempdir");
    seed_provider_secrets(tempdir.path());
    let harness =
        AgentHarness::spawn_with_config_and_home(test_config(), tempdir.path().to_path_buf()).await;
    let config_before = std::fs::read_to_string(&harness.config_path).expect("config before");

    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "opencode", "api_key_ref": "ANTHROPIC_API_KEY" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert_eq!(body["error"]["code"], "request.invalid_param");
    assert!(
        body["error"]
            .to_string()
            .contains("pass `provider` as well"),
        "{body}"
    );

    let config_after = std::fs::read_to_string(&harness.config_path).expect("config after");
    assert_eq!(
        config_after, config_before,
        "a rejected api-key ref must not rewrite the config"
    );
}

/// A crash between the config write and journal completion leaves a `committed`
/// same-target journal; the identical retry must resume at the runtime re-apply
/// instead of re-running the pre-commit pipeline.
#[tokio::test]
async fn agent_switch_same_target_interrupted_reconfigure_resumes_post_commit() {
    let tempdir = TempDir::new().expect("tempdir");
    seed_provider_secrets(tempdir.path());
    let harness =
        AgentHarness::spawn_with_config_and_home(test_config(), tempdir.path().to_path_buf()).await;
    start_primary(&harness).await;

    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "opencode", "provider": "anthropic" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["provider_status"], "set");

    // Simulate the crash: the committed config is on disk but the journal never
    // advanced past the commit boundary.
    let mut journal = load_switch_journal(&harness.config_path)
        .expect("journal load")
        .expect("journal present");
    assert_eq!(journal.phase, SwitchJournalPhase::Completed);
    assert_eq!(journal.old_target_id, journal.new_target_id);
    assert!(journal.was_running);
    journal.phase = SwitchJournalPhase::Committed;
    persist_switch_journal(&harness.config_path, &journal).expect("persist journal");
    let committed = std::fs::read(&harness.config_path).expect("config after reconfigure");

    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "opencode", "provider": "anthropic" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["provider_status"], "resumed");
    assert_eq!(body["data"]["provider"], "anthropic");
    assert_eq!(body["data"]["api_key_ref"], "ANTHROPIC_API_KEY");
    assert_eq!(body["data"]["restarted"], true);
    assert_eq!(body["data"]["restart_started"], true);
    assert_eq!(
        std::fs::read(&harness.config_path).expect("config after resume"),
        committed,
        "a post-commit resume must not rewrite the config"
    );
    let journal = load_switch_journal(&harness.config_path)
        .expect("journal load")
        .expect("journal present");
    assert_eq!(journal.phase, SwitchJournalPhase::Completed);
    assert_eq!(primary_process_state(&harness).await, "running");
}

/// The same retry carrying a different provider asks for a selection the
/// journaled commit cannot vouch for, so it must conflict instead of silently
/// converging on the journaled provider.
#[tokio::test]
async fn agent_switch_same_target_interrupted_reconfigure_rejects_changed_provider() {
    let tempdir = TempDir::new().expect("tempdir");
    seed_provider_secrets(tempdir.path());
    let harness =
        AgentHarness::spawn_with_config_and_home(test_config(), tempdir.path().to_path_buf()).await;

    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "opencode", "provider": "anthropic" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let mut journal = load_switch_journal(&harness.config_path)
        .expect("journal load")
        .expect("journal present");
    journal.phase = SwitchJournalPhase::Committed;
    persist_switch_journal(&harness.config_path, &journal).expect("persist journal");
    let committed = std::fs::read(&harness.config_path).expect("config after reconfigure");

    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "opencode", "provider": "openai" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {body}");
    assert_eq!(body["error"]["code"], "agent.switch_conflict");
    assert_eq!(
        std::fs::read(&harness.config_path).expect("config after conflict"),
        committed,
        "a conflicting retry must not rewrite the config"
    );
    let journal = load_switch_journal(&harness.config_path)
        .expect("journal load")
        .expect("journal present");
    assert_eq!(
        journal.phase,
        SwitchJournalPhase::Committed,
        "a conflicting retry must not advance the journal"
    );

    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "opencode", "provider": "anthropic" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["provider_status"], "resumed");
    let journal = load_switch_journal(&harness.config_path)
        .expect("journal load")
        .expect("journal present");
    assert_eq!(journal.phase, SwitchJournalPhase::Completed);
}

/// A crash before the config write leaves a `planned` journal and the old
/// config on disk; the identical retry re-runs the full pipeline and converges
/// on the journaled fingerprint, while a changed provider conflicts at the
/// commit boundary.
#[tokio::test]
async fn agent_switch_same_target_pre_commit_interruption_replans_and_converges() {
    let tempdir = TempDir::new().expect("tempdir");
    seed_provider_secrets(tempdir.path());
    let harness =
        AgentHarness::spawn_with_config_and_home(test_config(), tempdir.path().to_path_buf()).await;
    let original = std::fs::read(&harness.config_path).expect("config before");

    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "opencode", "provider": "anthropic" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let mut journal = load_switch_journal(&harness.config_path)
        .expect("journal load")
        .expect("journal present");

    // Simulate the pre-commit crash: the journal holds the anthropic
    // candidate's fingerprint but the write never landed.
    std::fs::write(&harness.config_path, &original).expect("restore config");
    journal.phase = SwitchJournalPhase::Planned;
    persist_switch_journal(&harness.config_path, &journal).expect("persist journal");

    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "opencode", "provider": "openai" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {body}");
    assert_eq!(body["error"]["code"], "agent.switch_conflict");
    assert_eq!(
        std::fs::read(&harness.config_path).expect("config after conflict"),
        original,
        "a conflicting retry must not rewrite the config"
    );

    let (status, body) = switch_request(
        &harness,
        json!({ "agent_id": "opencode", "provider": "anthropic" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["provider_status"], "set");
    assert_eq!(body["data"]["provider"], "anthropic");
    assert_eq!(body["data"]["restarted"], false);
    let journal = load_switch_journal(&harness.config_path)
        .expect("journal load")
        .expect("journal present");
    assert_eq!(journal.phase, SwitchJournalPhase::Completed);
    let on_disk = std::fs::read_to_string(&harness.config_path).expect("config after resume");
    assert!(on_disk.contains(r#"id = "anthropic""#), "config: {on_disk}");
}
