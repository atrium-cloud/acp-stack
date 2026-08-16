#![cfg(all(feature = "dev-tools", feature = "test-fixtures"))]

use acp_stack::state::{
    EVENT_SOURCE_CLI, INSTALLER_METHOD_GITHUB, INSTALLER_METHOD_NPM, INSTALLER_OPERATION_INSTALL,
    InstallerRunInput, StateStore, default_state_path,
};
use axum::{Json, Router, routing::get};
use base64::Engine;
use http::StatusCode;
use predicates::prelude::PredicateBooleanExt as _;
use serde_json::Value;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use tempfile::TempDir;
use tokio::net::UnixListener;
use tokio::task::JoinHandle;

mod common;
use common::cli::*;

struct HealthProbeHarness {
    socket_path: std::path::PathBuf,
    join: JoinHandle<std::io::Result<()>>,
    _tempdir: TempDir,
}

impl HealthProbeHarness {
    async fn spawn(status: StatusCode, body: Value) -> Self {
        let tempdir = TempDir::new().expect("tempdir");
        let socket_path = tempdir.path().join("probe.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind local probe");
        let app = Router::new().route(
            "/v1/health/ready",
            get(move || {
                let body = body.clone();
                async move { (status, Json(body)) }
            }),
        );
        let join = tokio::spawn(async move { axum::serve(listener, app).await });
        Self {
            socket_path,
            join,
            _tempdir: tempdir,
        }
    }
}

impl Drop for HealthProbeHarness {
    fn drop(&mut self) {
        self.join.abort();
    }
}

fn write_fake_agent_home(home: &std::path::Path, fake_args: &[&str]) {
    let config_dir = home.join(".config/acp-stack");
    let workspace = home.join("workspace");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::create_dir_all(&workspace).expect("workspace should be created");
    let mut args = vec!["acp"];
    args.extend_from_slice(fake_args);
    let args_toml = args
        .iter()
        .map(|arg| toml_string(arg))
        .collect::<Vec<_>>()
        .join(", ");
    let config = VALID_PLACEBO_CONFIG
        .replace(
            r#"root = "/workspace""#,
            &format!(r#"root = "{}""#, workspace.display()),
        )
        .replace(
            r#"uploads = "/workspace/uploads""#,
            &format!(r#"uploads = "{}/uploads""#, workspace.display()),
        )
        .replace(
            r#"command = "placebo-agent""#,
            &format!(
                "command = {}",
                toml_string(env!("CARGO_BIN_EXE_placebo-agent"))
            ),
        )
        .replace(r#"args = ["acp"]"#, &format!("args = [{args_toml}]"))
        .replace(
            r#"cwd = "/workspace""#,
            &format!("cwd = {}", toml_string(&workspace.to_string_lossy())),
        );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");
}

#[test]
fn prints_version() {
    let mut command = acps_command();

    command
        .arg("--version")
        .assert()
        .success()
        .stdout(predicates::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn security_check_is_listed_in_help() {
    acps_command()
        .args(["security", "--help"])
        .assert()
        .success()
        .stdout(predicates::str::contains("check"))
        .stdout(predicates::str::contains("runtime security self-check"));
}

#[test]
fn top_level_help_describes_common_subcommands() {
    acps_command()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "Initialize local config, secrets, workspace, and agent files",
        ))
        .stdout(predicates::str::contains(
            "Print daemon health and runtime status",
        ))
        .stdout(predicates::str::contains(
            "Rotate or inspect configured API key references",
        ))
        .stdout(predicates::str::contains(
            "Manage encrypted local secret values",
        ))
        .stdout(predicates::str::contains(
            "Validate, export, or import runtime config",
        ))
        .stdout(predicates::str::contains("Query durable runtime logs"))
        .stdout(predicates::str::contains(
            "Install, control, test, or configure the agent",
        ))
        .stdout(predicates::str::contains(
            "Configure OpenCode small-model behavior",
        ))
        .stdout(predicates::str::contains(
            "List, create, prompt, or close sessions",
        ))
        .stdout(predicates::str::contains("Run development-only workflows"))
        .stdout(predicates::str::contains(
            "acps config import acps-config.toml --dry-run",
        ))
        .stdout(predicates::str::contains("config import --path").not());
}

#[test]
fn config_help_uses_positional_import_path() {
    acps_command()
        .args(["config", "--help"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "acps config import acps-config.toml --dry-run",
        ))
        .stdout(predicates::str::contains("config import --path").not());
}

#[test]
fn validates_explicit_config_path() {
    let mut command = acps_command();

    command
        .args([
            "config",
            "validate",
            "tests/fixtures/valid-opencode-stack.toml",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("config is valid"));
}

#[test]
fn validate_failure_exits_nonzero_with_specific_error() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("invalid.toml");
    fs::write(
        &path,
        VALID_CONFIG.replace(r#"bind = "127.0.0.1:7700""#, r#"bind = "bad""#),
    )
    .expect("invalid config should be written");

    let mut command = acps_command();

    command
        .args([
            "config",
            "validate",
            path.to_str().expect("path should be UTF-8"),
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "api.bind must be a socket address",
        ))
        .stderr(predicates::str::contains(
            "hint: run the command with `--help` and correct the invalid input",
        ));
}

#[test]
fn exports_default_home_config_to_stdout() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    let mut command = acps_command();

    command
        .env("HOME", tempdir.path())
        .args(["config", "export"])
        .assert()
        .success()
        .stdout(predicates::str::contains("[api]"))
        .stdout(predicates::str::contains("[array.targets.agent.install]"))
        .stdout(predicates::str::contains(SESSION_KEY).not())
        .stdout(predicates::str::contains(ADMIN_KEY).not())
        .stdout(predicates::str::contains("sk-proj-exampleinlinevalue").not());
}

#[test]
fn exports_base64_default_home_config() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    let mut command = acps_command();
    let output = command
        .env("HOME", tempdir.path())
        .args(["config", "export", "--base64"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let encoded = String::from_utf8(output).expect("stdout should be UTF-8");
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .expect("stdout should be base64 TOML");
    let toml = String::from_utf8(decoded).expect("decoded TOML should be UTF-8");

    assert!(toml.contains("[api]"));
    assert!(toml.contains("[array.targets.agent.install]"));
}

#[test]
fn exports_default_home_config_to_output_path() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");
    let output_path = tempdir.path().join("exported.toml");

    let mut command = acps_command();

    command
        .env("HOME", tempdir.path())
        .args([
            "config",
            "export",
            "--output",
            output_path.to_str().expect("path should be UTF-8"),
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("progress: loading config"))
        .stdout(predicates::str::contains(
            "progress: rendering config export",
        ))
        .stdout(predicates::str::contains("progress: writing config export"));

    let exported = fs::read_to_string(output_path).expect("export should be readable");
    assert!(exported.contains("[api]"));
    assert!(exported.contains("[array.targets.agent.install]"));
}

#[test]
fn array_add_uses_canonical_agent_id_as_target() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config_path = config_dir.join("acps-config.toml");
    fs::write(&config_path, VALID_CONFIG).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["array", "add", "codex"])
        .assert()
        .success()
        .stdout(predicates::str::contains("array target added: codex"));

    let config: toml::Value = toml::from_str(
        &fs::read_to_string(config_path).expect("updated config should be readable"),
    )
    .expect("config should parse");
    assert_eq!(config["array"]["primary_target"].as_str(), Some("opencode"));
    assert_eq!(config["array"]["targets"][1]["id"].as_str(), Some("codex"));
    assert_eq!(
        config["array"]["targets"][1]["agent"]["id"].as_str(),
        Some("codex")
    );
}

#[test]
fn array_add_rejects_noncanonical_agent_alias() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["array", "add", "claude"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("claude"));
}

#[test]
fn array_set_supports_target_custom_provider() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config_path = config_dir.join("acps-config.toml");
    fs::write(&config_path, VALID_CONFIG).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "array",
            "set",
            "--target",
            "opencode",
            "--custom-provider",
            "--provider",
            "custom-openai",
            "--provider-name",
            "Custom OpenAI",
            "--base-url",
            "https://llm.example.test/v1",
            "--model",
            "custom/model",
            "--model-name",
            "Custom Model",
            "--context",
            "1234",
            "--output-max-tokens",
            "567",
            "--api-key-ref",
            "CUSTOM_OPENAI_KEY",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("array target set: opencode"))
        .stdout(predicates::str::contains("provider: custom-openai"));

    let config: toml::Value = toml::from_str(
        &fs::read_to_string(config_path).expect("updated config should be readable"),
    )
    .expect("config should parse");
    let provider = &primary_array_agent_value(&config)["provider"];
    assert_eq!(provider["id"].as_str(), Some("custom-openai"));
    assert_eq!(provider["model"].as_str(), Some("custom/model"));
    assert_eq!(provider["api_key_ref"].as_str(), Some("CUSTOM_OPENAI_KEY"));
    assert_eq!(
        provider["custom"]["base_url"].as_str(),
        Some("https://llm.example.test/v1")
    );
    assert_eq!(provider["custom"]["context"].as_integer(), Some(1234));
    assert_eq!(
        provider["custom"]["output_max_tokens"].as_integer(),
        Some(567)
    );
}

