use acp_stack::state::{EVENT_SOURCE_CLI, StateStore, default_state_path};
use http::StatusCode;
use serde_json::Value;
use std::fs;

use crate::common::cli::*;
use crate::support::*;

#[cfg(unix)]
#[test]
fn init_creates_owner_only_config_and_state_paths() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success();

    let config_dir = tempdir.path().join(".config/acp-stack");
    let state_dir = tempdir.path().join(".local/share/acp-stack");
    let config_path = config_dir.join("acps-config.toml");
    let state_path = state_dir.join("state.sqlite");

    assert_eq!(mode(&config_dir), 0o700);
    assert_eq!(mode(&state_dir), 0o700);
    assert_eq!(mode(&config_path), 0o600);
    assert_eq!(mode(&state_path), 0o600);
}

#[test]
fn init_does_not_overwrite_existing_config() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config_path = config_dir.join("acps-config.toml");
    fs::write(&config_path, VALID_PLACEBO_CONFIG).expect("config should be written");

    let mut command = acps_command();

    command
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--skip-workspace-init"])
        .assert()
        .success()
        .stdout(predicates::str::contains("validated existing config"));

    let config = fs::read_to_string(config_path).expect("config should be readable");
    assert_eq!(config, VALID_PLACEBO_CONFIG);
}

#[test]
fn init_fails_when_existing_config_is_invalid() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(
        config_dir.join("acps-config.toml"),
        VALID_CONFIG.replace(r#"bind = "127.0.0.1:7700""#, r#"bind = "bad""#),
    )
    .expect("invalid config should be written");

    let mut command = acps_command();

    command
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "api.bind must be a socket address",
        ));
}

#[test]
fn status_reports_config_state_workspace_agent_sink_and_deps() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    // Use a tempdir workspace so the test is deterministic across hosts.
    // Without this, `acps init` would pick the production default
    // `/workspace`, which is writable inside Docker dev images and the
    // Railway runtime but absent on the maintainer's macOS host. Pinning
    // workspace.root to a controlled tempdir keeps the assertion below
    // valid in both environments.
    let workspace_dir = tempdir.path().join("workspace");
    std::fs::create_dir_all(&workspace_dir).expect("workspace dir should be created");
    let uploads_dir = workspace_dir.join("uploads");
    std::fs::create_dir_all(&uploads_dir).expect("uploads dir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .arg("--workspace-root")
        .arg(&workspace_dir)
        .arg("--workspace-uploads")
        .arg(&uploads_dir)
        .assert()
        .success();

    let workspace_str = workspace_dir.display().to_string();
    let mut command = acps_command();
    command
        .env("HOME", tempdir.path())
        .arg("status")
        .assert()
        .success()
        .stdout(predicates::str::contains("config:    ok ("))
        .stdout(predicates::str::contains("state:     ok ("))
        .stdout(predicates::str::contains("schema=25"))
        .stdout(predicates::str::contains("latest_event="))
        .stdout(predicates::str::contains(format!(
            "workspace: ok ({workspace_str})"
        )))
        .stdout(predicates::str::contains("agent:"))
        .stdout(predicates::str::contains("sink:      supabase disabled"))
        .stdout(predicates::str::contains("deps:      no apply runs"))
        .stdout(predicates::str::contains("daemon:   unavailable"));
}

#[test]
fn status_format_json_reports_same_top_level_sections() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let workspace_dir = tempdir.path().join("workspace");
    std::fs::create_dir_all(&workspace_dir).expect("workspace dir should be created");
    let uploads_dir = workspace_dir.join("uploads");
    std::fs::create_dir_all(&uploads_dir).expect("uploads dir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .arg("--workspace-root")
        .arg(&workspace_dir)
        .arg("--workspace-uploads")
        .arg(&uploads_dir)
        .assert()
        .success();

    let output = acps_command()
        .env("HOME", tempdir.path())
        .args(["status", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let body: Value = serde_json::from_slice(&output).expect("status json parses");
    assert_eq!(body["config"]["ok"], true);
    assert_eq!(body["workspace"]["ok"], true);
    assert_eq!(
        body["workspace"]["root"],
        workspace_dir.display().to_string()
    );
    assert!(body["state"]["schema_version"].as_i64().is_some(), "{body}");
    assert_eq!(body["daemon"]["status"], "unavailable");
}

#[test]
fn status_reports_sink_open_failures_when_supabase_configured() {
    use chrono::{SecondsFormat, Utc};

    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = VALID_CONFIG.replace("enabled = false", "enabled = true");
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    let state_path = default_state_path(tempdir.path());
    fs::create_dir_all(state_path.parent().expect("state parent dir"))
        .expect("state dir should be created");
    let mut store = StateStore::open(&state_path).expect("state should open");
    store.migrate().expect("migration should pass");
    store.set_external_logging_enabled(true);
    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
    store
        .append_event_with_source(
            "info",
            "test.seed",
            EVENT_SOURCE_CLI,
            "seed sink_outbox row",
            "{}",
        )
        .expect("append seed event");
    let batch = store
        .next_sink_outbox_batch(10, &now)
        .expect("read outbox batch");
    let ids: Vec<String> = batch.iter().map(|row| row.id.clone()).collect();
    assert!(
        !ids.is_empty(),
        "seed event should have enqueued an outbox row"
    );
    store
        .mark_sink_outbox_failure(&ids, "boom", &now, &now)
        .expect("mark outbox failure");
    drop(store);

    acps_command()
        .env("HOME", tempdir.path())
        .arg("status")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "sink:      1 open failures (supabase",
        ));
}

#[tokio::test(flavor = "multi_thread")]
async fn status_reports_ready_daemon_when_health_probe_is_healthy() {
    let probe = HealthProbeHarness::spawn(
        StatusCode::OK,
        serde_json::json!({
            "ok": true,
            "data": {
                "ok": true,
                "failing": []
            }
        }),
    )
    .await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home_with_socket(
        home.path(),
        "http://127.0.0.1:9",
        ADMIN_KEY,
        Some(&probe.socket_path),
    );

    acps_command()
        .env("HOME", home.path())
        .arg("status")
        .assert()
        .success()
        .stdout(predicates::str::contains("daemon:   ready"));
}

#[tokio::test(flavor = "multi_thread")]
async fn status_reports_degraded_daemon_without_failing_command() {
    let probe = HealthProbeHarness::spawn(
        StatusCode::SERVICE_UNAVAILABLE,
        serde_json::json!({
            "ok": false,
            "data": {
                "ok": false,
                "failing": ["sink", "deps"]
            }
        }),
    )
    .await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home_with_socket(
        home.path(),
        "http://127.0.0.1:9",
        ADMIN_KEY,
        Some(&probe.socket_path),
    );

    acps_command()
        .env("HOME", home.path())
        .arg("status")
        .assert()
        .success()
        .stdout(predicates::str::contains("daemon:   degraded (sink, deps)"));
}

#[tokio::test(flavor = "multi_thread")]
async fn status_reports_unavailable_daemon_without_failing_command() {
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home(home.path(), "http://127.0.0.1:9", ADMIN_KEY);

    acps_command()
        .env("HOME", home.path())
        .arg("status")
        .assert()
        .success()
        .stdout(predicates::str::contains("daemon:   unavailable"));
}
