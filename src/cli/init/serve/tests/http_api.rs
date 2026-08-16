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

    // A running session has not published a result yet, so there is no
    // applied import to roll back.
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
