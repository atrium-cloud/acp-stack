#![cfg(all(feature = "dev-tools", feature = "test-fixtures"))]

use acp_stack::auth::{AuthVerifierSet, KeyKind};
use acp_stack::config::{AgentInstallConfig, load_config_from_str};
use acp_stack::dev_gates::TEST_SKIP_AGENT_INSTALL_ENV;
use acp_stack::secrets::SecretStore;
use acp_stack::state::{StateStore, default_state_path};
use axum::{Json, Router, routing::put};
use base64::Engine;
use http::StatusCode;
use predicates::prelude::PredicateBooleanExt as _;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use tokio::net::TcpListener;

mod common;
use common::cli::*;

fn parse_key_line(stdout: &str, label: &'static str) -> String {
    stdout
        .lines()
        .find_map(|line| line.strip_prefix(label))
        .unwrap_or_else(|| panic!("missing {label} in stdout: {stdout}"))
        .trim()
        .to_owned()
}

fn parse_init_keys(stdout: &str) -> (String, String) {
    (
        parse_key_line(stdout, "session key: "),
        parse_key_line(stdout, "admin key: "),
    )
}

fn run_init_with_home(home: &std::path::Path) -> (String, String) {
    let stdout = acps_command()
        .env("HOME", home)
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(stdout).expect("init stdout utf8");
    parse_init_keys(&stdout)
}

// ----- 0.0.1 auth/secrets/reset/config-import tests -----

fn run_operator_init_with_home(home: &std::path::Path, extra: &[&str]) {
    write_supabase_init_registry(home);
    let workspace = home.join("workspace");
    fs::create_dir_all(&workspace).expect("workspace dir");
    let workspace = workspace.to_str().expect("workspace path utf8");
    let mut args = vec![
        "init",
        "--non-interactive",
        "--agent",
        "supabase-test",
        "--workspace-root",
        workspace,
    ];
    args.extend_from_slice(extra);
    acps_command()
        .env("HOME", home)
        .args(args)
        .assert()
        .success();
}

fn write_supabase_init_registry(home: &std::path::Path) {
    let config_dir = home.join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir");
    fs::write(
        config_dir.join("agents.toml"),
        r#"
[[agents]]
id = "supabase-test"
name = "Supabase Test"
kind = "native"
headless_compatible = true
support_doc = "docs/agents/supabase-test.md"

[agents.harness]
id = "true"

[agents.harness.install.shell]
script = "true"
creates = "true"
"#,
    )
    .expect("agents override");
}

#[test]
fn init_agent_flag_updates_config_non_interactively() {
    let tempdir = tempfile::tempdir().expect("tempdir");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "cursor", "--skip-workspace-init"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent: Cursor CLI (cursor)"));

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config readable");
    assert!(written.contains(r#"id = "cursor""#));
    assert!(written.contains(&format!(
        r#"command = "{}""#,
        env!("CARGO_BIN_EXE_placebo-agent")
    )));
    assert!(written.contains(r#""acp""#));
    assert!(written.contains(r#""--model-config-option""#));
    assert!(written.contains(r#""placebo-model""#));
    assert!(written.contains(r#"env = ["CURSOR_API_KEY"]"#));
    assert!(written.contains("[array.targets.agent.auto_update]"));
    assert!(written.contains("enabled = true"));
    assert!(written.contains(r#"frequency = "1d""#));
    assert!(!written.contains("[array.targets.agent.install]"));
}

#[test]
fn agent_update_set_edits_auto_update_config() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "update", "set", "--auto-on", "--frequency", "3d"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent update auto: enabled"))
        .stdout(predicates::str::contains("frequency: 3d"));

    let config_text =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config readable");
    let config = load_config_from_str(&config_text).expect("config parses after update set");
    let auto_update = config.agent.auto_update.expect("auto-update written");
    assert!(auto_update.enabled);
    assert_eq!(auto_update.frequency, "3d");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "update", "set", "--auto-off"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent update auto: disabled"));

    let config_text =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config readable");
    let config = load_config_from_str(&config_text).expect("config parses after auto-off");
    let auto_update = config.agent.auto_update.expect("auto-update retained");
    assert!(!auto_update.enabled);
    assert_eq!(auto_update.frequency, "3d");
}

#[test]
fn agent_update_set_rejects_invalid_frequency() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "update", "set", "--frequency", "0d"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("agent.auto_update.frequency"));
}

#[test]
fn stack_update_set_edits_update_config() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "update",
            "set",
            "--policy",
            "compatible",
            "--frequency",
            "3d",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "acp-stack update policy: compatible",
        ))
        .stdout(predicates::str::contains("frequency: 3d"));

    let config_text =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config readable");
    let config = load_config_from_str(&config_text).expect("config parses after update set");
    assert_eq!(
        config.updates.acp_stack.policy,
        acp_stack::config::StackUpdatePolicy::Compatible
    );
    assert_eq!(config.updates.acp_stack.frequency, "3d");
}

#[test]
fn stack_update_set_rejects_sub_day_frequency() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["update", "set", "--frequency", "12h"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("updates.acp_stack.frequency"));

    let config_text =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config readable");
    let config = load_config_from_str(&config_text).expect("config still parses after failed set");
    assert_eq!(config.updates.acp_stack.frequency, "1d");
}

#[test]
fn agent_update_set_auto_on_rejects_non_registry_agent() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    // An escape-hatch agent id that the embedded registry does not resolve:
    // enabling auto-update would leave the daemon loop failing every cycle.
    let escape_hatch = VALID_CONFIG.replace(r#"id = "opencode""#, r#"id = "custom-private-agent""#);
    fs::write(config_dir.join("acps-config.toml"), escape_hatch).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "update", "set", "--auto-on"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("not a managed registry agent"));
}

#[test]
fn init_install_agent_runs_selected_registry_install() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir");
    let workspace_root = tempdir.path().join("workspace");
    fs::create_dir(&workspace_root).expect("workspace dir");
    let managed_binary = tempdir.path().join(".local/bin/init-test-agent");
    let config = VALID_CONFIG
        .replace(
            r#"root = "/workspace""#,
            &format!(r#"root = "{}""#, workspace_root.display()),
        )
        .replace(
            r#"uploads = "/workspace/uploads""#,
            &format!(r#"uploads = "{}/uploads""#, workspace_root.display()),
        );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config");
    let script = format!(
        "mkdir -p {bin} && printf '#!/bin/sh\\n' > {binary} && chmod 755 {binary}",
        bin = shell_quote_path(managed_binary.parent().expect("binary has parent")),
        binary = shell_quote_path(&managed_binary),
    );
    fs::write(
        config_dir.join("agents.toml"),
        format!(
            r#"
[[agents]]
id = "init-test"
name = "Init Test"
kind = "native"
headless_compatible = true
support_doc = "docs/agents/init-test.md"

[agents.harness]
id = "init-test-agent"

[agents.harness.install.shell]
script = {script:?}
creates = "init-test-agent"
"#
        ),
    )
    .expect("agents override");

    acps_command()
        .env_remove(TEST_SKIP_AGENT_INSTALL_ENV)
        .env("HOME", tempdir.path())
        .args(["init", "--agent", "init-test"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent install: installed"));

    assert!(managed_binary.is_file());
    let written = fs::read_to_string(config_dir.join("acps-config.toml")).expect("config readable");
    assert!(written.contains(r#"id = "init-test""#));
    assert!(written.contains(r#"command = "init-test-agent""#));
}

#[test]
fn init_creates_age_key_and_encrypted_secret_store() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    let age_key = tempdir.path().join(".config/acp-stack/age.key");
    let store = tempdir.path().join(".local/share/acp-stack/secrets.age");
    assert!(age_key.is_file(), "age key must be written");
    assert!(store.is_file(), "secret store ciphertext must be written");
}

#[cfg(unix)]
#[test]
fn init_age_key_and_store_are_owner_only() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    assert_eq!(
        mode(&tempdir.path().join(".config/acp-stack/age.key")),
        0o600
    );
    assert_eq!(
        mode(&tempdir.path().join(".local/share/acp-stack/secrets.age")),
        0o600,
    );
}

#[test]
fn init_prints_session_and_admin_keys_on_first_run() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let output = acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("utf8");
    assert!(stdout.contains("session key: acps_"));
    assert!(stdout.contains("admin key: acps_"));
    assert!(stdout.contains("save both keys now"));
    assert!(stdout.contains("next: start the runtime with `acps serve`"));
}

#[test]
fn init_text_rerun_prints_next_step_hint_without_keys() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    let output = acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("utf8");
    assert!(!stdout.contains("session key:"));
    assert!(!stdout.contains("admin key:"));
    assert!(stdout.contains("next: start the runtime with `acps serve`"));
}

#[test]
fn init_text_failure_prints_keys_without_next_step_hint() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let output = acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--supabase-url",
            "https://project-ref.supabase.co",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("utf8");
    assert!(stdout.contains("session key: acps_"));
    assert!(stdout.contains("admin key: acps_"));
    assert!(!stdout.contains("next: start the runtime with `acps serve`"));
}

#[test]
fn init_handoff_json_prints_fresh_keys_once() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let output = acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--handoff-json",
            "--agent",
            "placebo",
            "--skip-workspace-init",
            "--skip-testflight",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let body: Value = serde_json::from_slice(&output).expect("handoff json parses");
    assert_eq!(body["status"], "initialized");
    assert_eq!(
        body["config_path"],
        tempdir
            .path()
            .join(".config/acp-stack/acps-config.toml")
            .to_string_lossy()
            .as_ref()
    );
    assert_eq!(
        body["state_path"],
        tempdir
            .path()
            .join(".local/share/acp-stack/state.sqlite")
            .to_string_lossy()
            .as_ref()
    );
    assert_eq!(
        body["secret_store_path"],
        tempdir
            .path()
            .join(".local/share/acp-stack/secrets.age")
            .to_string_lossy()
            .as_ref()
    );
    assert_eq!(
        body["age_key_path"],
        tempdir
            .path()
            .join(".config/acp-stack/age.key")
            .to_string_lossy()
            .as_ref()
    );
    assert_eq!(body["agent"]["id"], "placebo");
    assert_eq!(body["agent"]["name"], "Placebo Agent");
    assert_eq!(body["auth"]["generated_keys"], json!(["session", "admin"]));
    assert_eq!(body["auth"]["preserved_keys"], json!([]));
    let session_key = body["session_key"].as_str().expect("session key");
    let admin_key = body["admin_key"].as_str().expect("admin key");
    assert!(session_key.starts_with("acps_"));
    assert!(admin_key.starts_with("acps_"));

    let store = StateStore::open(default_state_path(tempdir.path())).expect("state store");
    let verifiers = store.load_auth_verifier_pair().expect("auth verifiers");
    assert_eq!(verifiers.verify(session_key), Some(KeyKind::Session));
    assert_eq!(verifiers.verify(admin_key), Some(KeyKind::Admin));
}

#[test]
fn init_handoff_json_preserves_keys_without_reprinting_material() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let (session_key, admin_key) = run_init_with_home(tempdir.path());

    let output = acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--handoff-json",
            "--agent",
            "placebo",
            "--skip-workspace-init",
            "--skip-testflight",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output.clone()).expect("utf8");
    let body: Value = serde_json::from_slice(&output).expect("handoff json parses");
    assert_eq!(body["status"], "initialized");
    assert_eq!(body["auth"]["generated_keys"], json!([]));
    assert_eq!(body["auth"]["preserved_keys"], json!(["session", "admin"]));
    assert!(body.get("session_key").is_none(), "{body}");
    assert!(body.get("admin_key").is_none(), "{body}");
    assert!(!stdout.contains(&session_key));
    assert!(!stdout.contains(&admin_key));
    assert!(!stdout.contains("session key:"));
    assert!(!stdout.contains("admin key:"));
}

#[test]
fn init_handoff_json_rotate_keys_reissues_plaintext_over_existing_state() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let (old_session_key, old_admin_key) = run_init_with_home(tempdir.path());

    let output = acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--handoff-json",
            "--rotate-keys",
            "--agent",
            "placebo",
            "--skip-workspace-init",
            "--skip-testflight",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let body: Value = serde_json::from_slice(&output).expect("handoff json parses");
    assert_eq!(body["status"], "initialized");
    assert_eq!(body["auth"]["generated_keys"], json!(["session", "admin"]));
    assert_eq!(body["auth"]["preserved_keys"], json!([]));
    let session_key = body["session_key"].as_str().expect("session key");
    let admin_key = body["admin_key"].as_str().expect("admin key");
    assert_ne!(session_key, old_session_key);
    assert_ne!(admin_key, old_admin_key);

    let store = StateStore::open(default_state_path(tempdir.path())).expect("state store");
    let verifiers = store.load_auth_verifier_pair().expect("auth verifiers");
    assert_eq!(verifiers.verify(session_key), Some(KeyKind::Session));
    assert_eq!(verifiers.verify(admin_key), Some(KeyKind::Admin));
    assert_eq!(verifiers.verify(&old_session_key), None);
    assert_eq!(verifiers.verify(&old_admin_key), None);
}

#[test]
fn init_handoff_json_failure_reports_fresh_keys_once() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let output = acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--handoff-json",
            "--agent",
            "placebo",
            "--supabase-url",
            "https://project-ref.supabase.co",
            "--skip-workspace-init",
            "--skip-testflight",
        ])
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output.clone()).expect("utf8");
    let body: Value = serde_json::from_slice(&output).expect("handoff json parses");
    assert_eq!(body["status"], "failed");
    assert_eq!(body["auth"]["generated_keys"], json!(["session", "admin"]));
    assert_eq!(body["auth"]["preserved_keys"], json!([]));
    let session_key = body["session_key"].as_str().expect("session key");
    let admin_key = body["admin_key"].as_str().expect("admin key");
    assert!(session_key.starts_with("acps_"));
    assert!(admin_key.starts_with("acps_"));
    assert!(!stdout.contains("session key:"));
    assert!(!stdout.contains("admin key:"));

    let store = StateStore::open(default_state_path(tempdir.path())).expect("state store");
    let verifiers = store.load_auth_verifier_pair().expect("auth verifiers");
    assert_eq!(verifiers.verify(session_key), Some(KeyKind::Session));
    assert_eq!(verifiers.verify(admin_key), Some(KeyKind::Admin));
}

