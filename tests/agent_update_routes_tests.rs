#![cfg(feature = "test-fixtures")]

//! End-to-end coverage for the manual agent-update routes, with upstream
//! "latest" lookups redirected through `ACP_STACK_GITHUB_API_BASE`.

use reqwest::StatusCode;
use serde_json::Value;
use tempfile::TempDir;

mod common;
use common::HomeEnvGuard;
use common::agent::{
    AgentHarness, EnvVarGuard, admin_bearer, http, session_bearer, spawn_provider_models_server,
    test_config,
};

use acp_stack::runtime::install::agent_updater::NON_REGISTRY_SKIP_REASON;

const GITHUB_API_BASE_ENV: &str = "ACP_STACK_GITHUB_API_BASE";
const PINNED_TAG: &str = "v1.2.3";
const MOCK_LATEST_TAG: &str = "v0.4.2";

/// Registry override backing the default `opencode` agent with a
/// github-release install, so updates resolve through the API-base seam.
fn write_opencode_github_override(config_dir: &std::path::Path) {
    let body = r#"
[[agents]]
id = "opencode"
name = "OpenCode"
kind = "native"
headless_compatible = true
set_model = true
set_mode = true
support_doc = "docs/agents/opencode.md"
github = "https://github.com/test-owner/harness-repo"

[agents.harness]
id = "true"

[agents.harness.install.github]
asset_pattern = "fake-harness"
archive = "none"
binary_name = "true"

[agents.harness.install.github.arch]
x86_64 = "x86_64"
aarch64 = "aarch64"
"#;
    std::fs::write(config_dir.join("agents.toml"), body).expect("registry override");
}

/// A config whose agent id is absent from every registry: an escape-hatch agent.
fn custom_agent_config() -> acp_stack::config::Config {
    let mut config = test_config();
    config.agent.id = "custom-agent".to_owned();
    config.agent.name = "Custom Agent".to_owned();
    config
}

fn registry_config_dir(home: &std::path::Path) -> std::path::PathBuf {
    let dir = home.join(".config").join("acp-stack");
    std::fs::create_dir_all(&dir).expect("config dir");
    dir
}

async fn seed_installed_row(harness: &AgentHarness, step: &str, method: &str, version: &str) {
    let store = harness.state.lock().await;
    store
        .append_installer_run(acp_stack::state::InstallerRunInput {
            agent_id: "opencode",
            started_at: "2026-01-01T00:00:00Z",
            finished_at: Some("2026-01-01T00:00:01Z"),
            status: "ran",
            stdout: "",
            stderr: "",
            exit_status: Some(0),
            step,
            version: Some(version),
            operation: acp_stack::state::INSTALLER_OPERATION_INSTALL,
            method: Some(method),
            log_dir: None,
            apply_run_id: None,
        })
        .expect("seed installer row");
}

/// Signals each incoming request and holds the response until the test flips
/// `released`.
struct FixtureGate {
    released: std::sync::Arc<std::sync::atomic::AtomicBool>,
    hit_tx: std::sync::mpsc::Sender<()>,
}

/// Serve a fixed raw body for every request, optionally holding each response
/// until the test releases the `gate`.
fn spawn_raw_body_server(
    body: &'static [u8],
    content_type: &'static str,
    gate: Option<FixtureGate>,
) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind raw fixture");
    let base = format!(
        "http://{}",
        listener.local_addr().expect("raw fixture addr")
    );
    std::thread::spawn(move || {
        use std::io::{BufRead as _, BufReader, Write as _};
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let Ok(reader_stream) = stream.try_clone() else {
                continue;
            };
            let mut reader = BufReader::new(reader_stream);
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).is_err() {
                continue;
            }
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) if line == "\r\n" || line == "\n" => break,
                    Ok(_) => continue,
                    Err(_) => break,
                }
            }
            if let Some(gate) = &gate {
                if gate.hit_tx.send(()).is_err() {
                    return; // test tore down
                }
                while !gate.released.load(std::sync::atomic::Ordering::SeqCst) {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            if stream.write_all(header.as_bytes()).is_err() {
                continue;
            }
            if stream.write_all(body).is_err() {
                continue;
            }
        }
    });
    base
}

#[tokio::test]
async fn update_requires_admin_key() {
    let harness = AgentHarness::spawn().await;
    let response = http()
        .await
        .post(format!("{}/v1/agent/update", harness.base_url))
        .header("Authorization", session_bearer())
        .json(&serde_json::json!({ "force": false }))
        .send()
        .await
        .expect("send update");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "auth.wrong_kind");
}

