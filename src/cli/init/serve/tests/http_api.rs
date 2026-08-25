//! HTTP API tests for the bootstrap router: auth, conflicts, status, replay,
//! and the error envelope.

use super::super::*;
use super::support::*;

use axum::body::to_bytes;
use http::{Method, Request};
use serde_json::json;
use tower::ServiceExt;

#[tokio::test]
async fn bootstrap_api_auth_conflict_status_and_event_replay_are_non_secret() {
    let session = test_session("init_api");
    session.push_event(ServerEvent::Progress {
        message: "first".to_owned(),
    });
    session.push_event(ServerEvent::Progress {
        message: "second".to_owned(),
    });
    session.set_result(json!({
        "status": "initialized",
        "session_key": "acps_session_api_secret",
        "admin_key": "acps_admin_api_secret"
    }));
    let app = app_with_session(session);

    let (status, _) = request_json(
        app.clone(),
        Method::GET,
        "/v1/init/sessions/init_api",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, body) = request_json(
        app.clone(),
        Method::GET,
        "/v1/init/sessions/init_api",
        None,
        Some(TEST_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["status"], "completed_awaiting_ack");
    assert_eq!(body["data"]["result_available"], true);
    let status_body = body.to_string();
    assert!(!status_body.contains("acps_session_api_secret"));
    assert!(!status_body.contains("acps_admin_api_secret"));

    let (status, _) = request_json(
        app.clone(),
        Method::POST,
        "/v1/init/sessions",
        Some(json!({})),
        Some(TEST_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    let (status, body) = request_json(
        app,
        Method::GET,
        "/v1/init/sessions/init_api/events?after_seq=1",
        None,
        Some(TEST_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let events_body = body.to_string();
    assert!(events_body.contains("second"));
    assert!(events_body.contains("result_ready"));
    assert!(!events_body.contains("acps_session_api_secret"));
    assert!(!events_body.contains("acps_admin_api_secret"));
}

#[tokio::test]
async fn bootstrap_api_rejects_duplicate_authorization_headers() {
    let app = app_with_session(test_session("init_duplicate_auth"));
    let request = Request::builder()
        .method(Method::GET)
        .uri("/v1/init/sessions/init_duplicate_auth")
        .header(http::header::AUTHORIZATION, format!("Bearer {TEST_TOKEN}"))
        .header(http::header::AUTHORIZATION, "Bearer other")
        .body(Body::empty())
        .expect("request");
    let response = app.oneshot(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body bytes");
    let body: Value = serde_json::from_slice(&bytes).expect("json body");
    assert_eq!(body["ok"], false);
    assert_eq!(body["error"]["code"], "auth.malformed_header");
}

#[tokio::test]
async fn bootstrap_api_malformed_json_uses_error_envelope() {
    let app = app_with_session(test_session("init_malformed"));
    let (status, body) = request_raw_json(
        app,
        Method::POST,
        "/v1/init/sessions",
        "{not-json",
        Some(TEST_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["ok"], false);
    assert!(body["error"]["code"].is_string());
}

#[tokio::test]
async fn bootstrap_native_config_cancel_guards_session_state() {
    const CANCEL_BODY: &str = r#"{"operation_id":"nci_init_deadbeefdeadbeefdeadbeef","revision":"0000000000000000000000000000000000000000000000000000000000000000"}"#;

    let app = app_with_session(test_session("init_nc_cancel"));
    let (status, body) = request_raw_json(
        app,
        Method::POST,
        "/v1/init/sessions/unknown/native-config/cancel",
        CANCEL_BODY,
        Some(TEST_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "init.session_not_found");

    let app = app_with_session(test_session("init_nc_cancel"));
    let (status, body) = request_raw_json(
        app,
        Method::POST,
        "/v1/init/sessions/init_nc_cancel/native-config/cancel",
        CANCEL_BODY,
        Some(TEST_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "init.result_unavailable");
}

#[tokio::test]
async fn bootstrap_models_and_input_require_auth() {
    let app = app_with_session(test_session("init_auth_gate"));
    let (status, _) = request_json(app.clone(), Method::GET, "/v1/models", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _) = request_json(
        app,
        Method::POST,
        "/v1/init/sessions/init_auth_gate/input",
        Some(json!({"request_id": "ireq_x", "value": 0})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn bootstrap_input_rest_answers_pending_prompt() {
    let session = test_session("init_input_rest");
    let driver = SessionPromptDriver {
        session: session.clone(),
    };
    let request = hosted_test_request(
        HostedPromptKind::Model,
        HostedPromptStyle::SearchableSelect,
        "select model",
        &["alpha", "beta"],
    );
    let handle = std::thread::spawn(move || driver.select(request));
    let pending = wait_for_pending_input(&session);

    let app = app_with_session(session.clone());
    let (status, body) = request_json(
        app,
        Method::POST,
        "/v1/init/sessions/init_input_rest/input",
        Some(json!({"request_id": pending.request_id, "value": 1})),
        Some(TEST_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["request_id"], json!(pending.request_id));

    let outcome = handle
        .join()
        .expect("driver thread")
        .expect("driver result");
    // The answer went through the same `submit_answer` path as the WebSocket
    // frame, so the driver's index parsing applies unchanged.
    assert_eq!(outcome, HostedPromptOutcome::Handled(Some(1)));
}

#[tokio::test]
async fn bootstrap_input_rest_carries_deferred_flag() {
    let session = test_session("init_input_deferred");
    let driver = SessionPromptDriver {
        session: session.clone(),
    };
    let request = hosted_test_request(
        HostedPromptKind::TestflightConfirm,
        HostedPromptStyle::Confirm,
        "run the testflight?",
        &[],
    );
    let handle = std::thread::spawn(move || driver.confirm_with_deferral(request));
    let pending = wait_for_pending_input(&session);

    let app = app_with_session(session.clone());
    let (status, _) = request_json(
        app,
        Method::POST,
        "/v1/init/sessions/init_input_deferred/input",
        Some(json!({"request_id": pending.request_id, "value": false, "deferred": true})),
        Some(TEST_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let outcome = handle
        .join()
        .expect("driver thread")
        .expect("driver result");
    assert_eq!(
        outcome,
        HostedPromptOutcome::Handled(ConfirmAnswer {
            value: false,
            deferred: true,
        })
    );
}

#[tokio::test]
async fn bootstrap_input_rest_rejections_are_mapped() {
    let app = app_with_session(test_session("init_input_reject"));
    let (status, body) = request_json(
        app.clone(),
        Method::POST,
        "/v1/init/sessions/unknown/input",
        Some(json!({"request_id": "ireq_x", "value": 0})),
        Some(TEST_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "init.session_not_found");

    // No input request is pending on a fresh session.
    let (status, body) = request_json(
        app,
        Method::POST,
        "/v1/init/sessions/init_input_reject/input",
        Some(json!({"request_id": "ireq_x", "value": 0})),
        Some(TEST_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "init.input_rejected");

    // A stale request_id while another prompt is pending is the same
    // rejection the WebSocket reports as an `init.input_rejected` frame.
    let session = test_session("init_input_stale");
    let driver = SessionPromptDriver {
        session: session.clone(),
    };
    let request = hosted_test_request(
        HostedPromptKind::Model,
        HostedPromptStyle::SearchableSelect,
        "select model",
        &["alpha", "beta"],
    );
    let handle = std::thread::spawn(move || driver.select(request));
    let pending = wait_for_pending_input(&session);
    let app = app_with_session(session.clone());
    let (status, body) = request_json(
        app.clone(),
        Method::POST,
        "/v1/init/sessions/init_input_stale/input",
        Some(json!({"request_id": "ireq_stale", "value": 0})),
        Some(TEST_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "init.input_rejected");

    // Release the parked driver thread so the test does not leak it.
    let (status, _) = request_json(
        app,
        Method::POST,
        "/v1/init/sessions/init_input_stale/input",
        Some(json!({"request_id": pending.request_id, "value": 0})),
        Some(TEST_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    handle
        .join()
        .expect("driver thread")
        .expect("driver result");
}

/// Minimal on-disk config for the bootstrap `/v1/models` tests: a single
/// registry agent with no provider and no env refs, so discovery short-
/// circuits on the fixture file without touching a secret store.
#[cfg(feature = "test-fixtures")]
const MODELS_TEST_CONFIG_TOML: &str = r#"
[api]
bind = "127.0.0.1:7700"
max_request_bytes = 1048576

[security.http]
max_request_bytes = 1048576
rate_limit_per_minute = 120
burst = 30
auth_failures_per_minute = 5
auth_block_duration = "15m"
allowed_origins = []
trust_proxy_headers = false

[workspace]
root = "/workspace"
uploads = "/workspace/uploads"
default_shell = "/bin/bash"
runtime_user = "acp"
max_file_bytes = 8388608

[logging]
level = "info"
local_retention_days = 30

[logging.supabase]
enabled = false
url = "https://example.supabase.co"
api_key_ref = "SUPABASE_SECRET_KEY"
schema = "acp_stack"

[agent]
id = "opencode"
name = "OpenCode"
command = "opencode"
args = ["acp"]
cwd = "/workspace"
env = []
restart = "on-crash"
"#;

#[cfg(feature = "test-fixtures")]
fn write_models_test_home(root: &std::path::Path, config_toml: &str) -> std::path::PathBuf {
    let home = root.join("home");
    let config_dir = home.join(".config").join("acp-stack");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    std::fs::write(config_dir.join("acps-config.toml"), config_toml).expect("write config");
    home
}

#[cfg(feature = "test-fixtures")]
fn write_models_fixture(root: &std::path::Path) -> std::path::PathBuf {
    let fixture_path = root.join("config-options.json");
    let fixture_body = json!([
        {
            "id": "model",
            "name": "Model",
            "category": "model",
            "type": "select",
            "currentValue": "openai/gpt-4o",
            "options": [
                { "value": "openai/gpt-4o", "name": "openai/gpt-4o" },
                { "value": "anthropic/claude-3-5-sonnet", "name": "anthropic/claude-3-5-sonnet" }
            ]
        },
        {
            "id": "mode",
            "name": "Mode",
            "category": "mode",
            "type": "select",
            "currentValue": "default",
            "options": [
                { "value": "default", "name": "default" },
                { "value": "yolo", "name": "yolo" }
            ]
        }
    ]);
    std::fs::write(&fixture_path, fixture_body.to_string()).expect("write fixture");
    fixture_path
}

#[cfg(feature = "test-fixtures")]
#[tokio::test]
async fn bootstrap_models_serves_fixture_discovery() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let home = write_models_test_home(tempdir.path(), MODELS_TEST_CONFIG_TOML);
    let fixture_path = write_models_fixture(tempdir.path());
    let _guard = TestEnvGuard::set(&[
        ("HOME", home.as_path()),
        (
            "ACP_STACK_AGENT_CONFIG_OPTIONS_PATH",
            fixture_path.as_path(),
        ),
    ]);

    let app = app_with_manager(HostedInitManager::new());
    let (status, body) = request_json(app, Method::GET, "/v1/models", None, Some(TEST_TOKEN)).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["agent_id"], "opencode");
    assert_eq!(body["data"]["source"], "acp_advertised");
    let models = body["data"]["models"].as_array().expect("models array");
    assert!(
        models
            .iter()
            .any(|model| model["value"].as_str() == Some("openai/gpt-4o")),
        "fixture model values missing: {models:?}",
    );
    let modes = body["data"]["modes"].as_array().expect("modes array");
    assert!(
        modes.iter().any(|mode| mode.as_str() == Some("default")),
        "fixture mode values missing: {modes:?}",
    );
}

#[cfg(feature = "test-fixtures")]
#[tokio::test]
async fn bootstrap_models_target_param_resolves_array_target() {
    let mut config = config::load_config_from_str(MODELS_TEST_CONFIG_TOML).expect("config parses");
    config.array.enabled = true;
    let mut secondary = config.agent.clone();
    secondary.id = "codex".to_owned();
    secondary.name = "Codex".to_owned();
    config.array.targets.push(config::ArrayTargetConfig {
        id: "codex".to_owned(),
        agent: secondary,
    });
    let config_toml = config.to_canonical_toml().expect("canonical toml");

    let tempdir = tempfile::tempdir().expect("tempdir");
    let home = write_models_test_home(tempdir.path(), &config_toml);
    let fixture_path = write_models_fixture(tempdir.path());
    let _guard = TestEnvGuard::set(&[
        ("HOME", home.as_path()),
        (
            "ACP_STACK_AGENT_CONFIG_OPTIONS_PATH",
            fixture_path.as_path(),
        ),
    ]);

    let app = app_with_manager(HostedInitManager::new());
    let (status, body) = request_json(
        app.clone(),
        Method::GET,
        "/v1/models?target_id=codex",
        None,
        Some(TEST_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["agent_id"], "codex");

    // The `target` alias resolves the same target as `target_id`.
    let (status, body) = request_json(
        app.clone(),
        Method::GET,
        "/v1/models?target=codex",
        None,
        Some(TEST_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["agent_id"], "codex");

    let (status, body) = request_json(
        app,
        Method::GET,
        "/v1/models?target_id=opencode",
        None,
        Some(TEST_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["agent_id"], "opencode");
}

#[cfg(feature = "test-fixtures")]
#[tokio::test]
async fn bootstrap_models_rejects_unknown_and_gated_agent() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let home = write_models_test_home(tempdir.path(), MODELS_TEST_CONFIG_TOML);
    let _guard = TestEnvGuard::set(&[("HOME", home.as_path())]);

    let app = app_with_manager(HostedInitManager::new());
    // Array mode is off in the test config, so any non-primary id is rejected
    // by the same gate `session_agent_target` applies — never a silent
    // fallback to the default target.
    let (status, body) = request_json(
        app,
        Method::GET,
        "/v1/models?target_id=codex",
        None,
        Some(TEST_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "request.invalid_param");
}

#[cfg(feature = "test-fixtures")]
#[tokio::test]
async fn bootstrap_models_not_ready_before_config_is_staged() {
    // Fresh init has not written acps-config.toml yet: the picker reports a
    // retryable not-ready state, not an opaque 500 config read failure.
    let tempdir = tempfile::tempdir().expect("tempdir");
    let home = tempdir.path().join("home");
    std::fs::create_dir_all(&home).expect("home dir");
    let _guard = TestEnvGuard::set(&[("HOME", home.as_path())]);

    let app = app_with_manager(HostedInitManager::new());
    let (status, body) = request_json(app, Method::GET, "/v1/models", None, Some(TEST_TOKEN)).await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {body}");
    assert_eq!(body["error"]["code"], "init.config_not_ready");
}