#[test]
fn init_handoff_json_failure_reports_preserved_keys_without_reprinting_material() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let (session_key, admin_key) = run_init_with_home(tempdir.path());
    let config_path = tempdir.path().join(".config/acp-stack/acps-config.toml");
    let mut config =
        load_config_from_str(&fs::read_to_string(&config_path).expect("config readable"))
            .expect("config parses");
    let supabase = config.logging.supabase.as_mut().expect("supabase config");
    supabase.enabled = true;
    supabase.api_key_ref = "MISSING_SUPABASE_SECRET".to_owned();
    fs::write(
        &config_path,
        config.to_canonical_toml().expect("canonical config"),
    )
    .expect("config writable");

    let output = acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--handoff-json",
            "--agent",
            "placebo",
            "--skip-workspace-init",
            "--skip-testflight",
        ])
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output.clone()).expect("utf8");
    let body: Value = serde_json::from_slice(&output).expect("handoff json parses");
    assert_eq!(body["status"], "failed");
    assert_eq!(body["auth"]["generated_keys"], json!([]));
    assert_eq!(body["auth"]["preserved_keys"], json!(["session", "admin"]));
    assert!(body.get("session_key").is_none(), "{body}");
    assert!(body.get("admin_key").is_none(), "{body}");
    assert!(!stdout.contains(&session_key));
    assert!(!stdout.contains(&admin_key));
    assert!(!stdout.contains("session key:"));
    assert!(!stdout.contains("admin key:"));
}

#[test]
fn init_handoff_json_does_not_enable_global_format_json() {
    acps_command()
        .args(["init", "--handoff-json", "--format", "json"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "init does not support --format json",
        ));
}

#[test]
fn init_is_idempotent_and_preserves_keys() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    let store = tempdir.path().join(".local/share/acp-stack/secrets.age");
    let first = fs::read(&store).expect("ciphertext readable");

    let stdout = acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(stdout).expect("utf8");
    assert!(
        stdout.contains("preserved existing API keys"),
        "second init must report preservation, got: {stdout}",
    );
    assert!(
        !stdout.contains("save the admin key now"),
        "second init must not print key material again",
    );

    let second = fs::read(&store).expect("ciphertext readable");
    assert_eq!(
        first, second,
        "ciphertext is rewritten on init even with no changes; investigate",
    );
}

#[test]
fn init_backfills_legacy_auth_refs_without_reprinting_keys() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir");
    let legacy_config = VALID_PLACEBO_CONFIG.replace(
        "[security.http]",
        r#"[auth]
session_key_ref = "ACP_STACK_SESSION_KEY"
admin_key_ref = "ACP_STACK_ADMIN_KEY"

[security.http]"#,
    );
    fs::write(config_dir.join("acps-config.toml"), legacy_config).expect("legacy config");
    let mut secret_store = SecretStore::open_or_create(tempdir.path()).expect("secret store");
    secret_store
        .set_many([
            ("ACP_STACK_SESSION_KEY", SESSION_KEY),
            ("ACP_STACK_ADMIN_KEY", ADMIN_KEY),
        ])
        .expect("legacy auth secrets");

    let output = acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("utf8");
    assert!(
        stdout.contains("preserved existing API keys"),
        "legacy init must preserve old keys, got: {stdout}",
    );
    assert!(!stdout.contains("session key: acps_"));
    assert!(!stdout.contains("admin key: acps_"));
    assert!(!stdout.contains("save the admin key now"));

    let state_path = default_state_path(tempdir.path());
    let store = StateStore::open(&state_path).expect("state store");
    let verifiers = store.load_auth_verifier_pair().expect("auth verifiers");
    assert_eq!(verifiers.verify(SESSION_KEY), Some(KeyKind::Session));
    assert_eq!(verifiers.verify(ADMIN_KEY), Some(KeyKind::Admin));
}

