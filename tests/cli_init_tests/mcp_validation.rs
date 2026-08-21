use crate::common::cli::*;
use acp_stack::config::load_config_from_str;
use acp_stack::state::{StateStore, default_state_path};
use std::fs;

#[test]
fn init_rejects_invalid_mcp_declarations() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    for (extra_args, expected) in [
        (
            &["--mcp-http", "remote=http://mcp.example/mcp"][..],
            "mcp-http",
        ),
        (&["--mcp-http", "remote=https://"], "mcp-http"),
        (
            &["--mcp-http", "remote=https://token@mcp.example/mcp"],
            "credentials",
        ),
        (&["--mcp-preset", "unknown"], "mcp-preset"),
        (&["--mcp-stdio", "local"], "mcp-stdio"),
        (&["--mcp-stdio", "=local-mcp"], "mcp-stdio"),
        (&["--mcp-http", "remote="], "mcp-http"),
        (
            &[
                "--mcp-preset",
                "linear",
                "--mcp-http",
                "linear=https://mcp.example/mcp",
            ],
            "duplicate name",
        ),
        (
            &[
                "--mcp-stdio",
                "local=local-a",
                "--mcp-stdio",
                "local=local-b",
            ],
            "duplicate name",
        ),
        (
            &[
                "--mcp-http",
                "remote=https://mcp-a.example/mcp",
                "--mcp-http",
                "remote=https://mcp-b.example/mcp",
            ],
            "duplicate name",
        ),
        (
            &[
                "--mcp-stdio",
                "shared=local-mcp",
                "--mcp-http",
                "shared=https://mcp.example/mcp",
            ],
            "duplicate name",
        ),
        (
            &["--mcp-http-header", "remote=Authorization"],
            "mcp-http-header",
        ),
        (
            &[
                "--mcp-http",
                "remote=https://mcp.example/mcp",
                "--mcp-http-header",
                "remote=:REMOTE_MCP_TOKEN",
            ],
            "non-empty header",
        ),
        (
            &[
                "--mcp-http",
                "remote=https://mcp.example/mcp",
                "--mcp-http-header",
                "remote=Authorization:",
            ],
            "non-empty header",
        ),
        (
            &[
                "--mcp-http",
                "remote=https://mcp.example/mcp",
                "--mcp-http-header",
                "remote=Bad Header:REMOTE_MCP_TOKEN",
            ],
            "valid HTTP header name",
        ),
        (
            &[
                "--mcp-http-header",
                "missing=Authorization:REMOTE_MCP_TOKEN",
            ],
            "mcp-http-header",
        ),
        (
            &[
                "--mcp-stdio",
                "local=local-mcp",
                "--mcp-http-header",
                "local=Authorization:REMOTE_MCP_TOKEN",
            ],
            "not an HTTP server",
        ),
        (
            &[
                "--mcp-http",
                "remote=https://mcp.example/mcp",
                "--mcp-stdio-env",
                "remote=LOCAL_MCP_TOKEN",
            ],
            "not a stdio server",
        ),
        (
            &[
                "--mcp-stdio",
                "local=local-mcp",
                "--mcp-stdio-env",
                "local=BAD REF",
            ],
            "secret ref name",
        ),
        (
            &[
                "--mcp-http",
                "remote=https://mcp.example/mcp",
                "--mcp-http-header",
                "remote=Authorization:BAD REF",
            ],
            "secret ref name",
        ),
        (
            &[
                "--mcp-stdio",
                "local=local-mcp",
                "--mcp-stdio-env",
                "local=SHARED_MCP_TOKEN",
                "--mcp-http",
                "remote=https://mcp.example/mcp",
                "--mcp-http-header",
                "remote=Authorization:SHARED_MCP_TOKEN",
            ],
            "declared more than once",
        ),
        (
            &[
                "--mcp-http",
                "remote=https://mcp.example/mcp",
                "--mcp-http-header",
                "remote=Authorization:FIRST_TOKEN",
                "--mcp-http-header",
                "remote=authorization:SECOND_TOKEN",
            ],
            "already has header",
        ),
        (
            &["--mcp-stdio-env", "missing=LOCAL_MCP_TOKEN"],
            "mcp-stdio-env",
        ),
    ] {
        assert_init_mcp_failure(tempdir.path(), extra_args, expected);
    }
}