#[test]
fn agent_default_set_updates_primary_target_only() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config_path = config_dir.join("acps-config.toml");
    fs::write(&config_path, VALID_CONFIG).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["array", "add", "codex"])
        .assert()
        .success();
    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "default", "set", "codex"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent default: codex"));

    let config: toml::Value = toml::from_str(
        &fs::read_to_string(config_path).expect("updated config should be readable"),
    )
    .expect("config should parse");
    assert_eq!(config["array"]["primary_target"].as_str(), Some("codex"));
    assert_eq!(
        config["array"]["targets"][0]["id"].as_str(),
        Some("opencode")
    );
    assert_eq!(config["array"]["targets"][1]["id"].as_str(), Some("codex"));
}

#[test]
fn array_on_and_off_toggle_enabled_flag() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config_path = config_dir.join("acps-config.toml");
    fs::write(&config_path, VALID_CONFIG).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["array", "on"])
        .assert()
        .success()
        .stdout(predicates::str::contains("array: on"));
    let after_on: toml::Value =
        toml::from_str(&fs::read_to_string(&config_path).expect("config should be readable"))
            .expect("config should parse");
    assert_eq!(after_on["array"]["enabled"].as_bool(), Some(true));

    acps_command()
        .env("HOME", tempdir.path())
        .args(["array", "off"])
        .assert()
        .success()
        .stdout(predicates::str::contains("array: off"));
    let after_off: toml::Value =
        toml::from_str(&fs::read_to_string(&config_path).expect("config should be readable"))
            .expect("config should parse");
    assert_eq!(after_off["array"]["enabled"].as_bool(), Some(false));
}