#[test]
fn init_fails_fast_when_only_one_auth_verifier_exists() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    let state_path = default_state_path(tempdir.path());
    fs::remove_file(&state_path).expect("state db should be removable");
    let store = StateStore::open(&state_path).expect("state store should open");
    store.migrate().expect("state schema should migrate");
    store
        .upsert_auth_key(
            KeyKind::Admin,
            &AuthVerifierSet::create(SESSION_KEY, ADMIN_KEY).admin,
        )
        .expect("admin verifier should replace pair");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("auth_keys.session"));
}

#[test]
fn init_fails_fast_when_auth_verifier_is_malformed() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    let state_path = default_state_path(tempdir.path());
    let connection = rusqlite::Connection::open(&state_path).expect("state db should open");
    connection
        .execute(
            "UPDATE auth_keys SET algorithm = 'sha256-v0' WHERE key_kind = 'session'",
            [],
        )
        .expect("auth verifier should be corruptible");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("auth_keys.algorithm"));
}

#[test]
fn secrets_set_only_captures_first_line_of_stdin() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let (_, admin_key) = run_init_with_home(tempdir.path());

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "secrets",
            "set",
            "MULTILINE_TEST",
            "--admin-key",
            admin_key.as_str(),
        ])
        .write_stdin("first-line\nsecond-line\n")
        .assert()
        .success();

    let store = acp_stack::secrets::SecretStore::open(tempdir.path()).expect("open store");
    assert_eq!(store.get("MULTILINE_TEST").expect("get"), "first-line");
}

#[test]
fn init_supabase_url_enables_config_and_env_secret() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_supabase_init_registry(tempdir.path());
    let workspace = tempdir.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace dir");
    let workspace = workspace.to_str().expect("workspace path utf8");

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_SUPABASE_SECRET_KEY", "sb_secret_cli_test")
        .args([
            "init",
            "--non-interactive",
            "--agent",
            "supabase-test",
            "--workspace-root",
            workspace,
            "--supabase-url",
            "https://project-ref.supabase.co",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "supabase secret: set (SUPABASE_SECRET_KEY)",
        ));

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config readable");
    let config = load_config_from_str(&written).expect("config parses");
    let supabase = config.logging.supabase.expect("supabase configured");
    assert!(supabase.enabled);
    assert_eq!(supabase.url, "https://project-ref.supabase.co");
    assert_eq!(supabase.schema, "acp_stack");
    assert_eq!(supabase.api_key_ref, "SUPABASE_SECRET_KEY");
    let store = SecretStore::open(tempdir.path()).expect("store opens");
    assert_eq!(
        store.get("SUPABASE_SECRET_KEY").expect("supabase secret"),
        "sb_secret_cli_test"
    );
    assert!(!written.contains("sb_secret_cli_test"));
}

#[test]
fn init_supabase_env_bootstrap_matches_init_flags() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_supabase_init_registry(tempdir.path());
    let workspace = tempdir.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace dir");
    let workspace = workspace.to_str().expect("workspace path utf8");

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_SUPABASE_URL", "https://env-project.supabase.co")
        .env("ACP_STACK_SUPABASE_SCHEMA", "analytics")
        .env("ACP_STACK_SUPABASE_API_KEY_REF", "ENV_SUPABASE_SECRET")
        .env("ACP_STACK_SUPABASE_SECRET_KEY", "sb_secret_from_env")
        .args([
            "init",
            "--non-interactive",
            "--agent",
            "supabase-test",
            "--workspace-root",
            workspace,
        ])
        .assert()
        .success();

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config readable");
    let config = load_config_from_str(&written).expect("config parses");
    let supabase = config.logging.supabase.expect("supabase configured");
    assert!(supabase.enabled);
    assert_eq!(supabase.url, "https://env-project.supabase.co");
    assert_eq!(supabase.schema, "analytics");
    assert_eq!(supabase.api_key_ref, "ENV_SUPABASE_SECRET");
    let store = SecretStore::open(tempdir.path()).expect("store opens");
    assert_eq!(
        store.get("ENV_SUPABASE_SECRET").expect("supabase secret"),
        "sb_secret_from_env"
    );
}

#[test]
fn init_supabase_non_interactive_requires_secret() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    write_supabase_init_registry(tempdir.path());
    let workspace = tempdir.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace dir");
    let workspace = workspace.to_str().expect("workspace path utf8");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "init",
            "--non-interactive",
            "--agent",
            "supabase-test",
            "--workspace-root",
            workspace,
            "--supabase-url",
            "https://project-ref.supabase.co",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "does not contain the Supabase secret API key reference",
        ));
    let state_path = tempdir.path().join(".local/share/acp-stack/state.sqlite");
    let store = StateStore::open(&state_path).expect("state opens");
    let runs = store.query_init_runs(1).expect("query runs");
    assert_eq!(runs[0].status, acp_stack::state::INIT_RUN_FAILED);
}