#[test]
fn init_rejects_mcp_declarations_when_config_exists() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    for (extra_args, expected) in [
        (&["--mcp-preset", "linear"][..], "--mcp-preset"),
        (&["--mcp-stdio", "local=local-mcp"], "--mcp-stdio"),
        (
            &["--mcp-stdio-env", "local=LOCAL_MCP_TOKEN"],
            "--mcp-stdio-env",
        ),
        (
            &["--mcp-http", "remote=https://mcp.example/mcp"],
            "--mcp-http",
        ),
        (
            &["--mcp-http-header", "remote=Authorization:REMOTE_MCP_TOKEN"],
            "--mcp-http-header",
        ),
    ] {
        assert_init_mcp_failure(tempdir.path(), extra_args, expected);
    }
}

fn assert_init_mcp_failure(home: &std::path::Path, extra_args: &[&str], expected: &str) {
    acps_command()
        .env("HOME", home)
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .args(extra_args)
        .assert()
        .failure()
        .stderr(predicates::str::contains(expected));
}

#[test]
fn init_rejects_mcp_secret_ref_duplicates_after_registry_defaults() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "amp",
            "--skip-testflight",
            "--skip-workspace-init",
            "--mcp-stdio",
            "local=local-mcp",
            "--mcp-stdio-env",
            "local=AMP_API_KEY",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("declared more than once"));
    assert!(
        !tempdir
            .path()
            .join(".config/acp-stack/acps-config.toml")
            .exists(),
        "invalid post-registry config must not be written"
    );
}

#[test]
fn init_rejects_private_drive_file_viewer_url_as_data_source() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "init",
            "--agent",
            "placebo",
            "--skip-testflight",
            "--data-from",
            "https://drive.google.com/file/d/abc123/view",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("private Drive file viewer link"));
}

#[test]
fn init_accepts_drive_uc_export_download_url_as_data_source() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--skip-testflight",
            "--skip-workspace-init",
            "--data-from",
            "https://drive.google.com/uc?export=download&id=abc123",
        ])
        .assert()
        .success();
}

#[test]
fn init_rejects_drive_folder_url_as_data_source() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "init",
            "--agent",
            "placebo",
            "--skip-testflight",
            "--data-from",
            "https://drive.google.com/drive/folders/abc123",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("Drive folder"));
}

#[test]
fn init_rejects_dropbox_preview_url_without_dl_flag() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "init",
            "--agent",
            "placebo",
            "--skip-testflight",
            "--data-from",
            "https://www.dropbox.com/scl/fi/abc123/file.zip?dl=0",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("Dropbox preview link"));
}

#[test]
fn init_accepts_dropbox_url_with_dl_one() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--skip-testflight",
            "--skip-workspace-init",
            "--data-from",
            "https://www.dropbox.com/scl/fi/abc123/file.zip?dl=1",
        ])
        .assert()
        .success();
}

fn write_capabilities_fixture(
    dir: &std::path::Path,
    mcp_capabilities: serde_json::Value,
) -> std::path::PathBuf {
    let path = dir.join("agent-capabilities.json");
    let body = serde_json::json!({
        "protocol_version": 1,
        "capabilities": { "mcpCapabilities": mcp_capabilities },
        "agent_name": "placebo",
        "agent_title": null,
        "agent_version": null,
    });
    fs::write(&path, body.to_string()).expect("capabilities fixture written");
    path
}

fn init_step_payload(home: &std::path::Path, kind: &str) -> (String, String) {
    let store = StateStore::open(default_state_path(home)).expect("state store");
    let run = store
        .latest_init_run()
        .expect("latest init run")
        .expect("init run exists");
    let steps = store.query_init_steps(&run.id).expect("init steps");
    let step = steps
        .iter()
        .find(|step| step.kind == kind)
        .unwrap_or_else(|| panic!("step `{kind}` recorded: {steps:?}"));
    (step.status.clone(), step.payload_json.clone())
}