#[test]
fn array_start_rejects_non_default_target_when_array_is_off() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config_path = config_dir.join("acps-config.toml");
    fs::write(&config_path, VALID_CONFIG).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["array", "add", "codex"])
        .assert()
        .success();
    acps_command()
        .env("HOME", tempdir.path())
        .args(["array", "start", "--target", "codex"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("Array mode is off"));
}

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
        .stdout(predicates::str::contains("schema=23"))
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

#[test]
fn agent_check_reports_no_runs_when_state_is_empty() {
    // Without successful installer_runs the check command should report the
    // expected native install step as missing without hitting the network.
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    acps_command_without_placebo()
        .env("HOME", tempdir.path())
        .args(["agent", "check"])
        .assert()
        .failure()
        .stdout(predicates::str::contains("install: not installed"));
}

#[test]
fn agent_check_format_json_reports_steps() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    let output = acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "check", "--format", "json"])
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();
    let body: Value = serde_json::from_slice(&output).expect("agent check json parses");
    assert_eq!(body["agent"], "opencode");
    assert_eq!(body["ok"], false);
    assert_eq!(body["steps"][0]["step"], "install");
    assert_eq!(body["steps"][0]["result"]["status"], "not_installed");
}

#[test]
fn agent_check_reports_missing_adapter_step() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), amp_config()).expect("config should be written");

    let state_path = default_state_path(tempdir.path());
    fs::create_dir_all(state_path.parent().expect("state parent dir"))
        .expect("state dir should be created");
    let store = StateStore::open(&state_path).expect("state should open");
    store.migrate().expect("migration should pass");
    store
        .append_installer_run(InstallerRunInput {
            agent_id: "amp",
            started_at: "2026-05-22T00:00:00.000000000Z",
            finished_at: Some("2026-05-22T00:00:01.000000000Z"),
            status: "ran",
            stdout: "",
            stderr: "",
            exit_status: Some(0),
            step: "harness",
            version: None,
            operation: INSTALLER_OPERATION_INSTALL,
            method: None,
            log_dir: None,
            apply_run_id: None,
        })
        .expect("seed harness row");
    drop(store);

    acps_command_without_placebo()
        .env("HOME", tempdir.path())
        .args(["agent", "check"])
        .assert()
        .failure()
        .stdout(predicates::str::contains("harness: unknown"))
        .stdout(predicates::str::contains("adapter: not installed"));
}

#[test]
fn installer_history_reports_empty_state_when_nothing_recorded() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    fs::create_dir_all(tempdir.path().join(".config/acp-stack"))
        .expect("config dir should be created");
    fs::write(
        tempdir.path().join(".config/acp-stack/acps-config.toml"),
        VALID_CONFIG,
    )
    .expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["installer", "history"])
        .assert()
        .success()
        .stdout(predicates::str::contains("no installer runs recorded"));
}

#[test]
fn installer_history_renders_rows_with_filter() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    fs::create_dir_all(tempdir.path().join(".config/acp-stack"))
        .expect("config dir should be created");
    fs::write(
        tempdir.path().join(".config/acp-stack/acps-config.toml"),
        VALID_CONFIG,
    )
    .expect("config should be written");

    let state_path = default_state_path(tempdir.path());
    fs::create_dir_all(state_path.parent().expect("state parent dir"))
        .expect("state dir should be created");
    let store = StateStore::open(&state_path).expect("state should open");
    store.migrate().expect("migration should pass");
    store
        .append_installer_run(InstallerRunInput {
            agent_id: "opencode",
            started_at: "2026-05-22T00:00:00.000000000Z",
            finished_at: Some("2026-05-22T00:00:00.250000000Z"),
            status: "ran",
            stdout: "",
            stderr: "",
            exit_status: Some(0),
            step: "harness",
            version: Some("v1.0.0"),
            operation: INSTALLER_OPERATION_INSTALL,
            method: Some(INSTALLER_METHOD_GITHUB),
            log_dir: None,
            apply_run_id: None,
        })
        .expect("seed harness row");
    store
        .append_installer_run(InstallerRunInput {
            agent_id: "codex",
            started_at: "2026-05-22T00:00:01.000000000Z",
            finished_at: Some("2026-05-22T00:00:02.000000000Z"),
            status: "failed",
            stdout: "",
            stderr: "boom",
            exit_status: Some(2),
            step: "adapter",
            version: None,
            operation: INSTALLER_OPERATION_INSTALL,
            method: Some(INSTALLER_METHOD_GITHUB),
            log_dir: None,
            apply_run_id: None,
        })
        .expect("seed adapter row");
    drop(store);

    // No filter: both rows visible, newest (codex) first.
    acps_command()
        .env("HOME", tempdir.path())
        .args(["installer", "history"])
        .assert()
        .success()
        .stdout(predicates::str::contains("started_at"))
        .stdout(predicates::str::contains("codex"))
        .stdout(predicates::str::contains("opencode"))
        .stdout(predicates::str::contains("v1.0.0"))
        .stdout(predicates::str::contains("failed"));

    // Filter to opencode: only the harness row should appear.
    acps_command()
        .env("HOME", tempdir.path())
        .args(["installer", "history", "--agent", "opencode"])
        .assert()
        .success()
        .stdout(predicates::str::contains("opencode"))
        .stdout(predicates::str::contains("v1.0.0"))
        .stdout(predicates::str::contains("codex").not());
}