#[test]
fn logging_supabase_cli_edits_config_and_secret_store() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_operator_init_with_home(tempdir.path(), &[]);

    let enable_output = acps_command()
        .env("HOME", tempdir.path())
        .args([
            "logging",
            "supabase",
            "enable",
            "--url",
            "https://cli-project.supabase.co",
            "--schema",
            "analytics",
            "--api-key-ref",
            "CLI_SUPABASE_SECRET",
            "--format",
            "json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let enable_body: Value = serde_json::from_slice(&enable_output).expect("enable json parses");
    assert_eq!(enable_body["action"], "enabled");
    assert_eq!(enable_body["api_key_ref"], "CLI_SUPABASE_SECRET");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "logging",
            "supabase",
            "set-secret",
            "--api-key-ref",
            "CLI_SUPABASE_SECRET",
        ])
        .write_stdin("sb_secret_cli_value\nignored\n")
        .assert()
        .success()
        .stdout(predicates::str::contains("sb_secret_cli_value").not());

    let status_output = acps_command()
        .env("HOME", tempdir.path())
        .args(["logging", "supabase", "status", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let status_body: Value = serde_json::from_slice(&status_output).expect("status json parses");
    assert_eq!(status_body["enabled"], true);
    assert_eq!(status_body["schema"], "analytics");
    assert_eq!(status_body["secret_present"], true);
    assert!(!String::from_utf8_lossy(&status_output).contains("sb_secret_cli_value"));

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config readable");
    let config = load_config_from_str(&written).expect("config parses");
    let supabase = config.logging.supabase.expect("supabase configured");
    assert!(supabase.enabled);
    assert_eq!(supabase.url, "https://cli-project.supabase.co");
    assert_eq!(supabase.schema, "analytics");
    assert_eq!(supabase.api_key_ref, "CLI_SUPABASE_SECRET");
    assert!(!written.contains("sb_secret_cli_value"));
    let store = SecretStore::open(tempdir.path()).expect("store opens");
    assert_eq!(
        store.get("CLI_SUPABASE_SECRET").expect("supabase secret"),
        "sb_secret_cli_value"
    );

    acps_command()
        .env("HOME", tempdir.path())
        .args(["logging", "supabase", "disable"])
        .assert()
        .success();
    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config readable");
    let config = load_config_from_str(&written).expect("config parses");
    let supabase = config.logging.supabase.expect("supabase configured");
    assert!(!supabase.enabled);
    assert_eq!(supabase.url, "https://cli-project.supabase.co");
    assert_eq!(supabase.schema, "analytics");
}

#[test]
fn logging_supabase_setup_uses_cli_and_stores_writer_db_url() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_operator_init_with_home(tempdir.path(), &[]);
    let fake_bin = tempdir.path().join("bin");
    fs::create_dir_all(&fake_bin).expect("fake bin");
    let fake_log = tempdir.path().join("supabase.log");
    let fake_supabase = fake_bin.join("supabase");
    fs::write(
        &fake_supabase,
        "#!/bin/sh\nprintf '%s|%s\\n' \"$PWD\" \"$*\" >> \"$FAKE_SUPABASE_LOG\"\nexit 0\n",
    )
    .expect("write fake supabase");
    #[cfg(unix)]
    fs::set_permissions(&fake_supabase, fs::Permissions::from_mode(0o755))
        .expect("chmod fake supabase");
    let path = format!(
        "{}:{}",
        fake_bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let setup_output = acps_command()
        .env("HOME", tempdir.path())
        .env("PATH", path)
        .env("FAKE_SUPABASE_LOG", &fake_log)
        .args([
            "logging",
            "supabase",
            "setup",
            "--url",
            "https://psklvkrmvqqwzryiawgn.supabase.co/",
            "--yes",
            "--format",
            "json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let setup_body: Value = serde_json::from_slice(&setup_output).expect("setup json parses");
    assert_eq!(setup_body["backend"], "postgres");
    assert_eq!(setup_body["db_url_ref"], "SUPABASE_LOG_DB_URL");
    assert!(!String::from_utf8_lossy(&setup_output).contains("postgresql://"));

    let fake_log = fs::read_to_string(fake_log).expect("read fake log");
    assert!(fake_log.contains("|init\n"), "{fake_log}");
    assert!(
        fake_log.contains("|link --project-ref psklvkrmvqqwzryiawgn\n"),
        "{fake_log}"
    );
    assert!(fake_log.contains("|db push --yes\n"), "{fake_log}");

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config readable");
    let config = load_config_from_str(&written).expect("config parses");
    let supabase = config.logging.supabase.expect("supabase configured");
    assert!(supabase.enabled);
    assert_eq!(supabase.url, "https://psklvkrmvqqwzryiawgn.supabase.co");
    assert_eq!(
        supabase.backend,
        acp_stack::config::SupabaseLoggingBackend::Postgres
    );
    assert_eq!(supabase.schema, "public");
    assert_eq!(supabase.table_prefix, "acp_stack_");
    assert_eq!(supabase.db_url_ref.as_deref(), Some("SUPABASE_LOG_DB_URL"));
    assert!(!written.contains("postgresql://"));

    let store = SecretStore::open(tempdir.path()).expect("store opens");
    let db_url = store.get("SUPABASE_LOG_DB_URL").expect("db url");
    assert!(db_url.starts_with("postgresql://acp_stack_logger:"));
    assert!(db_url.contains("@db.psklvkrmvqqwzryiawgn.supabase.co:5432/postgres?sslmode=require"));
}

#[test]
fn logging_supabase_sql_prints_prefixed_public_ddl() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_operator_init_with_home(tempdir.path(), &[]);

    let output = acps_command()
        .env("HOME", tempdir.path())
        .args([
            "logging",
            "supabase",
            "sql",
            "--writer-password",
            "test_writer_password",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let sql = String::from_utf8(output).expect("sql utf8");
    assert!(sql.contains("CREATE TABLE IF NOT EXISTS \"public\".\"acp_stack_events\""));
    assert!(sql.contains("CREATE ROLE \"acp_stack_logger\" LOGIN PASSWORD 'test_writer_password'"));
    assert!(sql.contains("SECURITY DEFINER"));
    assert!(sql.contains(
        "GRANT EXECUTE ON FUNCTION \"public\".\"acp_stack_ingest_batch\"(text, jsonb) TO \"acp_stack_logger\""
    ));
    assert!(sql.contains("REVOKE ALL ON TABLE"));
    for table in [
        "schema_migrations",
        "events",
        "sessions",
        "prompts",
        "commands",
        "permission_requests",
        "permission_decisions",
        "auth_failures",
        "agent_lifecycle",
    ] {
        assert!(
            sql.contains(&format!(
                "ALTER TABLE \"public\".\"acp_stack_{table}\" ENABLE ROW LEVEL SECURITY"
            )),
            "missing RLS enablement for {table}"
        );
    }
    for view in [
        "session_turns",
        "permissions",
        "agent_events",
        "security_events",
        "connection_events",
        "usage_metrics",
    ] {
        assert!(
            sql.contains(&format!(
                "CREATE OR REPLACE VIEW \"public\".\"acp_stack_{view}\"\nWITH (security_invoker = true) AS"
            )),
            "missing security_invoker for {view}"
        );
    }
    // PUBLIC is revoked unconditionally; anon/authenticated are revoked only
    // behind a pg_roles existence guard (so the SQL is safe on a non-Supabase
    // Postgres), never as an unconditional `FROM PUBLIC, "anon", "authenticated"`.
    assert!(sql.contains("FROM PUBLIC;"));
    assert!(sql.contains("IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = api_role_name)"));
    assert!(sql.contains("EXECUTE format('REVOKE ALL ON TABLE"));
    assert!(sql.contains("EXECUTE format('REVOKE ALL ON FUNCTION"));
    assert!(!sql.contains("FROM PUBLIC, \"anon\", \"authenticated\""));
    // Writes go through the SECURITY DEFINER ingest function, so the writer role
    // gets no direct table access and no per-table RLS policies are emitted.
    assert!(!sql.contains("CREATE POLICY"));
    assert!(!sql.contains("FOR INSERT TO \"acp_stack_logger\""));
    assert!(!sql.contains("FOR UPDATE TO \"acp_stack_logger\""));
    assert!(!sql.contains("GRANT INSERT, UPDATE, SELECT ON TABLE"));
    assert!(!sql.contains(" TO PUBLIC"));
    assert!(!sql.contains(" TO \"anon\""));
    assert!(!sql.contains(" TO \"authenticated\""));
    assert!(!sql.contains("FOR SELECT TO \"acp_stack_logger\""));
    assert!(sql.contains("failure_detail_json jsonb"));
    assert!(sql.contains("message_id_acknowledged boolean NOT NULL DEFAULT false"));
    assert!(sql.contains("output_bytes bigint NOT NULL DEFAULT 0"));
}

#[test]
fn logging_supabase_sql_rejects_unsafe_schema() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_operator_init_with_home(tempdir.path(), &[]);

    // A schema with a single quote would break out of the PL/pgSQL `format()`
    // string literal in the generated revoke statements; reject it up front.
    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "logging",
            "supabase",
            "sql",
            "--schema",
            "pub'lic",
            "--writer-password",
            "test_writer_password",
        ])
        .assert()
        .failure();
}

#[test]
fn init_supabase_env_does_not_rewrite_existing_config() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_operator_init_with_home(tempdir.path(), &[]);
    let workspace = tempdir.path().join("workspace");
    let workspace = workspace.to_str().expect("workspace path utf8");

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_SUPABASE_URL", "https://ambient.supabase.co")
        .env("ACP_STACK_SUPABASE_SECRET_KEY", "sb_secret_ambient")
        .args([
            "init",
            "--non-interactive",
            "--agent",
            "supabase-test",
            "--workspace-root",
            workspace,
        ])
        .assert()
        .success();

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config readable");
    let config = load_config_from_str(&written).expect("config parses");
    let supabase = config.logging.supabase.expect("supabase configured");
    assert!(!supabase.enabled);
    assert_eq!(supabase.url, "https://example.supabase.co");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "init",
            "--non-interactive",
            "--agent",
            "supabase-test",
            "--workspace-root",
            workspace,
            "--supabase-url",
            "https://explicit.supabase.co",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "use `acps logging supabase` for initialized instances",
        ));
}

#[test]
fn logging_supabase_enable_rejects_invalid_url_before_writing() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_operator_init_with_home(tempdir.path(), &[]);
    let before = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config readable");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "logging",
            "supabase",
            "enable",
            "--url",
            "http://cli-project.supabase.co",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("must start with `https://`"));

    let after = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config readable");
    assert_eq!(before, after);
}

#[test]
fn init_fails_fast_when_admin_verifier_missing() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    let state_path = default_state_path(tempdir.path());
    fs::remove_file(&state_path).expect("state db should be removable");
    let store = StateStore::open(&state_path).expect("state store should open");
    store.migrate().expect("state schema should migrate");
    store
        .upsert_auth_key(
            KeyKind::Session,
            &AuthVerifierSet::create(SESSION_KEY, ADMIN_KEY).session,
        )
        .expect("session verifier should be stored");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("auth_keys.admin"));
}

#[test]
fn secrets_set_requires_admin_key() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    acps_command()
        .env("HOME", tempdir.path())
        .args(["secrets", "set", "OPENCODE_API_KEY"])
        .write_stdin("attacker-supplied")
        .assert()
        .failure()
        .stderr(predicates::str::contains("--admin-key"));
}

#[test]
fn secrets_set_allows_old_auth_ref_names_with_admin_key() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let (_, admin_key) = run_init_with_home(tempdir.path());

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "secrets",
            "set",
            "ACP_STACK_SESSION_KEY",
            "--admin-key",
            admin_key.as_str(),
        ])
        .write_stdin("ordinary-secret")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "set secret: ACP_STACK_SESSION_KEY",
        ));
}

#[test]
fn secrets_delete_requires_admin_key() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let (_, admin_key) = run_init_with_home(tempdir.path());

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "secrets",
            "set",
            "TEMP_VALUE",
            "--admin-key",
            admin_key.as_str(),
        ])
        .write_stdin("abc")
        .assert()
        .success();

    acps_command()
        .env("HOME", tempdir.path())
        .args(["secrets", "delete", "TEMP_VALUE"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--admin-key"));
}

#[test]
fn secrets_list_shows_session_and_admin_names_only_after_init() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    acps_command()
        .env("HOME", tempdir.path())
        .args(["secrets", "list"])
        .assert()
        .success()
        .stdout(predicates::str::contains("ACP_STACK_ADMIN_KEY").not())
        .stdout(predicates::str::contains("ACP_STACK_SESSION_KEY").not())
        .stdout(predicates::str::contains("acps_").not());
}

