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

    // A `harness` row with a version and an `adapter` row without one exercise both the
    // "show version" and "version unknown" branches.
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

    acps_command(tempdir.path())
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

    let output = acps_command(tempdir.path())
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

    acps_command(tempdir.path())
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

    acps_command(tempdir.path())
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

    acps_command(tempdir.path())
        .args(["agent", "test", "--prompt", "hello"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent test: ok"));
}

/// Advertise a `thought_level` select that omits `high`, so an ACP effort set for `high` can only
/// fail. A run that still passes proves the set was skipped.
const EFFORT_OPTION_WITHOUT_HIGH: &str = "reasoning_effort@thought_level=low:low,medium";
const EFFORT_OPTION_WITH_HIGH: &str = "reasoning_effort@thought_level=low:low,high";

/// Append `[agent]` keys to the fake-agent config `write_fake_agent_home` wrote. `[agent]` is the
/// last table in the fixture, so appended keys land in it.
fn append_agent_config(home: &std::path::Path, lines: &str) {
    let config_path = home.join(".config/acp-stack/acps-config.toml");
    let mut config = fs::read_to_string(&config_path).expect("config should be readable");
    config.push_str(lines);
    fs::write(&config_path, config).expect("config should be written");
}

#[test]
fn agent_test_skips_the_effort_config_option_when_the_effort_is_pinned_on_disk() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    write_fake_agent_home(
        tempdir.path(),
        &["--config-option-select", EFFORT_OPTION_WITHOUT_HIGH],
    );
    seed_provider_credential(tempdir.path(), "openrouter", &["OPENROUTER_API_KEY"]);
    let config_path = tempdir.path().join(".config/acp-stack/acps-config.toml");
    let config = fs::read_to_string(&config_path).expect("config should be readable");
    fs::write(
        &config_path,
        config.replace(r#"id = "placebo""#, r#"id = "codex""#),
    )
    .expect("config should be written");
    append_agent_config(
        tempdir.path(),
        "effort = \"high\"\nprovider = { id = \"openrouter\" }\n",
    );

    acps_command(tempdir.path())
        .args(["agent", "test", "--prompt", "hello"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent test: ok"));
}

#[test]
fn agent_test_fails_when_an_unpinned_effort_is_not_advertised() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    write_fake_agent_home(
        tempdir.path(),
        &["--config-option-select", EFFORT_OPTION_WITHOUT_HIGH],
    );
    append_agent_config(tempdir.path(), "effort = \"high\"\n");

    acps_command(tempdir.path())
        .args(["agent", "test", "--prompt", "hello"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "agent test failed at session creation",
        ))
        .stderr(predicates::str::contains("high"));
}

#[test]
fn agent_test_applies_an_advertised_effort_before_prompt() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    write_fake_agent_home(
        tempdir.path(),
        &[
            "--config-option-select",
            EFFORT_OPTION_WITH_HIGH,
            "--expect-config-option",
            "reasoning_effort=high",
        ],
    );
    append_agent_config(tempdir.path(), "effort = \"high\"\n");

    acps_command(tempdir.path())
        .args(["agent", "test", "--prompt", "hello"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent test: ok"));
}

#[test]
fn agent_test_reports_initialize_failure_stage() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    write_fake_agent_home(tempdir.path(), &["--initialize-error"]);

    acps_command(tempdir.path())
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

    acps_command(tempdir.path())
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

    // The prompt-failure path drops the raw upstream message, which could embed URLs, headers, or
    // secrets, in favor of a fixed string.
    acps_command(tempdir.path())
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

    acps_command(tempdir.path())
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

/// Run `agent test --format json` and return the parsed stdout document. `expect_success` pins the
/// exit status: a failing run still prints its document to stdout.
fn agent_test_json(home: &std::path::Path, extra_args: &[&str], expect_success: bool) -> Value {
    let mut command = acps_command(home);
    command
        .args(["agent", "test", "--format", "json"])
        .args(extra_args);
    let assert = command.assert();
    let assert = if expect_success {
        assert.success()
    } else {
        assert.failure()
    };
    let stdout = assert.get_output().stdout.clone();
    serde_json::from_slice(&stdout).unwrap_or_else(|error| {
        panic!(
            "agent test json must parse: {error}\nstdout: {}",
            String::from_utf8_lossy(&stdout)
        )
    })
}

fn json_keys(value: &Value) -> Vec<String> {
    let mut keys: Vec<String> = value
        .as_object()
        .expect("json object")
        .keys()
        .cloned()
        .collect();
    keys.sort();
    keys
}

#[test]
fn agent_test_json_success_document_has_the_full_schema() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    write_fake_agent_home(tempdir.path(), &[]);

    let document = agent_test_json(tempdir.path(), &["--prompt", "hello from cli"], true);

    assert_eq!(
        json_keys(&document),
        [
            "agent",
            "cleanup",
            "code",
            "elapsed_ms",
            "evidence",
            "fs_check",
            "ok",
            "phase",
            "prompt_source",
            "schema_version",
            "stop_reason",
            "updates",
        ]
    );
    assert_eq!(document["schema_version"], 1);
    assert_eq!(document["ok"], true);
    assert_eq!(document["phase"], "done");
    assert_eq!(document["code"], "ok");
    assert_eq!(document["agent"], "placebo");
    assert_eq!(document["prompt_source"], "provided");
    assert_eq!(document["stop_reason"], "end_turn");
    assert_eq!(document["updates"], 2);
    assert!(document["elapsed_ms"].is_u64());
    assert_eq!(
        json_keys(&document["evidence"]),
        [
            "final_assistant_text",
            "message_chunks",
            "text_truncated",
            "thought_chunks",
            "tool_call_updates",
            "tool_calls",
        ]
    );
    assert_eq!(
        document["evidence"]["final_assistant_text"],
        "chunk-1chunk-2"
    );
    assert_eq!(document["evidence"]["text_truncated"], false);
    assert_eq!(document["evidence"]["message_chunks"], 2);
    assert_eq!(document["evidence"]["thought_chunks"], 0);
    assert_eq!(document["evidence"]["tool_calls"], 0);
    assert_eq!(document["evidence"]["tool_call_updates"], 0);
    assert_eq!(json_keys(&document["fs_check"]), ["bytes", "status"]);
    assert_eq!(document["fs_check"]["status"], "skipped");
    assert_eq!(document["fs_check"]["bytes"], Value::Null);
    assert_eq!(
        json_keys(&document["cleanup"]),
        ["process", "session_delete"]
    );
    assert_eq!(document["cleanup"]["session_delete"], "deleted");
    assert_eq!(document["cleanup"]["process"], "terminated");
}

#[test]
fn agent_test_json_document_leaks_no_prompt_path_or_secret() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    write_fake_agent_home(tempdir.path(), &[]);

    let document = agent_test_json(
        tempdir.path(),
        &["--prompt", "unique-prompt-marker-42"],
        true,
    );
    let raw = document.to_string();

    assert!(!raw.contains("unique-prompt-marker-42"), "{raw}");
    assert!(!raw.contains("sess_fake_0"), "{raw}");
    assert!(
        !raw.contains(&tempdir.path().to_string_lossy().to_string()),
        "{raw}"
    );
    assert!(!raw.contains("workspace"), "{raw}");
    assert!(!raw.contains("sk-test-secret"), "{raw}");
    // Reason strings embed argv and workspace paths, so codes are the only machine channel.
    assert!(!raw.contains("\"reason\""), "{raw}");
}

#[test]
fn agent_test_json_failure_document_reports_phase_and_code() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    write_fake_agent_home(tempdir.path(), &["--session-new-error"]);

    let document = agent_test_json(tempdir.path(), &["--prompt", "hello"], false);

    assert_eq!(document["ok"], false);
    assert_eq!(document["phase"], "session_new");
    assert_eq!(document["code"], "session_create_failed");
    assert_eq!(document["stop_reason"], Value::Null);
    assert!(!document.to_string().contains("fake session/new failure"));
    assert_eq!(document["cleanup"]["session_delete"], "skipped");
    assert_eq!(document["cleanup"]["process"], "terminated");
}

