use acp_stack::state::{
    INSTALLER_METHOD_GITHUB, INSTALLER_METHOD_NPM, INSTALLER_OPERATION_INSTALL, InstallerRunInput,
    StateStore, default_state_path,
};
use predicates::prelude::PredicateBooleanExt as _;
use serde_json::Value;
use std::fs;

use crate::common::cli::*;
use crate::support::*;

#[test]
fn agent_status_surfaces_installed_versions_from_state() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    // Seed installer_runs rows so `acps agent status` surfaces the versions.
    // The latest-successful query buckets by `step`, so a 'harness' row with
    // a recorded version and an 'adapter' row without a version exercise both
    // the "show version" and "version unknown" branches of the surface.
    let state_path = default_state_path(tempdir.path());
    fs::create_dir_all(state_path.parent().expect("state parent dir"))
        .expect("state dir should be created");
    let store = StateStore::open(&state_path).expect("state should open");
    store.migrate().expect("migration should pass");
    store
        .upsert_agent_capabilities(
            "opencode",
            r#"{"protocol_version":1,"capabilities":{"loadSession":true},"agent_name":"opencode","agent_title":"OpenCode","agent_version":"1.15.10"}"#,
        )
        .expect("capability row should append");
    store
        .append_installer_run(InstallerRunInput {
            agent_id: "opencode",
            started_at: "2026-05-21T00:00:00.000000000Z",
            finished_at: Some("2026-05-21T00:00:01.000000000Z"),
            status: "ran",
            stdout: "",
            stderr: "",
            exit_status: Some(0),
            step: "install",
            version: Some("1.15.10"),
            operation: INSTALLER_OPERATION_INSTALL,
            method: Some(INSTALLER_METHOD_NPM),
            log_dir: None,
            apply_run_id: None,
        })
        .expect("install row should append");
    store
        .append_installer_run(InstallerRunInput {
            agent_id: "opencode",
            started_at: "2026-05-21T00:00:02.000000000Z",
            finished_at: Some("2026-05-21T00:00:03.000000000Z"),
            status: "ran",
            stdout: "",
            stderr: "",
            exit_status: Some(0),
            step: "harness",
            version: Some("v1.2.3"),
            operation: INSTALLER_OPERATION_INSTALL,
            method: Some(INSTALLER_METHOD_GITHUB),
            log_dir: None,
            apply_run_id: None,
        })
        .expect("harness row should append");
    store
        .append_installer_run(InstallerRunInput {
            agent_id: "opencode",
            started_at: "2026-05-21T00:00:04.000000000Z",
            finished_at: Some("2026-05-21T00:00:05.000000000Z"),
            status: "ran",
            stdout: "",
            stderr: "",
            exit_status: Some(0),
            step: "adapter",
            version: None,
            operation: INSTALLER_OPERATION_INSTALL,
            method: Some(INSTALLER_METHOD_GITHUB),
            log_dir: None,
            apply_run_id: None,
        })
        .expect("adapter row should append");
    drop(store);

    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "status"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent version: 1.15.10"))
        .stdout(predicates::str::contains("harness version: v1.2.3"))
        .stdout(predicates::str::contains(
            "adapter version: version unknown",
        ))
        .stdout(predicates::str::contains("ACP version: 1"));
}

#[test]
fn agent_status_format_json_omits_lifecycle_payloads() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    let state_path = default_state_path(tempdir.path());
    fs::create_dir_all(state_path.parent().expect("state parent dir"))
        .expect("state dir should be created");
    let store = StateStore::open(&state_path).expect("state should open");
    store.migrate().expect("migration should pass");
    store
        .append_agent_lifecycle(
            "agent.failed",
            "agent failed",
            r#"{"reason":"token sk-test-secret"}"#,
        )
        .expect("lifecycle row should append");
    drop(store);

    let output = acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "status", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let body: Value = serde_json::from_slice(&output).expect("agent status json parses");
    let lifecycle = body["recent_lifecycle"]
        .as_array()
        .expect("recent_lifecycle is an array");
    assert_eq!(lifecycle.len(), 1, "{body}");
    assert!(lifecycle[0].get("payload").is_none(), "{body}");
    assert!(!String::from_utf8_lossy(&output).contains("sk-test-secret"));
}