#[test]
fn secrets_commands_format_json_never_print_values() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let (_, admin_key) = run_init_with_home(tempdir.path());

    let set_output = acps_command()
        .env("HOME", tempdir.path())
        .args([
            "secrets",
            "set",
            "OPENCODE_API_KEY",
            "--format",
            "json",
            "--admin-key",
            admin_key.as_str(),
        ])
        .write_stdin("super-secret-value\n")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let set_body: Value = serde_json::from_slice(&set_output).expect("set json parses");
    assert_eq!(set_body["action"], "set");
    assert_eq!(set_body["name"], "OPENCODE_API_KEY");
    assert!(!String::from_utf8_lossy(&set_output).contains("super-secret-value"));

    let list_output = acps_command()
        .env("HOME", tempdir.path())
        .args(["secrets", "list", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let list_body: Value = serde_json::from_slice(&list_output).expect("list json parses");
    let names = list_body["secrets"]
        .as_array()
        .expect("secrets should be an array");
    assert!(names.iter().any(|name| name == "OPENCODE_API_KEY"));
    assert!(!String::from_utf8_lossy(&list_output).contains("super-secret-value"));

    let delete_output = acps_command()
        .env("HOME", tempdir.path())
        .args([
            "secrets",
            "delete",
            "OPENCODE_API_KEY",
            "--format",
            "json",
            "--admin-key",
            admin_key.as_str(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let delete_body: Value = serde_json::from_slice(&delete_output).expect("delete json parses");
    assert_eq!(delete_body["action"], "delete");
    assert_eq!(delete_body["name"], "OPENCODE_API_KEY");
}

#[test]
fn secrets_set_reads_value_from_stdin() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let (_, admin_key) = run_init_with_home(tempdir.path());

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "secrets",
            "set",
            "OPENCODE_API_KEY",
            "--admin-key",
            admin_key.as_str(),
        ])
        .write_stdin("super-secret-value\n")
        .assert()
        .success()
        .stdout(predicates::str::contains("set secret: OPENCODE_API_KEY"));

    acps_command()
        .env("HOME", tempdir.path())
        .args(["secrets", "list"])
        .assert()
        .success()
        .stdout(predicates::str::contains("OPENCODE_API_KEY"));
}

#[test]
fn secrets_set_accepts_name_and_value_flags() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let (_, admin_key) = run_init_with_home(tempdir.path());

    let output = acps_command()
        .env("HOME", tempdir.path())
        .args([
            "secrets",
            "set",
            "--name",
            "MOONSHOT_API_KEY",
            "--value",
            "super-secret-value",
            "--admin-key",
            admin_key.as_str(),
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("set secret: MOONSHOT_API_KEY"))
        .get_output()
        .stdout
        .clone();
    assert!(!String::from_utf8_lossy(&output).contains("super-secret-value"));

    acps_command()
        .env("HOME", tempdir.path())
        .args(["secrets", "list"])
        .assert()
        .success()
        .stdout(predicates::str::contains("MOONSHOT_API_KEY"));
}

#[test]
fn secrets_set_rejects_positional_name_with_name_flag() {
    let tempdir = tempfile::tempdir().expect("tempdir");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "secrets",
            "set",
            "OPENCODE_API_KEY",
            "--name",
            "MOONSHOT_API_KEY",
            "--value",
            "super-secret-value",
            "--admin-key",
            "unused",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "pass the secret name either positionally or with --name, not both",
        ));
}

#[test]
fn secrets_delete_removes_named_secret() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let (_, admin_key) = run_init_with_home(tempdir.path());

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "secrets",
            "set",
            "TEMP_VALUE",
            "--admin-key",
            admin_key.as_str(),
        ])
        .write_stdin("abc")
        .assert()
        .success();

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "secrets",
            "delete",
            "TEMP_VALUE",
            "--admin-key",
            admin_key.as_str(),
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("deleted secret: TEMP_VALUE"));

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "secrets",
            "delete",
            "TEMP_VALUE",
            "--admin-key",
            admin_key.as_str(),
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("was not found"));
}

#[test]
fn auth_regenerate_session_key_requires_admin_key() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    acps_command()
        .env("HOME", tempdir.path())
        .args(["auth", "regenerate-session-key"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--admin-key"));
}

#[test]
fn reset_without_yes_lists_targets_and_keeps_files() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    acps_command()
        .env("HOME", tempdir.path())
        .arg("reset")
        .assert()
        .failure()
        .stdout(predicates::str::contains("acps reset would delete:"))
        .stdout(predicates::str::contains("acps-config.toml"))
        .stdout(predicates::str::contains("state.sqlite"))
        .stdout(predicates::str::contains("age.key"))
        .stdout(predicates::str::contains("secrets.age"))
        .stdout(predicates::str::contains("re-run with --yes"));

    assert!(
        tempdir
            .path()
            .join(".config/acp-stack/acps-config.toml")
            .exists(),
        "dry-run must NOT remove files",
    );
}

#[test]
fn reset_dry_run_does_not_write_cli_error_event() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    acps_command()
        .env("HOME", tempdir.path())
        .arg("reset")
        .assert()
        .failure();

    // The dry-run contract is "exits without touching the filesystem".
    // Recording a `cli.error` event row would touch state.sqlite, so the
    // event log must show no error rows after a dry-run reset.
    acps_command()
        .env("HOME", tempdir.path())
        .args(["logs", "query", "--level", "error"])
        .assert()
        .success()
        .stdout("");
}

#[test]
fn reset_with_yes_wipes_config_state_age_key_and_secret_store() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    acps_command()
        .env("HOME", tempdir.path())
        .args(["reset", "--yes"])
        .assert()
        .success()
        .stdout(predicates::str::contains("reset acp-stack"));

    assert!(
        !tempdir
            .path()
            .join(".config/acp-stack/acps-config.toml")
            .exists()
    );
    assert!(!tempdir.path().join(".config/acp-stack/age.key").exists());
    assert!(
        !tempdir
            .path()
            .join(".local/share/acp-stack/state.sqlite")
            .exists()
    );
    assert!(
        !tempdir
            .path()
            .join(".local/share/acp-stack/secrets.age")
            .exists()
    );

    // Re-running reset is idempotent and does not error on missing files.
    acps_command()
        .env("HOME", tempdir.path())
        .args(["reset", "--yes"])
        .assert()
        .success();

    // Fresh init after reset produces a different admin key than the first.
    let init_after = acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(init_after).expect("utf8");
    assert!(stdout.contains("admin key: acps_"));
}

#[test]
fn config_import_refuses_without_force_when_config_exists() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    let exported = acps_command()
        .env("HOME", tempdir.path())
        .args(["config", "export"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let import_path = tempdir.path().join("exported.toml");
    fs::write(&import_path, exported).expect("write export");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["config", "import", import_path.to_str().unwrap()])
        .assert()
        .failure()
        .stdout("")
        .stderr(predicates::str::contains("config already exists"))
        .stderr(predicates::str::contains("--admin-key").not());
}

#[test]
fn config_import_with_force_replaces_existing_config() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let (_, admin_key) = run_init_with_home(tempdir.path());

    // Build an alternate config with a recognizable bind addr.
    let modified =
        VALID_PLACEBO_CONFIG.replace(r#"bind = "127.0.0.1:7700""#, r#"bind = "127.0.0.1:7777""#);
    let import_path = tempdir.path().join("alt.toml");
    fs::write(&import_path, &modified).expect("write alt");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "config",
            "import",
            import_path.to_str().unwrap(),
            "--force",
            "--admin-key",
            admin_key.as_str(),
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("imported config (replaced)"));

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config readable");
    assert!(written.contains("127.0.0.1:7777"));
}

#[test]
fn config_import_force_replaces_invalid_existing_config() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let (_, admin_key) = run_init_with_home(tempdir.path());
    let config_path = tempdir.path().join(".config/acp-stack/acps-config.toml");
    fs::write(&config_path, "not valid toml").expect("write invalid config");

    let modified =
        VALID_PLACEBO_CONFIG.replace(r#"bind = "127.0.0.1:7700""#, r#"bind = "127.0.0.1:7778""#);
    let import_path = tempdir.path().join("replacement.toml");
    fs::write(&import_path, &modified).expect("write replacement");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "config",
            "import",
            import_path.to_str().unwrap(),
            "--force",
            "--admin-key",
            admin_key.as_str(),
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("imported config (replaced)"))
        .stdout(predicates::str::contains(
            "local session access will apply on next daemon start",
        ));

    let written = fs::read_to_string(config_path).expect("config readable");
    assert!(written.contains("127.0.0.1:7778"));
}

#[tokio::test(flavor = "multi_thread")]
async fn config_import_treats_auth_rejection_from_previous_daemon_as_deferred_apply() {
    for status in [StatusCode::UNAUTHORIZED, StatusCode::FORBIDDEN] {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind rejecting daemon");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        let app = Router::new().route(
            "/v1/auth/local-session-access",
            put(move || async move {
                (
                    status,
                    Json(json!({
                        "ok": false,
                        "error": {
                            "code": "auth.invalid",
                            "message": "invalid credential",
                            "details": {}
                        }
                    })),
                )
            }),
        );
        let join = tokio::spawn(async move { axum::serve(listener, app).await });

        write_cli_home(tempdir.path(), &base_url, ADMIN_KEY);
        let modified = VALID_PLACEBO_CONFIG
            .replace(r#"bind = "127.0.0.1:7700""#, r#"bind = "127.0.0.1:7786""#);
        let import_path = tempdir.path().join("replacement.toml");
        fs::write(&import_path, &modified).expect("write replacement");

        acps_command()
            .env("HOME", tempdir.path())
            .args([
                "config",
                "import",
                import_path.to_str().unwrap(),
                "--force",
                "--admin-key",
                ADMIN_KEY,
            ])
            .assert()
            .success()
            .stdout(predicates::str::contains("imported config (replaced)"))
            .stdout(predicates::str::contains(
                "local session access will apply on next daemon start",
            ));

        join.abort();
    }
}

