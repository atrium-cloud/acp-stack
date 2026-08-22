//! Fixtures and helpers shared by more than one hosted-init test group.

use super::super::*;

use axum::body::to_bytes;
use http::{Method, Request};
use serde_json::json;
use std::time::Duration;
use tower::ServiceExt;

pub(crate) const TEST_TOKEN: &str = "test_bootstrap_token";

pub(crate) fn test_session(id: &str) -> Arc<HostedInitSession> {
    HostedInitSession::new(id.to_owned(), Arc::new(Notify::new()), false)
}

/// A session whose start request declared `defer_provider_credentials`, for the
/// tests that exercise the custom-provider credential soft-pass.
pub(crate) fn test_session_deferring_credentials(id: &str) -> Arc<HostedInitSession> {
    HostedInitSession::new(id.to_owned(), Arc::new(Notify::new()), true)
}

pub(crate) fn wait_for_pending_input(session: &HostedInitSession) -> PublicInputRequest {
    for _ in 0..100 {
        if let Some(input) = lock_unpoisoned(&session.inner).pending_input.clone() {
            return input;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for hosted init input request");
}

/// Option ids are derived from the labels so the wire `value` is stable and
/// distinct from the display text, exactly as the real call sites build them.
pub(crate) fn hosted_items(labels: &[&str]) -> Vec<prompt::HostedPromptItem> {
    labels
        .iter()
        .map(|label| prompt::HostedPromptItem {
            value: format!("id_{label}"),
            label: (*label).to_owned(),
            hint: String::new(),
        })
        .collect()
}

pub(crate) fn hosted_test_request(
    kind: HostedPromptKind,
    style: HostedPromptStyle,
    prompt: &str,
    labels: &[&str],
) -> HostedPromptRequest {
    HostedPromptRequest {
        kind,
        style,
        prompt: prompt.to_owned(),
        required: false,
        default: None,
        items: hosted_items(labels),
        inspection: None,
    }
}

/// Drives one select to completion and hands back the raw driver result, so
/// rejection paths stay assertable.
pub(crate) fn select_result(
    kind: HostedPromptKind,
    prompt: &str,
    labels: &[&str],
    response: Value,
) -> Result<HostedPromptOutcome<Option<usize>>> {
    let session = test_session("init_driver_select");
    let driver = SessionPromptDriver {
        session: session.clone(),
    };
    let request = hosted_test_request(kind, HostedPromptStyle::SearchableSelect, prompt, labels);
    let handle = std::thread::spawn(move || driver.select(request));
    let pending = wait_for_pending_input(&session);
    session
        .submit_input(&pending.request_id, response)
        .expect("submit input");
    handle.join().expect("driver thread")
}

pub(crate) fn send_select_response(
    kind: HostedPromptKind,
    prompt: &str,
    labels: &[&str],
    response: Value,
) -> HostedPromptOutcome<Option<usize>> {
    select_result(kind, prompt, labels, response).expect("driver result")
}

pub(crate) fn request_from_json(payload: &str) -> StartInitRequest {
    serde_json::from_str(payload).expect("request payload must deserialize")
}

/// A probe advertisement carrying the given `mcpCapabilities`.
pub(crate) fn mcp_capabilities(
    advertised: Value,
) -> crate::runtime::agent::acp_bridge::AgentCapabilitiesDto {
    serde_json::from_value(json!({
        "protocol_version": 1,
        "capabilities": { "mcpCapabilities": advertised },
        "agent_name": "placebo",
        "agent_title": null,
        "agent_version": null,
    }))
    .expect("capabilities fixture")
}

pub(crate) fn app_with_manager(manager: Arc<HostedInitManager>) -> Router {
    build_bootstrap_router(
        BootstrapState {
            token: Arc::new(TEST_TOKEN.to_owned()),
            allowed_origins: Arc::new(vec!["https://backend.example".to_owned()]),
            manager,
            native_config_mutation: Arc::new(TokioMutex::new(())),
        },
        super::super::super::STARTER_MAX_REQUEST_BYTES,
    )
}

pub(crate) fn app_with_session(session: Arc<HostedInitSession>) -> Router {
    let manager = HostedInitManager::new();
    *lock_unpoisoned(&manager.active) = Some(session);
    app_with_manager(manager)
}

pub(crate) async fn request_json(
    app: Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
    token: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header(http::header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let body = match body {
        Some(value) => Body::from(value.to_string()),
        None => Body::empty(),
    };
    let response = app
        .oneshot(builder.body(body).expect("request"))
        .await
        .expect("response");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body bytes");
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("json body")
    };
    (status, value)
}

pub(crate) async fn request_raw_json(
    app: Router,
    method: Method,
    uri: &str,
    body: &'static str,
    token: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(http::header::CONTENT_TYPE, "application/json");
    if let Some(token) = token {
        builder = builder.header(http::header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let response = app
        .oneshot(builder.body(Body::from(body)).expect("request"))
        .await
        .expect("response");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body bytes");
    let value = serde_json::from_slice(&bytes).expect("json body");
    (status, value)
}

/// Every recorded `signal` event, in seq order.
pub(crate) fn signal_events(session: &HostedInitSession) -> Vec<Value> {
    session
        .events_after(0)
        .into_iter()
        .filter(|event| event["type"] == json!("signal"))
        .collect()
}

/// The category view a client folds from the live signal stream. The instance
/// no longer derives one, so the view-shaped assertions run the reference fold
/// over what the session actually emitted.
pub(crate) fn folded_state(session: &HostedInitSession) -> Value {
    let snapshot = session.status_snapshot();
    let awaiting = super::state_fold::awaiting_category(
        snapshot.pending_input.as_ref().map(|input| input.kind),
    );
    super::state_fold::fold_state(&signal_events(session), awaiting)
}

/// Back-compat name for the folded view; the surface tests read it exactly as
/// they read a derived snapshot before the reshape.
pub(crate) fn latest_state(session: &HostedInitSession) -> Value {
    folded_state(session)
}

/// The fold applied to what `hello` carries, so a test can assert a late joiner
/// reconstructs the identical view a full-stream client computes.
pub(crate) fn folded_from_hello(session: &HostedInitSession) -> Value {
    let hello: Value = serde_json::from_str(&session.hello_frame()).expect("hello must be json");
    let signals = hello["signals"].as_array().cloned().unwrap_or_default();
    let awaiting = super::state_fold::awaiting_category(hello["pending_input"]["kind"].as_str());
    super::state_fold::fold_state(&signals, awaiting)
}

pub(crate) fn category<'a>(state: &'a Value, id: &str) -> &'a Value {
    state["categories"]
        .as_array()
        .expect("state must carry a category array")
        .iter()
        .find(|entry| entry["id"] == json!(id))
        .unwrap_or_else(|| panic!("category `{id}` is missing from the snapshot"))
}

pub(crate) fn category_ids(state: &Value) -> Vec<String> {
    state["categories"]
        .as_array()
        .expect("state must carry a category array")
        .iter()
        .map(|entry| entry["id"].as_str().unwrap_or_default().to_owned())
        .collect()
}

pub(crate) fn awaiting_ids(state: &Value) -> Vec<String> {
    state["categories"]
        .as_array()
        .expect("state must carry a category array")
        .iter()
        .filter(|entry| entry["status"] == json!("awaiting_input"))
        .map(|entry| entry["id"].as_str().unwrap_or_default().to_owned())
        .collect()
}

pub(crate) const CANONICAL_CATEGORY_IDS: [&str; 10] = [
    "agent",
    "provider",
    "model",
    "mode",
    "effort",
    "workspace",
    "native_config",
    "mcp",
    "skills",
    "deps",
];

/// Bytes of the recorded event at `seq`, as the WebSocket would send them.
pub(crate) fn recorded_frame(session: &HostedInitSession, seq: u64) -> String {
    session
        .events_after(seq - 1)
        .first()
        .map(Value::to_string)
        .unwrap_or_else(|| panic!("no recorded init event at seq {seq}"))
}