#[test]
fn init_reports_unsupported_mcp_transport_as_ignored() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let fixture = write_capabilities_fixture(tempdir.path(), serde_json::json!({}));

    let output = acps_command()
        .env("HOME", tempdir.path())
        .env(
            acp_stack::dev_gates::FIXTURE_AGENT_CAPABILITIES_ENV,
            &fixture,
        )
        .args([
            "dev",
            "init",
            "--handoff-json",
            "--agent",
            "placebo",
            "--skip-workspace-init",
            "--skip-testflight",
            "--mcp-http",
            "remote=https://mcp.example/mcp",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let body: serde_json::Value = serde_json::from_slice(&output).expect("handoff json parses");
    assert_eq!(body["status"], "initialized");
    let ignored = body["ignored_features"]
        .as_array()
        .expect("ignored_features present");
    assert_eq!(ignored.len(), 1, "{body}");
    assert_eq!(ignored[0]["feature"], "mcp.server");
    assert_eq!(ignored[0]["value"], "remote");
    assert_eq!(ignored[0]["capability"], "mcpCapabilities.http");

    // Keep-in-config contract: the declaration is a faithful record and stays.
    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config readable");
    let config = load_config_from_str(&written).expect("config validates");
    assert_eq!(config.mcp.servers.len(), 1);

    let (status, payload) = init_step_payload(tempdir.path(), "capability_probe");
    assert_eq!(status, "succeeded");
    assert!(payload.contains(r#""probe_status":"ok""#), "{payload}");
    assert!(payload.contains("mcpCapabilities.http"), "{payload}");

    // Non-interactive runs never record the interactive MCP step: MCP arrives
    // through flags and the ignored-features report, not prompts.
    {
        let store = StateStore::open(default_state_path(tempdir.path())).expect("state store");
        let run = store
            .latest_init_run()
            .expect("latest init run")
            .expect("init run exists");
        let steps = store.query_init_steps(&run.id).expect("init steps");
        assert!(
            steps.iter().all(|step| step.kind != "mcp_configure"),
            "{steps:?}"
        );
    }

    // The probe persists the advertisement so capability routes answer
    // without the agent ever having been started.
    let store = StateStore::open(default_state_path(tempdir.path())).expect("state store");
    let capabilities = store
        .latest_agent_capabilities("placebo")
        .expect("capabilities query");
    assert!(capabilities.is_some(), "agent_capabilities row missing");
}

#[test]
fn init_reports_no_ignores_when_transport_is_advertised() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let fixture = write_capabilities_fixture(tempdir.path(), serde_json::json!({ "http": true }));

    let output = acps_command()
        .env("HOME", tempdir.path())
        .env(
            acp_stack::dev_gates::FIXTURE_AGENT_CAPABILITIES_ENV,
            &fixture,
        )
        .args([
            "dev",
            "init",
            "--handoff-json",
            "--agent",
            "placebo",
            "--skip-workspace-init",
            "--skip-testflight",
            "--mcp-http",
            "remote=https://mcp.example/mcp",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let body: serde_json::Value = serde_json::from_slice(&output).expect("handoff json parses");
    assert_eq!(body["status"], "initialized");
    assert!(
        body.get("ignored_features").is_none(),
        "ignored_features must be omitted when empty: {body}"
    );
}

#[test]
fn init_probe_unavailable_never_fails_init() {
    let tempdir = tempfile::tempdir().expect("tempdir");

    // No capabilities fixture and `--skip-workspace-init` leaves the spawn cwd
    // unprovisioned, so the probe cannot run. Init must succeed regardless and
    // record why the probe made no claims.
    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--skip-workspace-init",
            "--skip-testflight",
            "--mcp-http",
            "remote=https://mcp.example/mcp",
        ])
        .assert()
        .success();

    let (status, payload) = init_step_payload(tempdir.path(), "capability_probe");
    assert_eq!(status, "succeeded");
    assert!(
        payload.contains(r#""probe_status":"unavailable""#),
        "{payload}"
    );
}