#[test]
fn config_validate_and_import_dry_run_format_json() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let (_, admin_key) = run_init_with_home(tempdir.path());
    let config_path = tempdir.path().join(".config/acp-stack/acps-config.toml");

    let validate_output = acps_command()
        .env("HOME", tempdir.path())
        .args(["config", "validate", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let validate_body: Value =
        serde_json::from_slice(&validate_output).expect("validate json parses");
    assert_eq!(validate_body["valid"], true);
    assert!(validate_body["path"].is_null(), "{validate_body}");

    let import_output = acps_command()
        .env("HOME", tempdir.path())
        .arg("config")
        .arg("import")
        .arg(&config_path)
        .args([
            "--dry-run",
            "--format",
            "json",
            "--admin-key",
            admin_key.as_str(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let import_body: Value = serde_json::from_slice(&import_output).expect("import json parses");
    assert_eq!(import_body["dry_run"], true);
    assert_eq!(import_body["target_exists"], true);
    assert!(import_body.get("auth_refs_unchanged").is_none());
}

#[test]
fn config_export_format_json_wraps_toml_without_leaking_secret_values() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    let output = acps_command()
        .env("HOME", tempdir.path())
        .args(["config", "export", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let body: Value = serde_json::from_slice(&output).expect("config export json parses");
    assert_eq!(body["format"], "toml");
    assert!(body["bytes"].as_u64().unwrap_or(0) > 0);
    let value = body["value"].as_str().expect("exported value is string");
    assert!(!value.contains("ACP_STACK_SESSION_KEY"));
    assert!(!value.contains("ACP_STACK_ADMIN_KEY"));
    assert!(!value.contains(SESSION_KEY));
    assert!(!value.contains(ADMIN_KEY));
}

#[test]
fn config_export_to_output_reports_progress() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());
    let output_path = tempdir.path().join("exported.toml");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["config", "export", "--output"])
        .arg(&output_path)
        .assert()
        .success()
        .stdout(predicates::str::contains("progress: loading config"))
        .stdout(predicates::str::contains(
            "progress: rendering config export",
        ))
        .stdout(predicates::str::contains("progress: writing config export"));

    let exported = fs::read_to_string(output_path).expect("export should be written");
    assert!(!exported.contains("ACP_STACK_SESSION_KEY"));
    assert!(!exported.contains("ACP_STACK_ADMIN_KEY"));
}

#[test]
fn config_import_supports_base64_input() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let (_, admin_key) = run_init_with_home(tempdir.path());
    let modified =
        VALID_PLACEBO_CONFIG.replace(r#"bind = "127.0.0.1:7700""#, r#"bind = "127.0.0.1:7788""#);
    let encoded = base64::engine::general_purpose::STANDARD.encode(modified);

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "config",
            "import",
            "--base64",
            &encoded,
            "--force",
            "--admin-key",
            admin_key.as_str(),
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("progress: reading config import"))
        .stdout(predicates::str::contains(
            "progress: validating config import",
        ))
        .stdout(predicates::str::contains("progress: writing config import"))
        .stdout(predicates::str::contains("imported config"));

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config readable");
    assert!(written.contains("127.0.0.1:7788"));
}

#[test]
fn init_from_base64_imports_config_and_continues() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let modified =
        VALID_PLACEBO_CONFIG.replace(r#"bind = "127.0.0.1:7700""#, r#"bind = "127.0.0.1:7791""#);
    let encoded = base64::engine::general_purpose::STANDARD.encode(modified);

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--from-base64",
            &encoded,
            "--non-interactive",
            "--skip-workspace-init",
            "--skip-testflight",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("progress: reading config import"))
        .stdout(predicates::str::contains("imported config:"))
        .stdout(predicates::str::contains("initialized acp-stack"));

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config readable");
    assert!(written.contains("127.0.0.1:7791"));
}

#[test]
fn init_from_file_imports_config_and_continues() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let modified =
        VALID_PLACEBO_CONFIG.replace(r#"bind = "127.0.0.1:7700""#, r#"bind = "127.0.0.1:7792""#);
    let import_path = tempdir.path().join("import-acps-config.toml");
    fs::write(&import_path, modified).expect("import config");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--from-file",
            import_path.to_str().expect("path utf8"),
            "--non-interactive",
            "--skip-workspace-init",
            "--skip-testflight",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("progress: reading config import"))
        .stdout(predicates::str::contains("initialized acp-stack"));

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config readable");
    assert!(written.contains("127.0.0.1:7792"));
}

#[test]
fn init_from_toml_imports_config_and_continues() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let modified =
        VALID_PLACEBO_CONFIG.replace(r#"bind = "127.0.0.1:7700""#, r#"bind = "127.0.0.1:7793""#);

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--from-toml",
            &modified,
            "--non-interactive",
            "--skip-workspace-init",
            "--skip-testflight",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("progress: reading config import"))
        .stdout(predicates::str::contains("initialized acp-stack"));

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config readable");
    assert!(written.contains("127.0.0.1:7793"));
}

#[test]
fn init_from_base64_rejects_invalid_base64() {
    let tempdir = tempfile::tempdir().expect("tempdir");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--from-base64",
            "!!!not-base64!!!",
            "--non-interactive",
            "--skip-workspace-init",
            "--skip-testflight",
        ])
        .assert()
        .failure()
        .stdout("")
        .stderr(predicates::str::contains("not valid base64"));

    assert!(
        !tempdir
            .path()
            .join(".config/acp-stack/acps-config.toml")
            .exists(),
        "invalid base64 must not create a config file"
    );
}

#[test]
fn config_import_requires_admin_key() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    let modified =
        VALID_PLACEBO_CONFIG.replace(r#"bind = "127.0.0.1:7700""#, r#"bind = "127.0.0.1:7781""#);
    let import_path = tempdir.path().join("rotated.toml");
    fs::write(&import_path, &modified).expect("write rotated");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["config", "import", import_path.to_str().unwrap(), "--force"])
        .assert()
        .failure()
        .stdout("")
        .stderr(predicates::str::contains("--admin-key"));
}

#[test]
fn config_import_strips_legacy_auth_section() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let (_, admin_key) = run_init_with_home(tempdir.path());

    let modified = VALID_PLACEBO_CONFIG.replace(
        "[security.http]",
        r#"[auth]
session_key_ref = "ACP_STACK_SESSION_KEY"
admin_key_ref = "ACP_STACK_ADMIN_KEY"

[security.http]"#,
    );
    let import_path = tempdir.path().join("rotated-session.toml");
    fs::write(&import_path, &modified).expect("write rotated session");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "config",
            "import",
            import_path.to_str().unwrap(),
            "--force",
            "--admin-key",
            admin_key.as_str(),
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("imported config (replaced)"));

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config");
    assert!(!written.contains("[auth]"));
    assert!(!written.contains("session_key_ref"));
    assert!(!written.contains("admin_key_ref"));
}

#[test]
fn config_import_rejects_invalid_base64() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    acps_command()
        .env("HOME", tempdir.path())
        .args(["config", "import", "--base64", "!!!not-base64!!!"])
        .assert()
        .failure()
        .stdout("")
        .stderr(predicates::str::contains("not valid base64"));
}

#[test]
fn config_import_dry_run_with_path() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());
    let config_path = tempdir.path().join(".config/acp-stack/acps-config.toml");
    let original_config = fs::read_to_string(&config_path).expect("config readable");

    let import_path = tempdir.path().join("import.toml");
    fs::write(&import_path, VALID_PLACEBO_CONFIG).expect("write config");

    let output = acps_command()
        .env("HOME", tempdir.path())
        .args([
            "config",
            "import",
            import_path.to_str().unwrap(),
            "--dry-run",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("utf8");
    assert!(stdout.contains("import dry-run complete"));
    assert!(stdout.contains("config_version:"));
    assert!(stdout.contains("canonical TOML size:"));
    assert!(stdout.contains("would write to:"));
    let current_config = fs::read_to_string(&config_path).expect("config readable");
    assert_eq!(current_config, original_config);
}

#[test]
fn config_import_dry_run_with_base64() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());
    let config_path = tempdir.path().join(".config/acp-stack/acps-config.toml");
    let original_config = fs::read_to_string(&config_path).expect("config readable");

    let encoded = base64::engine::general_purpose::STANDARD.encode(VALID_PLACEBO_CONFIG);

    let output = acps_command()
        .env("HOME", tempdir.path())
        .args(["config", "import", "--base64", &encoded, "--dry-run"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("utf8");
    assert!(stdout.contains("import dry-run complete"));
    assert!(stdout.contains("config_version:"));
    assert!(stdout.contains("would write to:"));
    let current_config = fs::read_to_string(&config_path).expect("config readable");
    assert_eq!(current_config, original_config);
}

#[test]
fn config_import_rejects_oversized_path_input() {
    let tempdir = tempfile::tempdir().expect("tempdir");

    let big_config = "x".repeat(2 * 1024 * 1024); // 2 MiB
    let import_path = tempdir.path().join("big.toml");
    fs::write(&import_path, &big_config).expect("write big config");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["config", "import", import_path.to_str().unwrap()])
        .assert()
        .failure()
        .stdout("")
        .stderr(predicates::str::contains("exceeds 1048576-byte size limit"));
}