#[test]
fn agent_test_succeeds_with_prompt() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    write_fake_agent_home(tempdir.path(), &[]);

    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "test", "--prompt", "hello from cli"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent test: ok"))
        .stdout(predicates::str::contains("agent: placebo"))
        .stdout(predicates::str::contains("prompt: provided"))
        .stdout(predicates::str::contains("session_id: sess_fake_0"))
        .stdout(predicates::str::contains("stop_reason: end_turn"))
        .stdout(predicates::str::contains("updates: 2"));
}

#[test]
fn agent_test_uses_default_prompt_when_omitted() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    write_fake_agent_home(tempdir.path(), &[]);

    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "test"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent test: ok"))
        .stdout(predicates::str::contains("agent: placebo"))
        .stdout(predicates::str::contains("prompt: default"))
        .stdout(predicates::str::contains("stop_reason: end_turn"));
}

#[test]
fn agent_test_applies_configured_model_before_prompt() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    write_fake_agent_home(
        tempdir.path(),
        &[
            "--model-config-option",
            "openai/gpt-5.5",
            "--expect-model-config",
            "openai/gpt-5.5",
        ],
    );
    let config_path = tempdir.path().join(".config/acp-stack/acps-config.toml");
    let config = fs::read_to_string(&config_path).expect("config should be readable");
    fs::write(
        &config_path,
        config.replace(
            r#"restart = "on-crash""#,
            "restart = \"on-crash\"\nmodel = \"openai/gpt-5.5\"",
        ),
    )
    .expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "test", "--prompt", "hello"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent test: ok"));
}

#[test]
fn agent_test_reports_initialize_failure_stage() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    write_fake_agent_home(tempdir.path(), &["--initialize-error"]);

    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "test", "--prompt", "hello"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "agent test failed at ACP initialize",
        ))
        .stderr(predicates::str::contains("fake initialize failure"));
}

#[test]
fn agent_test_reports_session_creation_failure_stage() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    write_fake_agent_home(tempdir.path(), &["--session-new-error"]);

    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "test", "--prompt", "hello"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "agent test failed at session creation",
        ))
        .stderr(predicates::str::contains("fake session/new failure"));
}

#[test]
fn agent_test_reports_prompt_failure_stage() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    write_fake_agent_home(tempdir.path(), &["--prompt-error"]);

    // Phase 2 sanitization: the prompt-failure path now drops the raw upstream
    // message (which could embed URLs, headers, or secrets) and surfaces a
    // fixed `"prompt request failed"` string instead. Assert on the sanitized
    // form rather than the agent-supplied text.
    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "test", "--prompt", "hello"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "agent test failed at prompt completion",
        ))
        .stderr(predicates::str::contains("prompt request failed"));
}

#[test]
fn agent_test_reports_progress_timeout_after_stall() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    write_fake_agent_home(tempdir.path(), &["--prompt-stall-after-update"]);

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "agent",
            "test",
            "--prompt",
            "hello",
            "--progress-timeout",
            "50ms",
            "--timeout",
            "2s",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "agent test failed at prompt/progress timeout",
        ))
        .stderr(predicates::str::contains(
            "no new session/update or terminal prompt response within 50ms",
        ));
}

#[test]
fn agent_status_reports_provider_with_unset_model_and_mode() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = format!("{}\n[agent.provider]\nid = \"openai\"\n", codex_config());
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "status"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent: codex"))
        .stdout(predicates::str::contains("provider: openai"))
        .stdout(predicates::str::contains("model and mode unset"))
        .stdout(predicates::str::contains("unavailable").not());
}

#[test]
fn agent_status_reports_all_configured_params() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = VALID_CONFIG.replace(
        r#"restart = "on-crash""#,
        r#"restart = "on-crash"
mode = "build"

[agent.provider]
id = "opencode-go"
model = "deepseek-v4-pro"
api_key_ref = "OPENCODE_API_KEY""#,
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "status"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent: opencode"))
        .stdout(predicates::str::contains("provider: opencode-go"))
        .stdout(predicates::str::contains("model: deepseek-v4-pro"))
        .stdout(predicates::str::contains("mode: build"))
        .stdout(predicates::str::contains(" unset").not())
        .stdout(predicates::str::contains(" unavailable").not());
}