#[test]
fn agent_test_json_reports_initialize_failure_before_any_session() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    write_fake_agent_home(tempdir.path(), &["--initialize-error"]);

    let document = agent_test_json(tempdir.path(), &["--prompt", "hello"], false);

    assert_eq!(document["ok"], false);
    assert_eq!(document["phase"], "initialize");
    assert_eq!(document["code"], "agent_initialize_failed");
    assert_eq!(document["cleanup"]["session_delete"], "skipped");
}

#[test]
fn agent_test_json_reports_progress_timeout_code() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    write_fake_agent_home(tempdir.path(), &["--prompt-stall-after-update"]);

    let document = agent_test_json(
        tempdir.path(),
        &[
            "--prompt",
            "hello",
            "--progress-timeout",
            "50ms",
            "--timeout",
            "2s",
        ],
        false,
    );

    assert_eq!(document["phase"], "prompt");
    assert_eq!(document["code"], "progress_timeout");
    // The stalled prompt wedges the agent's single event loop, so the bounded delete cannot
    // complete; the process is still reclaimed and the verdict code is unaffected.
    assert_eq!(document["cleanup"]["session_delete"], "cleanup_failed");
    assert_eq!(document["cleanup"]["process"], "terminated");
}

#[test]
fn agent_test_json_reports_unsupported_delete_session() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    write_fake_agent_home(tempdir.path(), &["--no-cap-delete-session"]);

    let document = agent_test_json(tempdir.path(), &["--prompt", "hello"], true);

    assert_eq!(document["ok"], true);
    assert_eq!(document["cleanup"]["session_delete"], "unsupported");
    assert_eq!(document["cleanup"]["process"], "terminated");
}