#[test]
fn init_records_run_with_succeeded_steps() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    let state_path = tempdir.path().join(".local/share/acp-stack/state.sqlite");
    let store = acp_stack::state::StateStore::open(&state_path).expect("state opens");

    let runs = store.query_init_runs(10).expect("query runs");
    assert_eq!(runs.len(), 1, "first init must record exactly one run");
    let run = &runs[0];
    assert_eq!(run.status, acp_stack::state::INIT_RUN_SUCCEEDED);

    let steps = store.query_init_steps(&run.id).expect("query steps");
    assert!(!steps.is_empty(), "run must record at least one step");
    let kinds: Vec<&str> = steps.iter().map(|s| s.kind.as_str()).collect();
    assert!(
        kinds.contains(&"secrets_init"),
        "expected secrets_init in {kinds:?}",
    );
    assert!(
        kinds.contains(&"init_complete"),
        "expected init_complete in {kinds:?}",
    );
    for step in &steps {
        assert!(
            matches!(step.status.as_str(), "succeeded" | "skipped"),
            "step `{}` settled with unexpected status `{}`",
            step.kind,
            step.status,
        );
    }
}

#[test]
fn init_records_workspace_before_provider_configure() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let workspace = tempdir.path().join("workspace");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--workspace-root",
            workspace.to_str().expect("workspace path should be UTF-8"),
        ])
        .assert()
        .success();

    let state_path = tempdir.path().join(".local/share/acp-stack/state.sqlite");
    let store = acp_stack::state::StateStore::open(&state_path).expect("state opens");
    let run = store
        .query_init_runs(1)
        .expect("query runs")
        .into_iter()
        .next()
        .expect("init run");
    let steps = store.query_init_steps(&run.id).expect("query steps");
    let workspace_step = steps
        .iter()
        .find(|step| step.kind == "workspace_materialize")
        .expect("workspace step");
    let provider_step = steps
        .iter()
        .find(|step| step.kind == "provider_configure")
        .expect("provider step");

    assert!(
        workspace_step.ordinal < provider_step.ordinal,
        "workspace materialization must run before provider/model discovery: {steps:?}",
    );
}

#[test]
fn init_resume_targets_specific_pending_run_by_id() {
    // Simulate the post-crash shape: a prior init created the run but
    // never reached `init_complete`, so the row stays `pending`.
    // `acps init --resume --run-id <id>` must pick it up, run any
    // remaining steps, and finalize it `succeeded`.
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    let state_path = tempdir.path().join(".local/share/acp-stack/state.sqlite");
    let store = acp_stack::state::StateStore::open(&state_path).expect("state opens");
    // Inject a synthetic pending run that resume will discover. Use the
    // public state API so this test exercises the same code path the
    // orchestrator would on a real crash mid-init.
    let pending = store
        .create_init_run(acp_stack::state::NewInitRun {
            runtime_user: None,
            agent_id: None,
            args_json: "{}",
        })
        .expect("synth pending run");
    let pending_id = pending.id.clone();
    drop(store);

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--resume",
            "--run-id",
            &pending_id,
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    let store = acp_stack::state::StateStore::open(&state_path).expect("state opens");
    let reloaded = store
        .lookup_init_run(&pending_id)
        .expect("lookup")
        .expect("pending row should still exist");
    assert_eq!(reloaded.status, acp_stack::state::INIT_RUN_SUCCEEDED);
    let steps = store.query_init_steps(&pending_id).expect("steps");
    assert!(
        !steps.is_empty(),
        "resume should have populated steps for the pending run",
    );
    for step in &steps {
        assert!(
            matches!(step.status.as_str(), "succeeded" | "skipped"),
            "step `{}` settled with unexpected status `{}`",
            step.kind,
            step.status,
        );
    }
}

#[test]
fn init_resume_retries_failed_agent_install_even_without_install_flag() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    let workspace = tempdir.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace dir should be created");
    let missing_creates = tempdir.path().join("missing-resume-install-marker");
    let config_path = tempdir.path().join(".config/acp-stack/acps-config.toml");
    let mut config =
        load_config_from_str(&fs::read_to_string(&config_path).expect("config should be readable"))
            .expect("config should validate");
    config.workspace.root = workspace.to_string_lossy().into_owned();
    config.agent.id = "resume-install-test".to_owned();
    config.agent.name = "Resume Install Test".to_owned();
    config.agent.command = "resume-install-test-agent".to_owned();
    config.agent.args.clear();
    config.agent.install = Some(AgentInstallConfig {
        install_type: "shell".to_owned(),
        creates: missing_creates.to_string_lossy().into_owned(),
        shell: Some("true".to_owned()),
    });
    fs::write(
        &config_path,
        config.to_canonical_toml().expect("canonical config"),
    )
    .expect("config should be written");

    let state_path = tempdir.path().join(".local/share/acp-stack/state.sqlite");
    let store = acp_stack::state::StateStore::open(&state_path).expect("state opens");
    let failed = store
        .create_init_run(acp_stack::state::NewInitRun {
            runtime_user: None,
            agent_id: Some("placeholder"),
            args_json: "{}",
        })
        .expect("failed run");
    let step = store
        .append_init_step(acp_stack::state::NewInitStep {
            run_id: &failed.id,
            ordinal: 2,
            kind: "agent_install",
            payload_json: "{}",
        })
        .expect("agent install step");
    store.mark_init_step_running(&step.id).expect("running");
    store
        .mark_init_step_failed(
            &step.id,
            None,
            "agent.installer_creates_missing",
            "missing",
            "{}",
        )
        .expect("failed step");
    store
        .finalize_init_run(&failed.id, acp_stack::state::INIT_RUN_FAILED)
        .expect("failed run finalize");
    let failed_id = failed.id.clone();
    drop(store);

    acps_command()
        .env("HOME", tempdir.path())
        .args(["init", "--resume", "--run-id", &failed_id])
        .assert()
        .failure();

    let store = acp_stack::state::StateStore::open(&state_path).expect("state opens");
    let reloaded = store
        .lookup_init_run(&failed_id)
        .expect("lookup")
        .expect("failed row should still exist");
    assert_eq!(reloaded.status, acp_stack::state::INIT_RUN_FAILED);
    let steps = store.query_init_steps(&failed_id).expect("steps");
    let install_step = steps
        .iter()
        .find(|step| step.kind == "agent_install")
        .expect("agent install step");
    assert_eq!(install_step.status, acp_stack::state::INIT_STEP_FAILED);
}