#[tokio::test]
async fn update_status_rejects_admin_key() {
    let harness = AgentHarness::spawn().await;
    let response = http()
        .await
        .get(format!("{}/v1/agent/update/status", harness.base_url))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("send status");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "auth.wrong_kind");
}

#[tokio::test]
async fn update_skips_non_registry_agent() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());

    let harness = AgentHarness::spawn_with_config(custom_agent_config()).await;
    let response = http()
        .await
        .post(format!("{}/v1/agent/update", harness.base_url))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("send update");
    let status = response.status();
    let body: Value = response.json().await.expect("json");

    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["agent_id"], "custom-agent");
    assert_eq!(body["data"]["skipped"], true);
    assert_eq!(body["data"]["updated"], false);
    assert_eq!(body["data"]["reason"], NON_REGISTRY_SKIP_REASON);
}

#[tokio::test]
async fn update_reports_up_to_date_at_pinned_version() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    write_opencode_github_override(&registry_config_dir(tempdir.path()));
    let mut config = test_config();
    config.agent.harness_version = Some(PINNED_TAG.to_owned());

    let harness = AgentHarness::spawn_with_config(config).await;
    seed_installed_row(&harness, "install", "github", "1.2.3").await;
    // The mock advertises a different latest release; the pin must win.
    let mock_base = spawn_provider_models_server(serde_json::json!({
        "tag_name": MOCK_LATEST_TAG,
        "assets": [],
    }));
    let _env = EnvVarGuard::set_many(vec![(
        GITHUB_API_BASE_ENV,
        std::ffi::OsString::from(mock_base),
    )]);
    let response = http()
        .await
        .post(format!("{}/v1/agent/update", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&serde_json::json!({ "force": false }))
        .send()
        .await
        .expect("send update");
    let status = response.status();
    let body: Value = response.json().await.expect("json");

    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["updated"], false);
    assert_eq!(body["data"]["skipped"], false);
    let steps = body["data"]["steps"].as_array().expect("steps");
    assert_eq!(steps.len(), 1, "body: {body}");
    assert_eq!(steps[0]["step"], "install");
    assert_eq!(steps[0]["status"], "up_to_date");
    assert_eq!(
        steps[0]["latest"], PINNED_TAG,
        "pin must win over the mock's latest"
    );
    assert_eq!(steps[0]["installed"], "1.2.3");
}