#[test]
fn installer_history_format_json_renders_runs() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    fs::create_dir_all(tempdir.path().join(".config/acp-stack"))
        .expect("config dir should be created");
    fs::write(
        tempdir.path().join(".config/acp-stack/acps-config.toml"),
        VALID_CONFIG,
    )
    .expect("config should be written");

    let state_path = default_state_path(tempdir.path());
    fs::create_dir_all(state_path.parent().expect("state parent dir"))
        .expect("state dir should be created");
    let store = StateStore::open(&state_path).expect("state should open");
    store.migrate().expect("migration should pass");
    store
        .append_installer_run(InstallerRunInput {
            agent_id: "opencode",
            started_at: "2026-05-22T01:00:00.000000000Z",
            finished_at: Some("2026-05-22T01:00:01.000000000Z"),
            status: "ran",
            stdout: "hi",
            stderr: "",
            exit_status: Some(0),
            step: "harness",
            version: Some("v1.0.0"),
            operation: INSTALLER_OPERATION_INSTALL,
            method: Some(INSTALLER_METHOD_GITHUB),
            log_dir: Some("/tmp/installer-logs/opencode/harness"),
            apply_run_id: None,
        })
        .expect("seed row");
    drop(store);

    let output = acps_command()
        .env("HOME", tempdir.path())
        .args(["installer", "history", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let body: Value = serde_json::from_slice(&output).expect("installer json parses");
    let runs = body["runs"].as_array().expect("runs should be an array");
    assert_eq!(runs.len(), 1, "{body}");
    assert_eq!(runs[0]["agent_id"], "opencode");
    assert_eq!(runs[0]["duration_ms"], 1_000);
}

#[test]
fn installer_history_renders_log_dir_continuation_line() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    fs::create_dir_all(tempdir.path().join(".config/acp-stack"))
        .expect("config dir should be created");
    fs::write(
        tempdir.path().join(".config/acp-stack/acps-config.toml"),
        VALID_CONFIG,
    )
    .expect("config should be written");

    let state_path = default_state_path(tempdir.path());
    fs::create_dir_all(state_path.parent().expect("state parent dir"))
        .expect("state dir should be created");
    let store = StateStore::open(&state_path).expect("state should open");
    store.migrate().expect("migration should pass");
    store
        .append_installer_run(InstallerRunInput {
            agent_id: "opencode",
            started_at: "2026-05-22T01:00:00.000000000Z",
            finished_at: Some("2026-05-22T01:00:01.000000000Z"),
            status: "ran",
            stdout: "hi",
            stderr: "",
            exit_status: Some(0),
            step: "harness",
            version: Some("v1.0.0"),
            operation: INSTALLER_OPERATION_INSTALL,
            method: Some(INSTALLER_METHOD_GITHUB),
            log_dir: Some("/tmp/installer-logs/opencode/2026-05-22T01:00:00.000000000Z/harness"),
            apply_run_id: None,
        })
        .expect("seed row with log_dir");
    drop(store);

    acps_command()
        .env("HOME", tempdir.path())
        .args(["installer", "history"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "log_dir: /tmp/installer-logs/opencode/2026-05-22T01:00:00.000000000Z/harness",
        ));
}

#[test]
fn installer_history_rejects_zero_limit() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    fs::create_dir_all(tempdir.path().join(".config/acp-stack"))
        .expect("config dir should be created");
    fs::write(
        tempdir.path().join(".config/acp-stack/acps-config.toml"),
        VALID_CONFIG,
    )
    .expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["installer", "history", "--limit", "0"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("limit must be"));
}