#[test]
fn agent_status_reports_model_only_agent_params() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = VALID_CONFIG
        .replace(r#"id = "opencode""#, r#"id = "kimi""#)
        .replace(r#"name = "OpenCode""#, r#"name = "Kimi Code""#)
        .replace(r#"command = "opencode""#, r#"command = "kimi""#)
        .replace(r#"env = ["OPENCODE_API_KEY"]"#, r#"env = ["KIMI_API_KEY"]"#)
        .replace(
            r#"restart = "on-crash""#,
            r#"restart = "on-crash"
model = "gpt-5.5""#,
        )
        .replace(
            r#"
[agent.install]
type = "shell"
shell = "curl -fsSL https://opencode.ai/install | bash"
creates = "opencode"
"#,
            "",
        );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "status"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent: kimi"))
        .stdout(predicates::str::contains("model: gpt-5.5"))
        .stdout(predicates::str::contains("mode unset"))
        .stdout(predicates::str::contains("provider unavailable"));
}

#[test]
fn agent_status_reports_amp_unavailable_provider_and_model() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = VALID_CONFIG
        .replace(r#"id = "opencode""#, r#"id = "amp""#)
        .replace(r#"name = "OpenCode""#, r#"name = "Amp Code""#)
        .replace(r#"command = "opencode""#, r#"command = "amp-acp""#)
        .replace(r#"args = ["acp"]"#, r#"args = []"#)
        .replace(r#"env = ["OPENCODE_API_KEY"]"#, r#"env = ["AMP_API_KEY"]"#)
        .replace(
            r#"restart = "on-crash""#,
            r#"restart = "on-crash"
mode = "smart""#,
        )
        .replace(
            r#"
[agent.install]
type = "shell"
shell = "curl -fsSL https://opencode.ai/install | bash"
creates = "opencode"
"#,
            "",
        );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "status"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent: amp"))
        .stdout(predicates::str::contains("mode: smart"))
        .stdout(predicates::str::contains("provider and model unavailable"));
}

#[test]
fn agent_status_reports_all_supported_params_unset() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "status"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent: opencode"))
        .stdout(predicates::str::contains("provider, model, and mode unset"))
        .stdout(predicates::str::contains("unavailable").not());
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_start_and_stop_call_running_daemon() {
    let harness = AgentCliHarness::spawn().await;
    let home = tempfile::tempdir().expect("tempdir should be created");
    write_cli_home_with_socket(
        home.path(),
        &harness.base_url,
        ADMIN_KEY,
        Some(&harness.socket_path),
    );

    acps_command()
        .env("HOME", home.path())
        .args(["agent", "start", "--admin-key", ADMIN_KEY])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent start: running"))
        .stdout(predicates::str::contains("pid: "));

    let output = acps_command()
        .env("HOME", home.path())
        .args([
            "agent",
            "restart",
            "--format",
            "json",
            "--admin-key",
            ADMIN_KEY,
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let body: Value = serde_json::from_slice(&output).expect("restart json parses");
    assert!(body["started_at"].as_str().is_some(), "{body}");
    assert!(body["stopped_at"].as_str().is_some(), "{body}");
    assert!(body["capabilities"].is_object(), "{body}");

    acps_command()
        .env("HOME", home.path())
        .args(["agent", "stop", "--admin-key", ADMIN_KEY])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent stop: stopped"));
}

#[test]
fn agent_switch_noninteractive_requires_admin_key() {
    acps_command()
        .args(["agent", "switch", "opencode"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--admin-key"));
}

#[test]
fn agent_switch_accepts_drop_flag() {
    acps_command()
        .args(["agent", "switch", "opencode", "--drop"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--admin-key"))
        .stderr(predicates::str::contains("unexpected argument").not());
}