#[test]
fn init_resume_restores_recorded_agent_after_provider_secret_failure() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let workspace = tempdir.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace");

    acps_with_empty_path(tempdir.path())
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "opencode",
            "--provider",
            "openai",
            "--api-key-ref",
            "CUSTOM_OPENAI_API_KEY",
            "--workspace-root",
            workspace.to_str().expect("workspace UTF-8"),
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("CUSTOM_OPENAI_API_KEY"));

    let config_before =
        fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
            .expect("config should be readable");
    assert!(config_before.contains(r#"id = "opencode""#));

    seed_init_secrets(
        tempdir.path(),
        &[("CUSTOM_OPENAI_API_KEY", "test-openai-key")],
    );

    acps_with_empty_path(tempdir.path())
        .env("HOME", tempdir.path())
        .args(["init", "--resume"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent: OpenCode (opencode)"));

    let config_after =
        fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
            .expect("config should be readable");
    assert!(config_after.contains(r#"id = "opencode""#));
    assert!(config_after.contains(r#"id = "openai""#));
    assert!(config_after.contains(r#"api_key_ref = "CUSTOM_OPENAI_API_KEY""#));
    assert!(!config_after.contains(r#"api_key_ref = "OPENAI_API_KEY""#));
}

#[test]
fn init_resume_restores_recorded_custom_provider_args_after_secret_failure() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let workspace = tempdir.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace");

    acps_with_empty_path(tempdir.path())
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "opencode",
            "--provider",
            "myprovider",
            "--custom-provider",
            "--provider-name",
            "My Provider",
            "--base-url",
            "https://api.myprovider.example/v1",
            "--provider-api",
            "chat-completions",
            "--api-key-ref",
            "MY_PROVIDER_API_KEY",
            "--model",
            "my-model",
            "--model-name",
            "My Model",
            "--context",
            "123456",
            "--output-max-tokens",
            "12345",
            "--workspace-root",
            workspace.to_str().expect("workspace UTF-8"),
            "--skip-workspace-init",
            "--skip-testflight",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("MY_PROVIDER_API_KEY"));

    seed_init_secrets(
        tempdir.path(),
        &[("MY_PROVIDER_API_KEY", "test-provider-key")],
    );

    acps_with_empty_path(tempdir.path())
        .env("HOME", tempdir.path())
        .args(["init", "--resume"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent: OpenCode (opencode)"));

    let config_after =
        fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
            .expect("config should be readable");
    assert!(config_after.contains(r#"id = "myprovider""#));
    assert!(config_after.contains("[array.targets.agent.provider.custom]"));
    assert!(config_after.contains(r#"name = "My Provider""#));
    assert!(config_after.contains(r#"api_key_ref = "MY_PROVIDER_API_KEY""#));
    assert!(config_after.contains(r#"base_url = "https://api.myprovider.example/v1""#));
    assert!(config_after.contains(r#"api = "chat-completions""#));
    assert!(config_after.contains(r#"model_name = "My Model""#));
    assert!(config_after.contains("context = 123456"));
    assert!(config_after.contains("output_max_tokens = 12345"));
}

#[test]
fn init_resume_without_prior_run_errors_clearly() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    // No prior `acps init` — the resume target doesn't exist.
    acps_command()
        .env("HOME", tempdir.path())
        .args(["init", "--resume"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("no resumable init run"));
}

fn write_opencode_config(config_dir: &std::path::Path) {
    fs::create_dir_all(config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");
}

fn seed_named_secret(home: &std::path::Path, name: &str, value: &str) {
    let mut store = SecretStore::open_or_create(home).expect("secret store should open");
    store.set(name, value).expect("secret should be stored");
}

#[test]
fn agent_provider_credential_add_and_list_never_expose_values() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    write_opencode_config(&config_dir);
    seed_named_secret(tempdir.path(), "OPENCODE_API_KEY", "opencode-agent-key");
    seed_named_secret(
        tempdir.path(),
        "OPENAI_SOURCE",
        "sk-super-secret-openai-value",
    );

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "agent",
            "provider",
            "credential",
            "add",
            "openai",
            "--from-secret",
            "OPENAI_API_KEY=OPENAI_SOURCE",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("provider credential: added"));

    // Human list surfaces env names but never the credential value.
    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "provider", "credential", "list", "openai"])
        .assert()
        .success()
        .stdout(predicates::str::contains("OPENAI_API_KEY"))
        .stdout(predicates::str::contains("sk-super-secret-openai-value").not());

    // JSON list exposes source-ref names but neither values nor revisions.
    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "agent",
            "provider",
            "credential",
            "list",
            "openai",
            "--format",
            "json",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("OPENAI_SOURCE"))
        .stdout(predicates::str::contains("sk-super-secret-openai-value").not())
        .stdout(predicates::str::contains("revision").not());
}

#[test]
fn agent_provider_credential_select_blocks_deleting_selected_alias() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    write_opencode_config(&config_dir);
    seed_named_secret(tempdir.path(), "OPENCODE_API_KEY", "opencode-agent-key");
    seed_provider_credential(tempdir.path(), "openrouter", &["OPENROUTER_API_KEY"]);
    seed_named_secret(tempdir.path(), "OR_BACKUP_SOURCE", "or-backup-secret");

    // Select the aliasless provider as the primary lane, then promote it
    // (auto-selecting `primary` for the target that uses it).
    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "provider", "use", "openrouter"])
        .assert()
        .success();
    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "agent",
            "provider",
            "credential",
            "add",
            "openrouter",
            "--existing-alias",
            "primary",
            "--alias",
            "backup",
            "--from-secret",
            "OPENROUTER_API_KEY=OR_BACKUP_SOURCE",
        ])
        .assert()
        .success();

    // Switch the target's selection to `backup` via the select command.
    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "agent",
            "provider",
            "credential",
            "select",
            "openrouter",
            "backup",
        ])
        .assert()
        .success();

    // The selected alias cannot be deleted while a target still points at it.
    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "agent",
            "provider",
            "credential",
            "delete",
            "openrouter",
            "backup",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("selected by target"));
}

#[test]
fn agent_provider_set_active_prunes_selected_alias_for_dropped_provider() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    write_opencode_config(&config_dir);
    seed_named_secret(tempdir.path(), "OPENCODE_API_KEY", "opencode-agent-key");
    seed_provider_credential(tempdir.path(), "openai", &["OPENAI_API_KEY"]);
    seed_provider_credential(tempdir.path(), "openrouter", &["OPENROUTER_API_KEY"]);
    seed_named_secret(tempdir.path(), "OR_BACKUP_SOURCE", "or-backup-secret");

    // Establish a primary provider, activate both, then promote openrouter so
    // the target holds a selected alias for it.
    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "provider", "use", "openai"])
        .assert()
        .success();
    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "provider", "set-active", "openai,openrouter"])
        .assert()
        .success();
    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "agent",
            "provider",
            "credential",
            "add",
            "openrouter",
            "--existing-alias",
            "primary",
            "--alias",
            "backup",
            "--from-secret",
            "OPENROUTER_API_KEY=OR_BACKUP_SOURCE",
        ])
        .assert()
        .success();

    // Drop openrouter from the active set; its stale alias selection must be pruned.
    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "provider", "set-active", "openai"])
        .assert()
        .success();

    let config = fs::read_to_string(config_dir.join("acps-config.toml"))
        .expect("updated config should be readable");
    assert!(!config.contains("openrouter"));

    // With the selection pruned, the previously blocked alias can now be deleted.
    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "agent",
            "provider",
            "credential",
            "delete",
            "openrouter",
            "primary",
        ])
        .assert()
        .success();
}

#[test]
fn agent_provider_list_active_reports_configured_state_offline() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    write_opencode_config(&config_dir);
    seed_named_secret(tempdir.path(), "OPENCODE_API_KEY", "opencode-agent-key");
    seed_provider_credential(tempdir.path(), "openai", &["OPENAI_API_KEY"]);

    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "provider", "use", "openai"])
        .assert()
        .success();

    // No daemon is running, so live state is unknown but configured state resolves.
    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "provider", "list-active"])
        .assert()
        .success()
        .stdout(predicates::str::contains("configured: provider=openai"))
        .stdout(predicates::str::contains("loaded: unknown"));
}

#[test]
fn agent_set_provider_for_mapped_provider_redirects_to_provider_use() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    write_opencode_config(&config_dir);
    seed_named_secret(tempdir.path(), "OPENCODE_API_KEY", "opencode-agent-key");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "set", "--provider", "openai"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("acps agent provider use"));
}

#[test]
fn extensions_status_reports_none_declared() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["extensions", "status"])
        .assert()
        .success()
        .stdout(predicates::str::contains("extensions: none declared"));
}

#[test]
fn extensions_status_reports_managed_state_watermark_without_values() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config_text = format!(
        "{VALID_CONFIG}\n\
         [extensions.platform-state]\n\
         type = \"managed-state\"\n\
         capability = \"provider-credential\"\n"
    );
    fs::write(config_dir.join("acps-config.toml"), config_text).expect("config should be written");

    let secret_value = "sk-status-secret";
    {
        let mut store =
            SecretStore::open_or_create(tempdir.path()).expect("secret store should initialize");
        store
            .apply_managed_state_credential(
                "platform-state",
                "provider-credential",
                7,
                Some(acp_stack::secrets::ManagedCredentialSelection {
                    provider_id: "openai".to_owned(),
                    values: BTreeMap::from([(
                        "OPENAI_API_KEY".to_owned(),
                        secret_value.to_owned(),
                    )]),
                    source_refs: BTreeMap::new(),
                }),
            )
            .expect("managed apply should persist");
    }

    acps_command()
        .env("HOME", tempdir.path())
        .args(["extensions", "status"])
        .assert()
        .success()
        .stdout(predicates::str::contains("platform-state: managed-state"))
        .stdout(predicates::str::contains("applied_revision: 7"))
        .stdout(predicates::str::contains("provider: openai"))
        .stdout(predicates::str::contains(secret_value).not());

    acps_command()
        .env("HOME", tempdir.path())
        .args(["extensions", "status", "--format", "json"])
        .assert()
        .success()
        .stdout(predicates::str::contains("\"applied_revision\": 7"))
        .stdout(predicates::str::contains(secret_value).not());
}

#[test]
fn extensions_status_treats_missing_secret_store_as_no_watermark() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config_text = format!(
        "{VALID_CONFIG}\n\
         [extensions.platform-state]\n\
         type = \"managed-state\"\n\
         capability = \"provider-credential\"\n"
    );
    fs::write(config_dir.join("acps-config.toml"), config_text).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["extensions", "status"])
        .assert()
        .success()
        .stdout(predicates::str::contains("platform-state: managed-state"))
        .stdout(predicates::str::contains("applied_revision: none"));
}