#[test]
fn deps_apply_prints_before_and_after_status() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");

    let dependency_one_name = "deps-apply-before-after-marker-one";
    let dependency_two_name = "deps-apply-before-after-marker-two";
    let feature = "deps-apply-before-after";
    let marker_one = tempdir.path().join("deps-apply-marker-one");
    let marker_two = tempdir.path().join("deps-apply-marker-two");
    let shell_one = format!(
        "printf '#!/bin/sh\\nexit 0\\n' > {marker} && chmod 755 {marker}",
        marker = shell_quote_path(&marker_one),
    );
    let shell_two = format!(
        "printf '#!/bin/sh\\nexit 0\\n' > {marker} && chmod 755 {marker}",
        marker = shell_quote_path(&marker_two),
    );
    let config = VALID_CONFIG.replace(
        "[agent]",
        &format!(
            r#"[[dependencies.commands]]
	name = "{dependency_one_name}"
	required = true
	feature = "{feature}"
	
	[dependencies.commands.install]
	shell = {}
	creates = {}

[[dependencies.commands]]
	name = "{dependency_two_name}"
	required = true
	feature = "{feature}"
	
	[dependencies.commands.install]
	shell = {}
	creates = {}
	
	[agent]"#,
            toml_string(&shell_one),
            toml_string(&marker_one.to_string_lossy()),
            toml_string(&shell_two),
            toml_string(&marker_two.to_string_lossy()),
        ),
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    let state_path = default_state_path(tempdir.path());
    fs::create_dir_all(state_path.parent().expect("state parent dir"))
        .expect("state dir should be created");
    let store = StateStore::open(&state_path).expect("state should open");
    store.migrate().expect("migration should pass");
    drop(store);
    seed_auth_verifiers(tempdir.path(), SESSION_KEY, ADMIN_KEY);

    let output = acps_command()
        .env("HOME", tempdir.path())
        .args([
            "deps",
            "apply",
            "--yes",
            "--feature",
            feature,
            "--admin-key",
            ADMIN_KEY,
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("stdout should be utf8");

    let before_index = stdout.find("before:\n").expect("before section");
    let progress_one_index = stdout
        .find(&format!(
            "progress: applying dependency 1/2: {dependency_one_name}\n"
        ))
        .expect("first progress line");
    let progress_two_index = stdout
        .find(&format!(
            "progress: applying dependency 2/2: {dependency_two_name}\n"
        ))
        .expect("second progress line");
    let results_index = stdout.find("results:\n").expect("results section");
    let after_index = stdout.find("after:\n").expect("after section");
    let audit_index = stdout.find("audit run: dap_").expect("audit run line");
    assert!(
        progress_one_index < progress_two_index
            && progress_two_index < before_index
            && before_index < results_index
            && results_index < after_index
            && after_index < audit_index,
        "expected before/results/after ordering, got:\n{stdout}",
    );
    assert!(
        stdout[before_index..results_index].contains(&format!("  MISS {dependency_one_name}")),
        "before section must report missing dependency, got:\n{stdout}",
    );
    assert!(
        stdout[before_index..results_index].contains(&format!("  MISS {dependency_two_name}")),
        "before section must report missing dependency, got:\n{stdout}",
    );
    assert!(
        stdout[after_index..].contains(&format!("  OK   {dependency_one_name}")),
        "after section must report available dependency, got:\n{stdout}",
    );
    assert!(
        stdout[after_index..].contains(&format!("  OK   {dependency_two_name}")),
        "after section must report available dependency, got:\n{stdout}",
    );
}

#[test]
fn deps_apply_exits_nonzero_and_prints_manual_commands_on_privilege_skip() {
    // SAFETY: `geteuid()` is always safe — no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        // As root the escalation probe short-circuits to "run directly"
        // and the skip path under test is unreachable.
        return;
    }
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");

    // Deterministic "no passwordless sudo": a fake sudo that exits 1, first
    // on PATH, collapses the escalation probe to Unavailable.
    let fake_bin = tempdir.path().join("fake-bin");
    fs::create_dir_all(&fake_bin).expect("fake bin dir");
    let fake_sudo = fake_bin.join("sudo");
    fs::write(&fake_sudo, "#!/bin/sh\nexit 1\n").expect("fake sudo");
    fs::set_permissions(&fake_sudo, fs::Permissions::from_mode(0o755)).expect("chmod fake sudo");
    let host_path = std::env::var("PATH").expect("PATH should be set");
    let path_with_fake_sudo = format!("{}:{host_path}", fake_bin.to_string_lossy());

    let dependency_name = "deps-apply-privilege-skip-marker";
    let config = VALID_CONFIG.replace(
        "[agent]",
        &format!(
            r#"[[dependencies.commands]]
	name = "{dependency_name}"
	required = true

	[dependencies.commands.install]
	shell = "exit 0"
	creates = "{dependency_name}"
	scope = "system"

	[agent]"#,
        ),
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    let state_path = default_state_path(tempdir.path());
    fs::create_dir_all(state_path.parent().expect("state parent dir"))
        .expect("state dir should be created");
    let store = StateStore::open(&state_path).expect("state should open");
    store.migrate().expect("migration should pass");
    drop(store);
    seed_auth_verifiers(tempdir.path(), SESSION_KEY, ADMIN_KEY);

    // Unlike init (which skips and continues), the explicit imperative
    // command must exit non-zero and hand the operator the manual commands.
    acps_command()
        .env("HOME", tempdir.path())
        .env("PATH", path_with_fake_sudo)
        .args(["deps", "apply", "--yes", "--admin-key", ADMIN_KEY])
        .assert()
        .failure()
        .stdout(predicates::str::contains(
            "no passwordless sudo; they will be skipped and recorded as privilege_required",
        ))
        .stdout(predicates::str::contains(format!(
            "privreq     {dependency_name}"
        )))
        .stdout(predicates::str::contains("sudo /bin/bash -c 'exit 0'"))
        .stderr(predicates::str::contains("need root privilege"));

    let store = StateStore::open(&state_path).expect("state should reopen");
    let rows = store
        .query_installer_runs_filtered(Some("deps_apply"), 10)
        .expect("installer history should query");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].status, "privilege_required");
}

#[test]
fn deps_apply_requires_admin_key() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["deps", "apply", "--yes"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--admin-key"));
}