#[cfg(unix)]
#[tokio::test]
async fn update_skips_while_agent_running_and_releases_lock_on_stop() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());

    let harness = AgentHarness::spawn_with_config(custom_agent_config()).await;
    let client = http().await;
    let start = client
        .post(format!("{}/v1/agent/start", harness.base_url))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("start");
    assert_eq!(start.status(), StatusCode::OK);

    let response = client
        .post(format!("{}/v1/agent/update", harness.base_url))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("send update while running");
    let status = response.status();
    let body: Value = response.json().await.expect("json");
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["skipped"], true);
    assert_eq!(body["data"]["reason"], "agent is running");

    let stop = client
        .post(format!("{}/v1/agent/stop", harness.base_url))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("stop");
    assert_eq!(stop.status(), StatusCode::OK);

    // The skip reason flipping from busy to non-registry proves `finish_update`
    // released the supervisor's update lock.
    let response = client
        .post(format!("{}/v1/agent/update", harness.base_url))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("send update after stop");
    let status = response.status();
    let body: Value = response.json().await.expect("json");
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["skipped"], true);
    assert_eq!(body["data"]["reason"], NON_REGISTRY_SKIP_REASON);

    let status_body: Value = client
        .get(format!("{}/v1/agent/status", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("status")
        .json()
        .await
        .expect("status json");
    let events = status_body["data"]["lifecycle_events"]
        .as_array()
        .expect("lifecycle events");
    let api_update_events: Vec<&Value> = events
        .iter()
        .filter(|event| {
            event["event_kind"]
                .as_str()
                .is_some_and(|kind| kind.starts_with("agent.update."))
                && event["payload_json"]
                    .as_str()
                    .is_some_and(|payload| payload.contains("\"trigger\":\"api\""))
        })
        .collect();
    assert!(
        api_update_events
            .iter()
            .any(|event| event["event_kind"] == "agent.update.skipped"),
        "expected an api-triggered agent.update.skipped event, got: {events:?}"
    );
}

#[tokio::test]
async fn update_status_reports_installed_latest_pin_and_policy() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    write_opencode_github_override(&registry_config_dir(tempdir.path()));
    let mut config = test_config();
    config.agent.harness_version = Some(PINNED_TAG.to_owned());

    let harness = AgentHarness::spawn_with_config(config).await;
    seed_installed_row(&harness, "install", "github", "0.1.0").await;
    let mock_base = spawn_provider_models_server(serde_json::json!({
        "tag_name": MOCK_LATEST_TAG,
        "assets": [],
    }));
    let _env = EnvVarGuard::set_many(vec![(
        GITHUB_API_BASE_ENV,
        std::ffi::OsString::from(mock_base),
    )]);
    let response = http()
        .await
        .get(format!("{}/v1/agent/update/status", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("send status");
    let status = response.status();
    let body: Value = response.json().await.expect("json");

    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["agent_id"], "opencode");
    assert_eq!(body["data"]["managed"], true);
    assert_eq!(body["data"]["pinned"], PINNED_TAG);
    // No `[agent.auto_update]` in the fixture config: disabled, but the default
    // frequency is still reported so the caller has a value to render.
    assert_eq!(body["data"]["auto_update"]["enabled"], false);
    assert_eq!(body["data"]["auto_update"]["frequency"], "1d");
    let components = body["data"]["components"].as_array().expect("components");
    assert_eq!(components.len(), 1, "body: {body}");
    assert_eq!(components[0]["step"], "install");
    assert_eq!(components[0]["status"], "stale");
    assert_eq!(components[0]["installed"], "0.1.0");
    assert_eq!(components[0]["latest"], MOCK_LATEST_TAG);
}

#[tokio::test]
async fn update_status_degrades_to_unknown_on_upstream_failure() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    write_opencode_github_override(&registry_config_dir(tempdir.path()));

    let harness = AgentHarness::spawn_with_config(test_config()).await;
    seed_installed_row(&harness, "install", "github", "0.1.0").await;
    let _env = EnvVarGuard::set_many(vec![(
        GITHUB_API_BASE_ENV,
        std::ffi::OsString::from("http://127.0.0.1:1"),
    )]);
    let response = http()
        .await
        .get(format!("{}/v1/agent/update/status", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("send status");
    let status = response.status();
    let body: Value = response.json().await.expect("json");

    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["managed"], true);
    let components = body["data"]["components"].as_array().expect("components");
    assert_eq!(components.len(), 1, "body: {body}");
    assert_eq!(components[0]["status"], "unknown");
    let reason = components[0]["reason"].as_str().expect("reason");
    assert!(
        reason.contains("upstream lookup failed"),
        "reason: {reason}"
    );
}

#[tokio::test]
async fn update_status_reports_unmanaged_for_non_registry_agent() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());

    let harness = AgentHarness::spawn_with_config(custom_agent_config()).await;
    let response = http()
        .await
        .get(format!("{}/v1/agent/update/status", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("send status");
    let status = response.status();
    let body: Value = response.json().await.expect("json");

    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["managed"], false);
    assert_eq!(body["data"]["reason"], NON_REGISTRY_SKIP_REASON);
    assert_eq!(body["data"]["components"].as_array().map(Vec::len), Some(0));
}

#[cfg(unix)]
#[tokio::test]
async fn update_force_reinstalls_when_version_matches() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    write_opencode_github_override(&registry_config_dir(tempdir.path()));

    let harness = AgentHarness::spawn_with_config(test_config()).await;
    // Installed already matches the mock's latest: without `force` this
    // update would short-circuit as up_to_date.
    seed_installed_row(&harness, "install", "github", "0.4.2").await;
    // A shebang script so the installer's spawn gate passes.
    let download_base =
        spawn_raw_body_server(b"#!/bin/sh\nexit 0\n", "application/octet-stream", None);
    let mock_base = spawn_provider_models_server(serde_json::json!({
        "tag_name": MOCK_LATEST_TAG,
        "assets": [{
            "name": "fake-harness",
            "browser_download_url": format!("{download_base}/fake-harness"),
            "size": 17,
        }],
    }));
    let _env = EnvVarGuard::set_many(vec![(
        GITHUB_API_BASE_ENV,
        std::ffi::OsString::from(mock_base),
    )]);
    let response = http()
        .await
        .post(format!("{}/v1/agent/update", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&serde_json::json!({ "force": true }))
        .send()
        .await
        .expect("send update");
    let status = response.status();
    let body: Value = response.json().await.expect("json");

    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["updated"], true, "body: {body}");
    assert_eq!(body["data"]["skipped"], false, "body: {body}");
    let steps = body["data"]["steps"].as_array().expect("steps");
    assert_eq!(steps.len(), 1, "body: {body}");
    assert_eq!(steps[0]["step"], "install");
    assert_eq!(steps[0]["status"], "updated", "body: {body}");
    assert_eq!(steps[0]["latest"], MOCK_LATEST_TAG);
    assert_eq!(steps[0]["installed"], "0.4.2");
    let binary = tempdir.path().join(".local").join("bin").join("true");
    let content = std::fs::read(&binary).expect("reinstalled binary");
    assert_eq!(content, b"#!/bin/sh\nexit 0\n");
}

