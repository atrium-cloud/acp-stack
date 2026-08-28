use crate::common::cli::*;
use crate::support::*;
use acp_stack::secrets::SecretStore;
use axum::{Json, Router, routing::put};
use base64::Engine;
use http::StatusCode;
use predicates::prelude::PredicateBooleanExt as _;
use serde_json::{Value, json};
use std::fs;
use tokio::net::TcpListener;

#[test]
fn config_import_refuses_without_force_when_config_exists() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());

    let exported = acps_command(tempdir.path())
        .args(["config", "export"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let import_path = tempdir.path().join("exported.toml");
    fs::write(&import_path, exported).expect("write export");

    acps_command(tempdir.path())
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

    let modified =
        VALID_PLACEBO_CONFIG.replace(r#"bind = "127.0.0.1:7700""#, r#"bind = "127.0.0.1:7777""#);
    let import_path = tempdir.path().join("alt.toml");
    fs::write(&import_path, &modified).expect("write alt");

    acps_command(tempdir.path())
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

    acps_command(tempdir.path())
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

        acps_command(tempdir.path())
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

    let validate_output = acps_command(tempdir.path())
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

    let import_output = acps_command(tempdir.path())
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

    let output = acps_command(tempdir.path())
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

    acps_command(tempdir.path())
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

    acps_command(tempdir.path())
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

    acps_command(tempdir.path())
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

    acps_command(tempdir.path())
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

const ADAPTER_OVERRIDE_GITHUB_BLOCK: &str = r#"
[agent.adapter_override]
command = "placebo-acp"
github = "example/placebo-acp"

[agent.adapter_override.install.github]
asset_pattern = "placebo-acp-linux-{arch}.tar.gz"
archive = "tar.gz"
binary_name = "placebo-acp"

[agent.adapter_override.install.github.arch]
x86_64 = "x86_64"
aarch64 = "aarch64"
"#;

fn placebo_config_with_adapter_override() -> String {
    // The launch command doubles as the adapter identity, so [agent] command/args must
    // match the override block.
    let base = VALID_PLACEBO_CONFIG
        .replace(r#"command = "placebo-agent""#, r#"command = "placebo-acp""#)
        .replace(r#"args = ["acp"]"#, r#"args = []"#);
    format!("{}\n{ADAPTER_OVERRIDE_GITHUB_BLOCK}", base.trim_end())
}

#[test]
fn config_import_preserves_adapter_override_block() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let (_, admin_key) = run_init_with_home(tempdir.path());
    let import_path = tempdir.path().join("override.toml");
    fs::write(&import_path, placebo_config_with_adapter_override()).expect("write import");

    acps_command(tempdir.path())
        .args([
            "config",
            "import",
            import_path.to_str().unwrap(),
            "--force",
            "--admin-key",
            admin_key.as_str(),
        ])
        .assert()
        .success();

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config readable");
    let config = acp_stack::config::load_config_from_str(&written).expect("config parses");
    let override_config = config
        .agent
        .adapter_override
        .as_ref()
        .expect("override preserved through import");
    assert_eq!(override_config.command, "placebo-acp");
    assert!(override_config.install.github.is_some());
    assert_eq!(config.agent.command, "placebo-acp");
}

#[test]
fn init_from_file_preserves_adapter_override_with_same_agent_flag() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let import_path = tempdir.path().join("override-import.toml");
    fs::write(&import_path, placebo_config_with_adapter_override()).expect("write import");

    acps_command(tempdir.path())
        .args([
            "dev",
            "init",
            "--from-file",
            import_path.to_str().expect("path utf8"),
            "--agent",
            "placebo",
            "--non-interactive",
            "--skip-workspace-init",
            "--skip-testflight",
        ])
        .assert()
        .success();

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config readable");
    let config = acp_stack::config::load_config_from_str(&written).expect("config parses");
    assert!(config.agent.adapter_override.is_some());
    assert_eq!(config.agent.command, "placebo-acp");
}

#[test]
fn init_from_toml_imports_config_and_continues() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let modified =
        VALID_PLACEBO_CONFIG.replace(r#"bind = "127.0.0.1:7700""#, r#"bind = "127.0.0.1:7793""#);

    acps_command(tempdir.path())
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

    acps_command(tempdir.path())
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

    acps_command(tempdir.path())
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

    acps_command(tempdir.path())
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
    acps_command(tempdir.path())
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

    let output = acps_command(tempdir.path())
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
fn config_import_kilo_seeds_key_declaration_and_records_empty_placeholder() {
    // Kilo requires KILO_API_KEY present even with a non-Kilo provider, so the import
    // seeds the declaration and records the empty placeholder.
    let tempdir = tempfile::tempdir().expect("tempdir");
    let (_, admin_key) = run_init_with_home(tempdir.path());
    let mut store = SecretStore::open(tempdir.path()).expect("secret store should open");
    store
        .set("OPENROUTER_API_KEY", "test-openrouter-key")
        .expect("provider key should be stored");

    let kilo_config = VALID_PLACEBO_CONFIG
        .replace(r#"id = "placebo""#, r#"id = "kilo""#)
        .replace(r#"name = "Placebo Agent""#, r#"name = "Kilo Code""#)
        .replace(r#"command = "placebo-agent""#, r#"command = "kilo""#)
        .replace("env = []", r#"env = ["OPENROUTER_API_KEY"]"#);
    let import_path = tempdir.path().join("kilo.toml");
    fs::write(&import_path, &kilo_config).expect("write kilo config");

    acps_command(tempdir.path())
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
        .stdout(predicates::str::contains(
            "recorded empty KILO_API_KEY placeholder",
        ));

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config readable");
    let config = acp_stack::config::load_config_from_str(&written).expect("config should validate");
    assert_eq!(
        config.agent.env,
        vec!["OPENROUTER_API_KEY".to_owned(), "KILO_API_KEY".to_owned()],
        "import should seed the KILO_API_KEY declaration"
    );
    let store = SecretStore::open(tempdir.path()).expect("secret store should open");
    assert_eq!(store.get("KILO_API_KEY").expect("placeholder recorded"), "");
}

#[test]
fn config_import_dry_run_with_base64() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    run_init_with_home(tempdir.path());
    let config_path = tempdir.path().join(".config/acp-stack/acps-config.toml");
    let original_config = fs::read_to_string(&config_path).expect("config readable");

    let encoded = base64::engine::general_purpose::STANDARD.encode(VALID_PLACEBO_CONFIG);

    let output = acps_command(tempdir.path())
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

    acps_command(tempdir.path())
        .args(["config", "import", import_path.to_str().unwrap()])
        .assert()
        .failure()
        .stdout("")
        .stderr(predicates::str::contains("exceeds 1048576-byte size limit"));
}