#[test]
fn deps_check_format_json_reports_dependency_shape() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");

    let config = VALID_CONFIG.replace(
        "[agent]",
        r#"[[dependencies.commands]]
name = "deps-check-json"
required = true

[agent]"#,
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    let output = acps_command()
        .env("HOME", tempdir.path())
        .args(["deps", "check", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let body: Value = serde_json::from_slice(&output).expect("deps check json parses");
    let deps = body["dependencies"]
        .as_array()
        .expect("dependencies should be an array");
    assert_eq!(deps[0]["name"], "deps-check-json");
    assert_eq!(deps[0]["available"], false);
}

#[test]
fn deps_apply_format_json_omits_stderr_tail() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");

    let marker = tempdir.path().join("deps-apply-failed-marker");
    let config = VALID_CONFIG.replace(
        "[agent]",
        &format!(
            r#"[[dependencies.commands]]
name = "deps-apply-json-failure"
required = true

[dependencies.commands.install]
shell = "printf 'token sk-test-secret' >&2; exit 7"
creates = {}

[agent]"#,
            toml_string(&marker.to_string_lossy()),
        ),
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    let state_path = default_state_path(tempdir.path());
    fs::create_dir_all(state_path.parent().expect("state parent dir"))
        .expect("state dir should be created");
    let store = StateStore::open(&state_path).expect("state should open");
    store.migrate().expect("migration should pass");
    drop(store);
    seed_auth_verifiers(tempdir.path(), SESSION_KEY, ADMIN_KEY);

    let output = acps_command()
        .env("HOME", tempdir.path())
        .args([
            "deps",
            "apply",
            "--yes",
            "--format",
            "json",
            "--admin-key",
            ADMIN_KEY,
        ])
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8_lossy(&output);
    assert!(!stdout.contains("running dependency install actions"));
    assert!(!stdout.contains("progress: applying dependency"));
    assert!(!stdout.contains("sk-test-secret"));
    let body: Value = serde_json::from_slice(&output).expect("deps apply json parses");
    assert!(
        body["apply_run_id"]
            .as_str()
            .is_some_and(|value| value.starts_with("dap_")),
        "{body}",
    );
    let outcome = &body["results"][0]["outcome"];
    assert_eq!(outcome["kind"], "failed");
    assert_eq!(outcome["exit_code"], 7);
    assert_eq!(outcome["stderr_tail_omitted"], true);
    assert!(outcome.get("stderr_tail").is_none(), "{body}");
}

#[test]
fn deps_apply_persists_one_apply_run_id_for_all_rows() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");

    let installed_marker = tempdir.path().join("deps-apply-installed-marker");
    let skipped_marker = tempdir.path().join("deps-apply-skipped-marker");
    fs::write(&skipped_marker, "#!/bin/sh\nexit 0\n").expect("skipped marker should be written");
    #[cfg(unix)]
    fs::set_permissions(&skipped_marker, fs::Permissions::from_mode(0o755))
        .expect("skipped marker should be executable");
    let shell = format!(
        "printf '#!/bin/sh\\nexit 0\\n' > {marker} && chmod 755 {marker}",
        marker = shell_quote_path(&installed_marker),
    );
    let config = VALID_CONFIG.replace(
        "[agent]",
        &format!(
            r#"[[dependencies.commands]]
name = "deps-apply-installed"
required = true

[dependencies.commands.install]
shell = {}
creates = {}

[[dependencies.commands]]
name = "deps-apply-skipped"
required = true

[dependencies.commands.install]
shell = "exit 99"
creates = {}

[agent]"#,
            toml_string(&shell),
            toml_string(&installed_marker.to_string_lossy()),
            toml_string(&skipped_marker.to_string_lossy()),
        ),
    );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");

    let state_path = default_state_path(tempdir.path());
    fs::create_dir_all(state_path.parent().expect("state parent dir"))
        .expect("state dir should be created");
    let store = StateStore::open(&state_path).expect("state should open");
    store.migrate().expect("migration should pass");
    drop(store);
    seed_auth_verifiers(tempdir.path(), SESSION_KEY, ADMIN_KEY);

    acps_command()
        .env("HOME", tempdir.path())
        .args(["deps", "apply", "--yes", "--admin-key", ADMIN_KEY])
        .assert()
        .success();

    let store = StateStore::open(&state_path).expect("state should open");
    let rows = store
        .query_installer_runs_filtered(Some("deps_apply"), 10)
        .expect("deps rows should query");
    assert_eq!(
        rows.len(),
        2,
        "expected one row per declared install action"
    );
    let apply_run_id = rows[0]
        .apply_run_id
        .as_deref()
        .expect("apply_run_id should be present");
    assert!(
        apply_run_id.starts_with("dap_"),
        "apply_run_id should use the deps apply prefix, got {apply_run_id}"
    );
    assert!(
        rows.iter()
            .all(|row| row.apply_run_id.as_deref() == Some(apply_run_id)),
        "all rows from one invocation must share apply_run_id, got {rows:?}"
    );
    assert!(rows.iter().any(|row| row.status == "installed"));
    assert!(rows.iter().any(|row| row.status == "skipped"));
}

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

#[cfg(unix)]
#[test]
fn status_creates_owner_only_state_when_config_exists_without_state() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config_path = config_dir.join("acps-config.toml");
    fs::write(&config_path, VALID_CONFIG).expect("config should be written");
    fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o755))
        .expect("config dir permissions should be set");
    fs::set_permissions(&config_path, fs::Permissions::from_mode(0o644))
        .expect("config file permissions should be set");

    acps_command()
        .env("HOME", tempdir.path())
        .arg("status")
        .assert()
        .success();

    let state_dir = tempdir.path().join(".local/share/acp-stack");
    let state_path = state_dir.join("state.sqlite");
    assert_eq!(mode(&config_dir), 0o700);
    assert_eq!(mode(&config_path), 0o600);
    assert_eq!(mode(&state_dir), 0o700);
    assert_eq!(mode(&state_path), 0o600);
}

#[cfg(unix)]
#[test]
fn status_repairs_config_permissions_before_validation_failure() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config_path = config_dir.join("acps-config.toml");
    fs::write(
        &config_path,
        VALID_CONFIG.replace(r#"bind = "127.0.0.1:7700""#, r#"bind = "bad""#),
    )
    .expect("invalid config should be written");
    fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o755))
        .expect("config dir permissions should be set");
    fs::set_permissions(&config_path, fs::Permissions::from_mode(0o644))
        .expect("config file permissions should be set");

    acps_command()
        .env("HOME", tempdir.path())
        .arg("status")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "api.bind must be a socket address",
        ));

    assert_eq!(mode(&config_dir), 0o700);
    assert_eq!(mode(&config_path), 0o600);
}

#[test]
fn logs_query_shows_init_event() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success();

    let mut command = acps_command();
    command
        .env("HOME", tempdir.path())
        .args(["logs", "query"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "info cli init.completed initialized",
        ));
}