#[test]
fn agent_test_json_failed_delete_session_does_not_flip_the_verdict() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    write_fake_agent_home(tempdir.path(), &["--fail-delete-session"]);

    // A working agent with a flaky delete must not read as a failed test.
    let document = agent_test_json(tempdir.path(), &["--prompt", "hello"], true);

    assert_eq!(document["ok"], true);
    assert_eq!(document["phase"], "done");
    assert_eq!(document["code"], "ok");
    assert_eq!(document["cleanup"]["session_delete"], "cleanup_failed");
    assert_eq!(document["cleanup"]["process"], "terminated");
}

#[test]
fn agent_test_writes_no_session_row() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    write_fake_agent_home(tempdir.path(), &[]);

    acps_command(tempdir.path())
        .args(["agent", "test", "--prompt", "hello"])
        .assert()
        .success();

    // `agent test` is disposable: it opens no state store, so no database is left behind.
    let state_path = tempdir.path().join(".local/share/acp-stack/state.sqlite");
    assert!(
        !state_path.exists(),
        "agent test must not open the state store: {}",
        state_path.display()
    );
}

#[test]
fn agent_status_reports_provider_with_unset_model_mode_and_effort() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = format!("{}\n[agent.provider]\nid = \"openai\"\n", codex_config());
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command(tempdir.path())
        .args(["agent", "status"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent: codex"))
        .stdout(predicates::str::contains("provider: openai"))
        .stdout(predicates::str::contains("model, mode, and effort unset"))
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
effort = "high"

[agent.provider]
id = "opencode-go"
model = "deepseek-v4-pro"
api_key_ref = "OPENCODE_API_KEY""#,
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    acps_command(tempdir.path())
        .args(["agent", "status"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent: opencode"))
        .stdout(predicates::str::contains("provider: opencode-go"))
        .stdout(predicates::str::contains("model: deepseek-v4-pro"))
        .stdout(predicates::str::contains("mode: build"))
        .stdout(predicates::str::contains("effort: high"))
        .stdout(predicates::str::contains(" unset").not())
        .stdout(predicates::str::contains(" unavailable").not());
}

#[test]
fn agent_status_reports_kimi_model_with_unset_provider_lane() {
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

    acps_command(tempdir.path())
        .args(["agent", "status"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent: kimi"))
        .stdout(predicates::str::contains("model: gpt-5.5"))
        .stdout(predicates::str::contains(
            "provider, mode, and effort unset",
        ));
}

#[test]
fn agent_status_reports_amp_unavailable_provider_and_unset_model() {
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
mode = "default""#,
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

    acps_command(tempdir.path())
        .args(["agent", "status"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent: amp"))
        .stdout(predicates::str::contains("mode: default"))
        .stdout(predicates::str::contains("model unset"))
        .stdout(predicates::str::contains("provider and effort unavailable"));
}

#[test]
fn agent_status_reports_all_supported_params_unset() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    acps_command(tempdir.path())
        .args(["agent", "status"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent: opencode"))
        .stdout(predicates::str::contains(
            "provider, model, mode, and effort unset",
        ))
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

    acps_command(home.path())
        .args(["agent", "start", "--admin-key", ADMIN_KEY])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent start: running"))
        .stdout(predicates::str::contains("pid: "));

    let output = acps_command(home.path())
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

    acps_command(home.path())
        .args(["agent", "stop", "--admin-key", ADMIN_KEY])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent stop: stopped"));
}

#[test]
fn agent_switch_noninteractive_requires_admin_key() {
    let home = tempfile::tempdir().expect("home tempdir");
    acps_command(home.path())
        .args(["agent", "switch", "opencode"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--admin-key"));
}

#[test]
fn agent_switch_accepts_drop_flag() {
    let home = tempfile::tempdir().expect("home tempdir");
    acps_command(home.path())
        .args(["agent", "switch", "opencode", "--drop"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--admin-key"))
        .stderr(predicates::str::contains("unexpected argument").not());
}
