use crate::common::cli::*;
use crate::support::*;
use acp_stack::auth::{AuthVerifierSet, KeyKind};
use acp_stack::config::load_config_from_str;
use acp_stack::dev_gates::TEST_SKIP_AGENT_INSTALL_ENV;
use acp_stack::secrets::SecretStore;
use acp_stack::state::{StateStore, default_state_path};
use serde_json::{Value, json};
use std::fs;

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

    acps_command(tempdir.path())
        .env_remove(TEST_SKIP_AGENT_INSTALL_ENV)
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
    let output = acps_command(tempdir.path())
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

    let output = acps_command(tempdir.path())
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
    let output = acps_command(tempdir.path())
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
    let output = acps_command(tempdir.path())
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
    // Placebo settles no provider/model/mode/effort lane, so the selection
    // reports explicit nulls rather than omitting the keys.
    assert_eq!(
        body["selection"],
        json!({"provider": null, "model": null, "mode": null, "effort": null})
    );
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
fn init_handoff_json_reports_the_settled_selection() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir");
    let workspace = tempdir.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace dir");
    let config = VALID_CONFIG
        .replace(
            r#"root = "/workspace""#,
            &format!(r#"root = "{}""#, workspace.display()),
        )
        .replace(
            r#"uploads = "/workspace/uploads""#,
            &format!(r#"uploads = "{}/uploads""#, workspace.display()),
        )
        .replace(
            r#"cwd = "/workspace""#,
            &format!(r#"cwd = "{}""#, workspace.display()),
        )
        .replace(r#"command = "opencode""#, r#"command = "/bin/true""#);
    fs::write(config_dir.join("acps-config.toml"), config).expect("config");
    seed_init_secrets(tempdir.path(), &[("OPENAI_API_KEY", "test-openai-key")]);
    let options_path = write_acp_config_options_with_efforts(
        tempdir.path(),
        &["openai/gpt-5.5"],
        &["build", "plan"],
        &["low", "high"],
    );

    let output = acps_with_empty_path(tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "init",
            "--handoff-json",
            "--agent",
            "opencode",
            "--provider",
            "openai",
            "--api-key-ref",
            "OPENAI_API_KEY",
            "--model",
            "openai/gpt-5.5",
            "--mode",
            "plan",
            "--effort",
            "high",
            "--skip-testflight",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let body: Value = serde_json::from_slice(&output).expect("handoff json parses");
    assert_eq!(body["status"], "initialized");
    assert_eq!(body["selection"]["provider"], "openai");
    assert_eq!(body["selection"]["model"], "openai/gpt-5.5");
    assert_eq!(body["selection"]["mode"], "plan");
    assert_eq!(body["selection"]["effort"], "high");

    // The selection is read back from the written config, and a provider-backed
    // agent's model lives only in the provider slot: the value above proves the
    // payload read that slot rather than the cleared agent root.
    let written = load_config_from_str(
        &fs::read_to_string(config_dir.join("acps-config.toml")).expect("config readable"),
    )
    .expect("config parses");
    let provider = written.agent.provider.as_ref().expect("provider written");
    assert_eq!(provider.id, "openai");
    assert_eq!(provider.model.as_deref(), Some("openai/gpt-5.5"));
    assert!(written.agent.model.is_none());
}

#[test]
fn init_handoff_json_preserves_keys_without_reprinting_material() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let (session_key, admin_key) = run_init_with_home(tempdir.path());

    let output = acps_command(tempdir.path())
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

    let output = acps_command(tempdir.path())
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
    let output = acps_command(tempdir.path())
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
    assert!(body.get("selection").is_none(), "{body}");
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

    let output = acps_command(tempdir.path())
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
    let home = tempfile::tempdir().expect("home tempdir");
    acps_command(home.path())
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

    let stdout = acps_command(tempdir.path())
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

    let output = acps_command(tempdir.path())
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

    acps_command(tempdir.path())
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

    acps_command(tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("auth_keys.algorithm"));
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

    acps_command(tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("auth_keys.admin"));
}
