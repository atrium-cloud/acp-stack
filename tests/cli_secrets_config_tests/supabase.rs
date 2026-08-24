use crate::common::cli::*;
use acp_stack::config::load_config_from_str;
use acp_stack::secrets::SecretStore;
use acp_stack::state::StateStore;
use predicates::prelude::PredicateBooleanExt as _;
use serde_json::Value;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

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
    // anon/authenticated must be revoked behind a pg_roles existence guard,
    // never unconditionally, so the SQL stays safe on non-Supabase Postgres.
    assert!(sql.contains("FROM PUBLIC;"));
    assert!(sql.contains("IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = api_role_name)"));
    assert!(sql.contains("EXECUTE format('REVOKE ALL ON TABLE"));
    assert!(sql.contains("EXECUTE format('REVOKE ALL ON FUNCTION"));
    assert!(!sql.contains("FROM PUBLIC, \"anon\", \"authenticated\""));
    // Writes go through the SECURITY DEFINER ingest function, so the writer
    // role gets no direct table access.
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

    // A single quote would break out of the PL/pgSQL `format()` string literal
    // in the generated revoke statements, so reject it up front.
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
