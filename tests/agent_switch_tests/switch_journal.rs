//! Coverage for the pending-switch journal: same-target retries converge past
//! the commit boundary, different-target retries conflict.
//!
//! Failure is injected with a gated shim that exists from the start (so the
//! installer's spawn gate passes) but exits 1 until a marker file appears.

use reqwest::StatusCode;
use serde_json::{Value, json};
use tempfile::TempDir;

use acp_stack::config::ArrayTargetConfig;
use acp_stack::runtime::agent::switch_journal::{
    SwitchJournal, SwitchJournalPhase, load_switch_journal, persist_switch_journal,
    switch_journal_path,
};
use acp_stack::state::NewSessionRecord;

use crate::common::agent::{
    AgentHarness, EnvVarGuard, admin_bearer, http, session_bearer, test_config,
    write_config_options_fixture, write_gated_placebo_shim, write_kimi_registry_override,
    write_kimi_registry_override_with_command, write_placebo_shim,
};

fn journal_phase(harness: &AgentHarness) -> SwitchJournalPhase {
    load_switch_journal(&harness.config_path)
        .expect("journal load")
        .expect("journal present")
        .phase
}

async fn switch_request(harness: &AgentHarness, agent: &str) -> (StatusCode, Value) {
    let response = http()
        .await
        .post(format!("{}/v1/agent/switch", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&json!({ "agent_id": agent }))
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

/// Harness booted on the standard opencode fixture with a kimi registry
/// override whose harness command is `shim_path`.
async fn spawn_kimi_switch_fixture(tempdir: &TempDir, shim_path: &std::path::Path) -> AgentHarness {
    let config_dir = tempdir.path().join(".config/acp-stack");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    write_kimi_registry_override_with_command(&config_dir, &shim_path.to_string_lossy());
    let mut config = test_config();
    let workspace = tempdir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    config.workspace.root = workspace.to_string_lossy().into_owned();
    config.workspace.uploads = workspace.join("uploads").to_string_lossy().into_owned();
    config.agent.cwd = Some(config.workspace.root.clone());
    AgentHarness::spawn_with_config_and_home(config, tempdir.path().to_path_buf()).await
}

fn set_kimi_secret(tempdir: &TempDir) {
    let mut secrets =
        acp_stack::secrets::SecretStore::open_or_create(tempdir.path()).expect("secret store");
    secrets
        .set_many([("KIMI_API_KEY", "kimi-secret")])
        .expect("kimi secret");
}

#[tokio::test]
async fn agent_switch_retry_after_post_commit_start_failure_converges() {
    let tempdir = TempDir::new().expect("tempdir");
    let shim_path = tempdir.path().join("kimi-shim");
    let marker_path = tempdir.path().join("kimi-ready");
    let fixture_path = write_config_options_fixture(tempdir.path(), &["kimi/kimi-k3"]);
    let _fixture_guard = EnvVarGuard::set("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &fixture_path);
    set_kimi_secret(&tempdir);
    write_gated_placebo_shim(&shim_path, &marker_path);
    let harness = spawn_kimi_switch_fixture(&tempdir, &shim_path).await;
    start_primary(&harness).await;

    let (status, body) = switch_request(&harness, "kimi").await;
    assert!(status.is_server_error(), "body: {body}");
    let on_disk = std::fs::read_to_string(&harness.config_path).expect("read config");
    assert!(on_disk.contains(r#"id = "kimi""#));
    let journal = load_switch_journal(&harness.config_path)
        .expect("journal load")
        .expect("journal present");
    assert_eq!(journal.phase, SwitchJournalPhase::Committed);
    assert!(journal.was_running);
    assert_eq!(
        target_field(&harness, "kimi", "process_state").await,
        "stopped"
    );

    std::fs::write(&marker_path, b"ready\n").expect("write marker");
    let (status, body) = switch_request(&harness, "kimi").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["agent_id"], "kimi");
    assert_eq!(body["data"]["provider_status"], "resumed");
    assert_eq!(body["data"]["restarted"], true);
    assert_eq!(body["data"]["restart_started"], true);
    assert!(body["data"].get("install").is_none());
    assert_eq!(journal_phase(&harness), SwitchJournalPhase::Completed);
    assert_eq!(
        target_field(&harness, "kimi", "process_state").await,
        "running"
    );
}

#[tokio::test]
async fn agent_switch_resume_survives_process_restart() {
    let tempdir = TempDir::new().expect("tempdir");
    let shim_path = tempdir.path().join("kimi-shim");
    let marker_path = tempdir.path().join("kimi-ready");
    let fixture_path = write_config_options_fixture(tempdir.path(), &["kimi/kimi-k3"]);
    let _fixture_guard = EnvVarGuard::set("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &fixture_path);
    set_kimi_secret(&tempdir);
    write_gated_placebo_shim(&shim_path, &marker_path);
    let harness = spawn_kimi_switch_fixture(&tempdir, &shim_path).await;
    start_primary(&harness).await;

    let (status, body) = switch_request(&harness, "kimi").await;
    assert!(status.is_server_error(), "body: {body}");

    // Simulate a daemon restart: the journaled `was_running` is the only
    // surviving record that the old primary was up.
    std::fs::write(&marker_path, b"ready\n").expect("write marker");
    let harness = harness.respawn().await;
    let (status, body) = switch_request(&harness, "kimi").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["provider_status"], "resumed");
    assert_eq!(body["data"]["restarted"], true);
    assert_eq!(body["data"]["restart_started"], true);
    assert_eq!(journal_phase(&harness), SwitchJournalPhase::Completed);
    assert_eq!(
        target_field(&harness, "kimi", "process_state").await,
        "running"
    );
}

#[tokio::test]
async fn agent_switch_existing_target_resume_stops_old_target_left_running() {
    let tempdir = TempDir::new().expect("tempdir");
    let config_dir = tempdir.path().join(".config/acp-stack");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    write_kimi_registry_override(&config_dir);
    set_kimi_secret(&tempdir);
    let shim_path = tempdir.path().join("kimi-shim");
    let mut config = test_config();
    config.array.enabled = true;
    // The escape-hatch install recipe does not spawn-gate the command, so the
    // switch commits and only the start fails.
    let mut kimi = config.agent.clone();
    kimi.id = "kimi".to_owned();
    kimi.name = "Kimi Code".to_owned();
    kimi.command = shim_path.to_string_lossy().into_owned();
    kimi.args = vec!["acp".to_owned()];
    kimi.env = vec!["KIMI_API_KEY".to_owned()];
    kimi.adapter = None;
    config.array.targets.push(ArrayTargetConfig {
        id: "kimi".to_owned(),
        agent: kimi,
    });
    let harness =
        AgentHarness::spawn_with_config_and_home(config, tempdir.path().to_path_buf()).await;
    start_primary(&harness).await;

    let (status, body) = switch_request(&harness, "kimi").await;
    assert!(status.is_server_error(), "body: {body}");
    let journal = load_switch_journal(&harness.config_path)
        .expect("journal load")
        .expect("journal present");
    assert_eq!(journal.phase, SwitchJournalPhase::Committed);

    // Recreate the state a failed old-agent shutdown leaves behind: the old
    // target is still up even though the new primary is committed.
    let restart_old = http()
        .await
        .post(format!(
            "{}/v1/array/targets/opencode/start",
            harness.base_url
        ))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("restart old target");
    assert_eq!(restart_old.status(), StatusCode::OK);

    write_placebo_shim(&shim_path);
    let (status, body) = switch_request(&harness, "kimi").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["provider_status"], "resumed");
    assert_eq!(body["data"]["restarted"], true);
    assert_eq!(body["data"]["restart_started"], true);
    assert_eq!(journal_phase(&harness), SwitchJournalPhase::Completed);
    assert_eq!(
        target_field(&harness, "kimi", "process_state").await,
        "running"
    );
    assert_eq!(
        target_field(&harness, "opencode", "process_state").await,
        "stopped"
    );
}

#[tokio::test]
async fn agent_switch_rename_collision_clears_journal_and_unblocks_retry() {
    let tempdir = TempDir::new().expect("tempdir");
    let shim_path = tempdir.path().join("kimi-shim");
    let fixture_path = write_config_options_fixture(tempdir.path(), &["kimi/kimi-k3"]);
    let _fixture_guard = EnvVarGuard::set("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &fixture_path);
    set_kimi_secret(&tempdir);
    write_placebo_shim(&shim_path);
    let harness = spawn_kimi_switch_fixture(&tempdir, &shim_path).await;

    // Seed the collision the rename check rejects: two sessions sharing an
    // agent_session_id across the current primary and the future target.
    {
        let store = harness.state.lock().await;
        store
            .insert_session_for_target(
                "opencode",
                "agent-session-dup".to_owned(),
                NewSessionRecord {
                    id: "sess_old".to_owned(),
                    agent_id: "opencode".to_owned(),
                    cwd: "/tmp/old".to_owned(),
                    title: None,
                    metadata_json: "{}".to_owned(),
                },
            )
            .expect("old-target session inserted");
        store
            .insert_session_for_target(
                "kimi",
                "agent-session-dup".to_owned(),
                NewSessionRecord {
                    id: "sess_new".to_owned(),
                    agent_id: "kimi".to_owned(),
                    cwd: "/tmp/new".to_owned(),
                    title: None,
                    metadata_json: "{}".to_owned(),
                },
            )
            .expect("new-target session inserted");
    }

    let (status, body) = switch_request(&harness, "kimi").await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {body}");
    assert_eq!(body["error"]["code"], "session.target_rename_conflict");
    let on_disk = std::fs::read_to_string(&harness.config_path).expect("read config");
    assert!(!on_disk.contains(r#"id = "kimi""#), "config: {on_disk}");
    assert_eq!(
        load_switch_journal(&harness.config_path).expect("journal load"),
        None
    );

    {
        let store = harness.state.lock().await;
        store
            .delete_session("sess_new")
            .expect("colliding session deleted");
    }
    let (status, body) = switch_request(&harness, "kimi").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["agent_id"], "kimi");
    assert_eq!(journal_phase(&harness), SwitchJournalPhase::Completed);
}

#[tokio::test]
async fn agent_switch_conflicting_target_is_rejected_while_journal_incomplete() {
    let tempdir = TempDir::new().expect("tempdir");
    let shim_path = tempdir.path().join("kimi-shim");
    let marker_path = tempdir.path().join("kimi-ready");
    let fixture_path = write_config_options_fixture(tempdir.path(), &["kimi/kimi-k3"]);
    let _fixture_guard = EnvVarGuard::set("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &fixture_path);
    set_kimi_secret(&tempdir);
    write_gated_placebo_shim(&shim_path, &marker_path);
    let harness = spawn_kimi_switch_fixture(&tempdir, &shim_path).await;
    start_primary(&harness).await;

    let (status, body) = switch_request(&harness, "kimi").await;
    assert!(status.is_server_error(), "body: {body}");

    let (status, body) = switch_request(&harness, "amp").await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {body}");
    assert_eq!(body["error"]["code"], "agent.switch_conflict");

    std::fs::write(&marker_path, b"ready\n").expect("write marker");
    let (status, body) = switch_request(&harness, "kimi").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["provider_status"], "resumed");
    assert_eq!(journal_phase(&harness), SwitchJournalPhase::Completed);
}

#[tokio::test]
async fn agent_switch_same_primary_still_conflicts_while_foreign_journal_incomplete() {
    let tempdir = TempDir::new().expect("tempdir");
    let harness =
        AgentHarness::spawn_with_config_and_home(test_config(), tempdir.path().to_path_buf()).await;
    let journal = SwitchJournal {
        old_target_id: "opencode".to_owned(),
        new_target_id: "kimi".to_owned(),
        target_agent_id: "kimi".to_owned(),
        candidate_fingerprint: "pending".to_owned(),
        was_running: false,
        phase: SwitchJournalPhase::Planned,
    };
    persist_switch_journal(&harness.config_path, &journal).expect("persist incomplete journal");

    let (status, body) = switch_request(&harness, "opencode").await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {body}");
    assert_eq!(body["error"]["code"], "agent.switch_conflict");
}

#[tokio::test]
async fn agent_switch_stale_completed_journal_still_noops_same_target() {
    let tempdir = TempDir::new().expect("tempdir");
    let harness =
        AgentHarness::spawn_with_config_and_home(test_config(), tempdir.path().to_path_buf()).await;
    let journal = SwitchJournal {
        old_target_id: "opencode".to_owned(),
        new_target_id: "kimi".to_owned(),
        target_agent_id: "kimi".to_owned(),
        candidate_fingerprint: "stale".to_owned(),
        was_running: false,
        phase: SwitchJournalPhase::Completed,
    };
    persist_switch_journal(&harness.config_path, &journal).expect("persist stale journal");
    let config_before = std::fs::read_to_string(&harness.config_path).expect("config before");

    let (status, body) = switch_request(&harness, "opencode").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["provider_status"], "no_op");
    assert_eq!(body["data"]["agent_id"], "opencode");
    let config_after = std::fs::read_to_string(&harness.config_path).expect("config after");
    assert_eq!(config_after, config_before, "no-op must not rewrite config");
}

#[tokio::test]
async fn agent_switch_conflicts_when_resumed_candidate_differs() {
    let tempdir = TempDir::new().expect("tempdir");
    let shim_path = tempdir.path().join("kimi-shim");
    let fixture_path = write_config_options_fixture(tempdir.path(), &["kimi/kimi-k3"]);
    let _fixture_guard = EnvVarGuard::set("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &fixture_path);
    set_kimi_secret(&tempdir);
    write_placebo_shim(&shim_path);
    let harness = spawn_kimi_switch_fixture(&tempdir, &shim_path).await;

    // The operator edited config between attempts, so the journaled candidate
    // no longer matches what the retry would commit.
    persist_switch_journal(
        &harness.config_path,
        &SwitchJournal {
            old_target_id: "opencode".to_owned(),
            new_target_id: "kimi".to_owned(),
            target_agent_id: "kimi".to_owned(),
            candidate_fingerprint: "00".repeat(32),
            was_running: false,
            phase: SwitchJournalPhase::Planned,
        },
    )
    .expect("seed journal");

    let (status, body) = switch_request(&harness, "kimi").await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {body}");
    assert_eq!(body["error"]["code"], "agent.switch_conflict");
}

#[tokio::test]
async fn agent_switch_while_stopped_completes_and_same_target_retry_is_noop() {
    let tempdir = TempDir::new().expect("tempdir");
    let shim_path = tempdir.path().join("kimi-shim");
    let fixture_path = write_config_options_fixture(tempdir.path(), &["kimi/kimi-k3"]);
    let _fixture_guard = EnvVarGuard::set("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &fixture_path);
    set_kimi_secret(&tempdir);
    write_placebo_shim(&shim_path);
    let harness = spawn_kimi_switch_fixture(&tempdir, &shim_path).await;

    let (status, body) = switch_request(&harness, "kimi").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["restarted"], false);
    assert_eq!(body["data"]["restart_started"], false);
    assert_eq!(journal_phase(&harness), SwitchJournalPhase::Completed);
    let committed = std::fs::read(&harness.config_path).expect("read config");

    let (status, body) = switch_request(&harness, "kimi").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["agent_id"], "kimi");
    assert_eq!(body["data"]["provider_status"], "no_op");
    assert_eq!(body["data"]["restarted"], false);
    assert_eq!(body["data"]["restart_started"], false);
    assert!(body["data"].get("install").is_none());
    assert_eq!(
        std::fs::read(&harness.config_path).expect("read config"),
        committed
    );
    assert_eq!(
        target_field(&harness, "kimi", "process_state").await,
        "stopped"
    );
}

#[tokio::test]
async fn agent_switch_completed_retry_leaves_running_agent_untouched() {
    let tempdir = TempDir::new().expect("tempdir");
    let shim_path = tempdir.path().join("kimi-shim");
    let fixture_path = write_config_options_fixture(tempdir.path(), &["kimi/kimi-k3"]);
    let _fixture_guard = EnvVarGuard::set("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &fixture_path);
    set_kimi_secret(&tempdir);
    write_placebo_shim(&shim_path);
    let harness = spawn_kimi_switch_fixture(&tempdir, &shim_path).await;
    start_primary(&harness).await;

    let (status, body) = switch_request(&harness, "kimi").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["restarted"], true);
    let pid_before = target_field(&harness, "kimi", "pid").await;
    assert!(
        pid_before.is_number(),
        "kimi should be running: {pid_before}"
    );

    let (status, body) = switch_request(&harness, "kimi").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["provider_status"], "no_op");
    assert_eq!(body["data"]["restarted"], false);
    assert_eq!(body["data"]["restart_started"], false);
    let pid_after = target_field(&harness, "kimi", "pid").await;
    assert_eq!(
        pid_before, pid_after,
        "no-op retry must not recycle the agent"
    );
}

#[tokio::test]
async fn agent_switch_corrupt_journal_is_a_hard_error() {
    let tempdir = TempDir::new().expect("tempdir");
    let harness =
        AgentHarness::spawn_with_config_and_home(test_config(), tempdir.path().to_path_buf()).await;
    let journal_path = switch_journal_path(&harness.config_path).expect("journal path");
    std::fs::write(&journal_path, b"{not json").expect("write corrupt journal");

    let (status, body) = switch_request(&harness, "kimi").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "body: {body}");
    assert_eq!(body["error"]["code"], "agent.switch_journal_corrupt");
}

#[tokio::test]
async fn agent_switch_without_journal_converges_same_target_as_noop() {
    let tempdir = TempDir::new().expect("tempdir");
    let shim_path = tempdir.path().join("kimi-shim");
    let fixture_path = write_config_options_fixture(tempdir.path(), &["kimi/kimi-k3"]);
    let _fixture_guard = EnvVarGuard::set("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &fixture_path);
    set_kimi_secret(&tempdir);
    write_placebo_shim(&shim_path);
    let harness = spawn_kimi_switch_fixture(&tempdir, &shim_path).await;

    let (status, body) = switch_request(&harness, "kimi").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");

    // With no journal at all, a bare same-target retry must still converge as
    // a side-effect-free no-op.
    let journal_path = switch_journal_path(&harness.config_path).expect("journal path");
    std::fs::remove_file(&journal_path).expect("remove journal");
    let config_before = std::fs::read_to_string(&harness.config_path).expect("config before");
    let (status, body) = switch_request(&harness, "kimi").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["provider_status"], "no_op");
    assert_eq!(body["data"]["restarted"], false);
    let config_after = std::fs::read_to_string(&harness.config_path).expect("config after");
    assert_eq!(config_after, config_before, "no-op must not rewrite config");
    assert!(!journal_path.exists(), "no-op must not journal a switch");
}