#[tokio::test]
async fn update_returns_ok_with_failed_step_when_asset_missing() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    write_opencode_github_override(&registry_config_dir(tempdir.path()));

    let harness = AgentHarness::spawn_with_config(test_config()).await;
    seed_installed_row(&harness, "install", "github", "0.1.0").await;
    // No asset matches `asset_pattern`, so the step must degrade to `failed`
    // while the route still answers 200.
    let mock_base = spawn_provider_models_server(serde_json::json!({
        "tag_name": MOCK_LATEST_TAG,
        "assets": [],
    }));
    let _env = EnvVarGuard::set_many(vec![(
        GITHUB_API_BASE_ENV,
        std::ffi::OsString::from(mock_base),
    )]);
    let response = http()
        .await
        .post(format!("{}/v1/agent/update", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&serde_json::json!({ "force": false }))
        .send()
        .await
        .expect("send update");
    let status = response.status();
    let body: Value = response.json().await.expect("json");

    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["updated"], false, "body: {body}");
    assert_eq!(body["data"]["skipped"], false, "body: {body}");
    let steps = body["data"]["steps"].as_array().expect("steps");
    assert_eq!(steps.len(), 1, "body: {body}");
    assert_eq!(steps[0]["step"], "install");
    assert_eq!(steps[0]["status"], "failed", "body: {body}");
    assert_eq!(steps[0]["installed"], "0.1.0");
    assert_eq!(steps[0]["latest"], MOCK_LATEST_TAG);
    let message = steps[0]["message"].as_str().expect("failure message");
    assert!(!message.is_empty(), "body: {body}");
}

#[tokio::test]
async fn update_skips_while_another_update_is_in_flight() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    write_opencode_github_override(&registry_config_dir(tempdir.path()));

    let harness = AgentHarness::spawn_with_config(test_config()).await;
    seed_installed_row(&harness, "install", "github", "0.1.0").await;
    // Once the gated fixture reports a hit, the first update provably holds
    // the supervisor's update lock.
    let released = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (hit_tx, hit_rx) = std::sync::mpsc::channel::<()>();
    let mock_base = spawn_raw_body_server(
        b"{\"tag_name\": \"v0.4.2\", \"assets\": []}",
        "application/json",
        Some(FixtureGate {
            released: released.clone(),
            hit_tx,
        }),
    );
    let _env = EnvVarGuard::set_many(vec![(
        GITHUB_API_BASE_ENV,
        std::ffi::OsString::from(mock_base),
    )]);

    let client = http().await;
    let first = {
        let client = client.clone();
        let url = format!("{}/v1/agent/update", harness.base_url);
        tokio::spawn(async move {
            client
                .post(url)
                .header("Authorization", admin_bearer())
                .send()
                .await
                .expect("send first update")
        })
    };
    // The blocking wait MUST run off the async executor: the in-process
    // fixture server shares this runtime.
    tokio::task::spawn_blocking(move || {
        hit_rx
            .recv_timeout(std::time::Duration::from_secs(15))
            .expect("first update reached the upstream lookup")
    })
    .await
    .expect("gate wait task");

    let second = client
        .post(format!("{}/v1/agent/update", harness.base_url))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("send second update");
    let status = second.status();
    let body: Value = second.json().await.expect("json");
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["skipped"], true, "body: {body}");
    assert_eq!(body["data"]["reason"], "agent is running");

    released.store(true, std::sync::atomic::Ordering::SeqCst);
    let first_response = first.await.expect("first update task");
    let first_status = first_response.status();
    let first_body: Value = first_response.json().await.expect("first json");
    assert_eq!(first_status, StatusCode::OK, "body: {first_body}");
}
