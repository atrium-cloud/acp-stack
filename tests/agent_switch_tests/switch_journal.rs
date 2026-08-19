//! Coverage for the pending-switch journal: a failure at or after the
//! switch's commit boundary (session rename + canonical config write) used to
//! leave the new primary on disk with its agent stopped, and the retry was
//! rejected as "already configured". The journal makes the same-target retry
//! converge and the different-target retry a conflict.
//!
//! Failure injection: the switch target's command is a gated shim. The shim
//! file exists from the start so the installer's resolve-and-spawn gate
//! passes, but it exits 1 until a marker file appears, so the post-commit
//! agent start fails. Creating the marker fixes the retry without touching
//! the committed config — editing the config would change the journaled
//! candidate fingerprint and legitimately conflict.

use reqwest::StatusCode;
use serde_json::{Value, json};
use tempfile::TempDir;

use acp_stack::config::ArrayTargetConfig;
use acp_stack::runtime::agent::switch_journal::{
    SwitchJournal, SwitchJournalPhase, load_switch_journal, persist_switch_journal,
    switch_journal_path,
};
use acp_stack::state::NewSessionRecord;

use crate::common::HomeEnvGuard;
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
        .json(&json!({ "agent": agent }))
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
/// override whose harness command is `shim_path`. The kimi model discovery is
/// stubbed through the config-options fixture.
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
    AgentHarness::spawn_with_config(config).await
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
    let _home = HomeEnvGuard::set(tempdir.path());
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
    // The commit boundary was crossed: the new primary is on disk and the
    // journal holds the pre-commit `was_running` snapshot.
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

    // Once the launch is fixed, the same-target retry resumes after the
    // commit boundary and converges instead of failing "already configured".
    std::fs::write(&marker_path, b"ready\n").expect("write marker");
    let (status, body) = switch_request(&harness, "kimi").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["agent_id"], "kimi");
    assert_eq!(body["data"]["provider_status"], "resumed");
    assert_eq!(body["data"]["restarted"], true);
    assert_eq!(body["data"]["restart_started"], true);
    // Pre-commit steps (install, provisioning, model discovery) are not
    // re-run on a post-commit resume.
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
    let _home = HomeEnvGuard::set(tempdir.path());
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

    // Simulate a daemon restart between the failed attempt and the retry: the
    // journaled `was_running` is the only surviving record that the old
    // primary was up.
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
    let _home = HomeEnvGuard::set(tempdir.path());
    let config_dir = tempdir.path().join(".config/acp-stack");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    write_kimi_registry_override(&config_dir);
    set_kimi_secret(&tempdir);
    let shim_path = tempdir.path().join("kimi-shim");
    let mut config = test_config();
    config.array.enabled = true;
    // The existing-array-target path takes the runtime command from the
    // target's stored agent config, so the missing binary lives there. The
    // escape-hatch install recipe does not spawn-gate the command, so the
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
    let harness = AgentHarness::spawn_with_config(config).await;
    start_primary(&harness).await;

    let (status, body) = switch_request(&harness, "kimi").await;
    assert!(status.is_server_error(), "body: {body}");
    let journal = load_switch_journal(&harness.config_path)
        .expect("journal load")
        .expect("journal present");
    assert_eq!(journal.phase, SwitchJournalPhase::Committed);

    // Recreate the state a failed old-agent shutdown would leave behind: the
    // old target's process is still up even though the new primary is
    // committed. (The bridge teardown escalates to a process-group SIGKILL
    // and never surfaces an error, so a genuine shutdown failure cannot be
    // driven through the placebo fixture.)
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
    let _home = HomeEnvGuard::set(tempdir.path());
    let shim_path = tempdir.path().join("kimi-shim");
    let fixture_path = write_config_options_fixture(tempdir.path(), &["kimi/kimi-k3"]);
    let _fixture_guard = EnvVarGuard::set("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &fixture_path);
    set_kimi_secret(&tempdir);
    write_placebo_shim(&shim_path);
    let harness = spawn_kimi_switch_fixture(&tempdir, &shim_path).await;

    // Seed the collision the rename check rejects: a session under the current
    // primary and one under the switch's future target id sharing the same
    // agent_session_id.
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
    // Nothing durable changed: the config still names the old primary, and the
    // Planned journal the attempt persisted is gone instead of stranding an
    // in-progress record.
    let on_disk = std::fs::read_to_string(&harness.config_path).expect("read config");
    assert!(!on_disk.contains(r#"id = "kimi""#), "config: {on_disk}");
    assert_eq!(
        load_switch_journal(&harness.config_path).expect("journal load"),
        None
    );

    // With the collision resolved, the retry runs as a fresh switch — a stale
    // journal would have reproduced the collision or 409'd another target.
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
    let _home = HomeEnvGuard::set(tempdir.path());
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

    // A different target must not silently abandon the in-flight switch.
    let (status, body) = switch_request(&harness, "amp").await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {body}");
    assert_eq!(body["error"]["code"], "agent.switch_conflict");

    // The conflict did not disturb the in-flight switch: it still resumes.
    std::fs::write(&marker_path, b"ready\n").expect("write marker");
    let (status, body) = switch_request(&harness, "kimi").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["provider_status"], "resumed");
    assert_eq!(journal_phase(&harness), SwitchJournalPhase::Completed);
}

#[tokio::test]
async fn agent_switch_conflicts_when_resumed_candidate_differs() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    let shim_path = tempdir.path().join("kimi-shim");
    let fixture_path = write_config_options_fixture(tempdir.path(), &["kimi/kimi-k3"]);
    let _fixture_guard = EnvVarGuard::set("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &fixture_path);
    set_kimi_secret(&tempdir);
    write_placebo_shim(&shim_path);
    let harness = spawn_kimi_switch_fixture(&tempdir, &shim_path).await;

    // A journaled in-flight switch whose recorded candidate no longer matches
    // what the retry would commit: the operator edited config between
    // attempts, so the retry must conflict rather than converge on a
    // different switch.
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
    let _home = HomeEnvGuard::set(tempdir.path());
    let shim_path = tempdir.path().join("kimi-shim");
    let fixture_path = write_config_options_fixture(tempdir.path(), &["kimi/kimi-k3"]);
    let _fixture_guard = EnvVarGuard::set("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &fixture_path);
    set_kimi_secret(&tempdir);
    write_placebo_shim(&shim_path);
    let harness = spawn_kimi_switch_fixture(&tempdir, &shim_path).await;
    // The primary was never started: the switch must not start the target.

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
    // A no-op retry rewrites nothing and starts nothing.
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
    let _home = HomeEnvGuard::set(tempdir.path());
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
    let _home = HomeEnvGuard::set(tempdir.path());
    let harness = AgentHarness::spawn().await;
    let journal_path = switch_journal_path(&harness.config_path).expect("journal path");
    std::fs::write(&journal_path, b"{not json").expect("write corrupt journal");

    let (status, body) = switch_request(&harness, "kimi").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "body: {body}");
    assert_eq!(body["error"]["code"], "agent.switch_journal_corrupt");
}

#[tokio::test]
async fn agent_switch_without_journal_keeps_already_configured_error() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    let shim_path = tempdir.path().join("kimi-shim");
    let fixture_path = write_config_options_fixture(tempdir.path(), &["kimi/kimi-k3"]);
    let _fixture_guard = EnvVarGuard::set("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &fixture_path);
    set_kimi_secret(&tempdir);
    write_placebo_shim(&shim_path);
    let harness = spawn_kimi_switch_fixture(&tempdir, &shim_path).await;

    let (status, body) = switch_request(&harness, "kimi").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");

    // A daemon whose journal is absent (e.g. written before the journal
    // existed) must keep the pre-journal rejection for a same-target retry.
    let journal_path = switch_journal_path(&harness.config_path).expect("journal path");
    std::fs::remove_file(&journal_path).expect("remove journal");
    let (status, body) = switch_request(&harness, "kimi").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert_eq!(body["error"]["code"], "request.invalid_param");
}