#[cfg(unix)]
#[test]
fn logs_query_creates_owner_only_empty_state_when_missing() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["logs", "query"])
        .assert()
        .success()
        .stdout("");

    let state_dir = tempdir.path().join(".local/share/acp-stack");
    let state_path = state_dir.join("state.sqlite");
    assert_eq!(mode(&state_dir), 0o700);
    assert_eq!(mode(&state_path), 0o600);
}

#[test]
fn logs_query_supports_limit_and_level_filter() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success();
    acps_command()
        .env("HOME", tempdir.path())
        .arg("status")
        .assert()
        .success();

    let mut limit_command = acps_command();
    limit_command
        .env("HOME", tempdir.path())
        .args(["logs", "query", "--limit", "1"])
        .assert()
        .success()
        .stdout(predicates::str::contains("status.checked").count(1));

    let mut level_command = acps_command();
    level_command
        .env("HOME", tempdir.path())
        .args(["logs", "query", "--level", "error"])
        .assert()
        .success()
        .stdout("");
}

#[test]
fn logs_query_json_emits_envelope_with_cursor() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success();
    acps_command()
        .env("HOME", tempdir.path())
        .arg("status")
        .assert()
        .success();

    let output = acps_command()
        .env("HOME", tempdir.path())
        .args(["logs", "query", "--limit", "1", "--json"])
        .output()
        .expect("acps logs query --json should execute");
    assert!(
        output.status.success(),
        "exit status: {:?}, stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout must be valid JSON");
    let events = parsed
        .get("events")
        .and_then(|v| v.as_array())
        .expect("events array present");
    assert_eq!(events.len(), 1, "limit=1 must return exactly one event");
    let event = &events[0];
    for field in [
        "id",
        "created_at",
        "level",
        "kind",
        "message",
        "payload_json",
        "source",
    ] {
        assert!(
            event.get(field).is_some(),
            "event JSON missing field `{field}`: {event}"
        );
    }
    let cursor = parsed
        .get("next_cursor")
        .expect("next_cursor key present even when null")
        .as_str()
        .expect("next_cursor populated when page saturates limit");
    assert!(
        !cursor.is_empty(),
        "next_cursor must be a non-empty id when limit=1 saturates"
    );

    let stderr = String::from_utf8(output.stderr).expect("stderr is utf8");
    assert!(
        !stderr.contains("-- more rows available"),
        "JSON mode must suppress the human cursor hint, got: {stderr}"
    );
}

#[test]
fn logs_query_global_format_json_matches_json_alias() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success();

    let output = acps_command()
        .env("HOME", tempdir.path())
        .args(["logs", "query", "--limit", "1", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let parsed: Value = serde_json::from_slice(&output).expect("format json should parse");
    assert!(parsed["events"].as_array().is_some(), "{parsed}");
    assert!(parsed.get("next_cursor").is_some(), "{parsed}");
}

#[test]
fn logs_query_json_alias_conflicts_with_explicit_text_format() {
    acps_command()
        .args(["logs", "query", "--json", "--format", "text"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "--json conflicts with --format text",
        ));
}

#[test]
fn logs_tail_rejects_format_json_before_loading_config() {
    acps_command()
        .args(["logs", "tail", "--format", "json"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "logs tail does not support --format json",
        ));
}

#[test]
fn text_only_commands_reject_format_json_before_loading_config() {
    acps_command()
        .args(["subagent", "status", "--format", "json"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "subagent does not support --format json",
        ));
}

#[test]
fn completion_scripts_include_root_and_common_commands() {
    for shell in ["bash", "zsh", "fish", "powershell", "elvish"] {
        let output = acps_command()
            .args(["completion", shell])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let stdout = String::from_utf8(output).expect("completion is utf8");
        assert!(
            stdout.contains("acps"),
            "{shell} completion missing binary name"
        );
        assert!(
            stdout.contains("sessions"),
            "{shell} completion missing sessions"
        );
        assert!(
            stdout.contains("completion"),
            "{shell} completion missing completion command"
        );
    }
}

#[test]
fn completion_rejects_format_json() {
    acps_command()
        .args(["completion", "bash", "--format", "json"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "completion does not support --format json",
        ));
}

#[test]
fn failed_cli_command_records_error_after_state_exists() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success();

    let config_path = tempdir.path().join(".config/acp-stack/acps-config.toml");
    fs::write(
        config_path,
        VALID_CONFIG.replace(r#"bind = "127.0.0.1:7700""#, r#"bind = "bad""#),
    )
    .expect("invalid config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .arg("status")
        .assert()
        .failure();

    let mut logs_command = acps_command();
    logs_command
        .env("HOME", tempdir.path())
        .args(["logs", "query", "--level", "error"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "error cli cli.error command failed",
        ));
}

#[test]
fn parse_failure_records_error_after_state_exists() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success();

    acps_command()
        .env("HOME", tempdir.path())
        .arg("unknown-command")
        .assert()
        .failure();

    let mut logs_command = acps_command();
    logs_command
        .env("HOME", tempdir.path())
        .args(["logs", "query", "--level", "error"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "error cli cli.error command failed",
        ));
}

#[test]
fn help_invocations_do_not_record_error_events() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success();

    acps_command()
        .env("HOME", tempdir.path())
        .arg("--help")
        .assert()
        .success();

    acps_command()
        .env("HOME", tempdir.path())
        .arg("--version")
        .assert()
        .success();

    acps_command()
        .env("HOME", tempdir.path())
        .args(["logs", "--help"])
        .assert()
        .success();

    acps_command()
        .env("HOME", tempdir.path())
        .args(["logs", "query", "--help"])
        .assert()
        .success();

    acps_command()
        .env("HOME", tempdir.path())
        .args(["logs", "query", "--level", "error"])
        .assert()
        .success()
        .stdout("");
}

#[cfg(unix)]
#[test]
fn cli_error_payload_handles_control_bytes_in_argument() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success();

    // Path that mixes a stray ANSI escape sequence and a bare control byte. The runtime
    // must strip ANSI, encode the remaining bytes via serde_json, and still produce a
    // valid JSON payload that survives json_valid() in SQLite.
    let bad_path = OsString::from_vec(b"/tmp/acp\x1b[31m-missing\x07\x08-file.toml".to_vec());

    acps_command()
        .env("HOME", tempdir.path())
        .args(["config", "validate"])
        .arg(&bad_path)
        .assert()
        .failure();

    acps_command()
        .env("HOME", tempdir.path())
        .args(["logs", "query", "--level", "error"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "error cli cli.error command failed",
        ));
}

#[test]
fn empty_home_is_treated_as_unset() {
    acps_command()
        .env("HOME", "")
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("HOME is not set"));
}

#[cfg(unix)]
#[test]
fn init_repairs_config_permissions_before_validation_failure() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config_path = config_dir.join("acps-config.toml");
    fs::write(
        &config_path,
        VALID_CONFIG.replace(r#"bind = "127.0.0.1:7700""#, r#"bind = "bad""#),
    )
    .expect("invalid config should be written");
    fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o755))
        .expect("config dir perms should set");
    fs::set_permissions(&config_path, fs::Permissions::from_mode(0o644))
        .expect("config file perms should set");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "api.bind must be a socket address",
        ));

    assert_eq!(mode(&config_dir), 0o700);
    assert_eq!(mode(&config_path), 0o600);
}

#[cfg(unix)]
#[test]
fn init_repairs_existing_permissive_state_file() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let state_dir = tempdir.path().join(".local/share/acp-stack");
    fs::create_dir_all(&state_dir).expect("state dir should be created");
    let state_path = state_dir.join("state.sqlite");
    fs::write(&state_path, b"").expect("placeholder state file should be written");
    fs::set_permissions(&state_path, fs::Permissions::from_mode(0o644))
        .expect("permissive perms should set");
    assert_eq!(mode(&state_path), 0o644);

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success();

    assert_eq!(mode(&state_path), 0o600);
}

#[cfg(unix)]
#[test]
fn status_repairs_existing_permissive_state_file() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG)
        .expect("valid config should be written");

    let state_dir = tempdir.path().join(".local/share/acp-stack");
    fs::create_dir_all(&state_dir).expect("state dir should be created");
    let state_path = state_dir.join("state.sqlite");
    fs::write(&state_path, b"").expect("placeholder state file should be written");
    fs::set_permissions(&state_path, fs::Permissions::from_mode(0o644))
        .expect("permissive perms should set");

    acps_command()
        .env("HOME", tempdir.path())
        .arg("status")
        .assert()
        .success();

    assert_eq!(mode(&state_path), 0o600);
}

#[cfg(unix)]
#[test]
fn logs_query_repairs_existing_permissive_state_file() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let state_dir = tempdir.path().join(".local/share/acp-stack");
    fs::create_dir_all(&state_dir).expect("state dir should be created");
    let state_path = state_dir.join("state.sqlite");
    fs::write(&state_path, b"").expect("placeholder state file should be written");
    fs::set_permissions(&state_path, fs::Permissions::from_mode(0o644))
        .expect("permissive perms should set");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["logs", "query"])
        .assert()
        .success();

    assert_eq!(mode(&state_path), 0o600);
}

#[cfg(unix)]
#[test]
fn error_recording_path_repairs_permissive_state_file() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success();

    let state_path = tempdir.path().join(".local/share/acp-stack/state.sqlite");
    fs::set_permissions(&state_path, fs::Permissions::from_mode(0o644))
        .expect("permissive perms should set");
    assert_eq!(mode(&state_path), 0o644);

    // Corrupt the config so the next invocation fails through the error-recording path.
    let config_path = tempdir.path().join(".config/acp-stack/acps-config.toml");
    fs::write(
        &config_path,
        VALID_CONFIG.replace(r#"bind = "127.0.0.1:7700""#, r#"bind = "bad""#),
    )
    .expect("invalid config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .arg("status")
        .assert()
        .failure();

    assert_eq!(
        mode(&state_path),
        0o600,
        "record_cli_error_message must repair permissive perms before writing the error row",
    );

    acps_command()
        .env("HOME", tempdir.path())
        .args(["logs", "query", "--level", "error"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "error cli cli.error command failed",
        ));
}

fn amp_config() -> String {
    VALID_CONFIG
        .replace(r#"id = "opencode""#, r#"id = "amp""#)
        .replace(r#"name = "OpenCode""#, r#"name = "Amp Code""#)
        .replace(r#"command = "opencode""#, r#"command = "amp-acp""#)
        .replace(r#"args = ["acp"]"#, r#"args = []"#)
        .replace(r#"env = ["OPENCODE_API_KEY"]"#, r#"env = ["AMP_API_KEY"]"#)
        .replace(
            r#"
[agent.install]
type = "shell"
shell = "curl -fsSL https://opencode.ai/install | bash"
creates = "opencode"
"#,
            "",
        )
}
