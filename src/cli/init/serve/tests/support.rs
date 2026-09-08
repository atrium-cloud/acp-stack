//! Fixtures and helpers shared by more than one hosted-init test group.

use super::super::*;

use axum::body::to_bytes;
use http::{Method, Request};
use serde_json::json;
use std::time::Duration;
use tower::ServiceExt;

use crate::secrets::{SecretStore, new_shared_secret_store};

pub(crate) const TEST_TOKEN: &str = "test_bootstrap_token";

#[cfg(feature = "test-fixtures")]
pub(crate) use crate::cli::init::test_env::TestEnvGuard;

pub(crate) fn test_session(id: &str) -> Arc<HostedInitSession> {
    HostedInitSession::new(id.to_owned(), Arc::new(Notify::new()), false)
}

/// A shared store in a fresh tempdir; the returned guard must outlive the
/// router built on the handle, or a later mutation writes into a deleted dir.
pub(crate) fn test_shared_secret_store() -> (SharedSecretStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = SecretStore::open_or_create(dir.path()).expect("secret store");
    (new_shared_secret_store(store), dir)
}

/// A session whose start request declared `defer_provider_credentials`.
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

/// The pending prompt of a given wire `kind`, for waiting on a re-issued lane
/// rather than racing whichever prompt happens to be pending.
pub(crate) fn wait_for_pending_kind(session: &HostedInitSession, kind: &str) -> PublicInputRequest {
    for _ in 0..200 {
        if let Some(input) = lock_unpoisoned(&session.inner).pending_input.clone()
            && input.kind == kind
        {
            return input;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for a pending `{kind}` input request");
}

pub(crate) fn wait_for_status(session: &HostedInitSession, status: &str) {
    for _ in 0..200 {
        if session.status() == status {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!(
        "timed out waiting for status `{status}`; session is `{}`",
        session.status()
    );
}

/// One advertised generic config option, in the shape the wizard streams.
pub(crate) fn config_option_snapshot(
    id: &str,
) -> crate::runtime::agent::config_options::SessionConfigOptionSnapshot {
    serde_json::from_value(json!({
        "id": id,
        "name": id,
        "type": "select",
        "current_value": "balanced",
        "options": [
            { "value": "balanced", "name": "Balanced" },
            { "value": "research", "name": "Research" }
        ]
    }))
    .expect("config option snapshot")
}

pub(crate) fn config_option_test_request(
    advertised: crate::runtime::agent::config_options::SessionConfigOptionSnapshot,
) -> HostedPromptRequest {
    HostedPromptRequest {
        kind: HostedPromptKind::ConfigOption,
        style: HostedPromptStyle::SearchableSelect,
        prompt: advertised.name.clone(),
        required: false,
        default: None,
        items: Vec::new(),
        inspection: None,
        config_option: Some(advertised),
    }
}

/// Option ids derived from labels, so the wire `value` stays distinct from the
/// display text exactly as the real call sites build them.
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
        config_option: None,
    }
}

/// One discovery-lane select, in the searchable style the wizard uses.
pub(crate) fn revisable_select_request(
    kind: HostedPromptKind,
    labels: &[&str],
) -> HostedPromptRequest {
    hosted_test_request(kind, HostedPromptStyle::SearchableSelect, "pick", labels)
}

/// A wizard thread that answers each request as a revisable prompt and then
/// drains the close wait, handing back every revision it was given in the order
/// it received them.
pub(crate) fn spawn_discovery_wizard(
    session: Arc<HostedInitSession>,
    requests: Vec<HostedPromptRequest>,
) -> std::thread::JoinHandle<Result<Vec<DiscoveryRevision>>> {
    std::thread::spawn(move || {
        let mut revisions = Vec::new();
        for request in requests {
            match session.request_input_revisable(request)? {
                Some(HostedInput::Revision(revision)) => revisions.push(revision),
                Some(HostedInput::Answer(_)) | None => {}
            }
        }
        while let DiscoveryWait::Revised(revision) = session.await_discovery_close()? {
            revisions.push(revision);
        }
        Ok(revisions)
    })
}

/// Answer whichever prompt of this kind is pending, returning the prompt so a
/// later revision can address it by its own id.
pub(crate) fn answer_pending(
    session: &HostedInitSession,
    kind: &str,
    value: Value,
) -> PublicInputRequest {
    let pending = wait_for_pending_kind(session, kind);
    session
        .submit_input(&pending.request_id, value)
        .expect("answer the pending prompt");
    // Wait for the wizard to consume it, so the next `answer_pending` of the
    // same kind cannot address this prompt a second time.
    for _ in 0..200 {
        let still_pending = lock_unpoisoned(&session.inner)
            .pending_input
            .as_ref()
            .is_some_and(|input| input.request_id == pending.request_id);
        if !still_pending {
            return pending;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for the wizard to consume its answer");
}

/// Close the phase the way a client does: retry while the wizard is still
/// draining, since the refusal stays `Busy` until it parks with an empty queue.
pub(crate) fn close_discovery_when_ready(session: &HostedInitSession) {
    for _ in 0..200 {
        match session.close_discovery() {
            Ok(()) => return,
            Err(CloseRejected::Busy(_)) => std::thread::sleep(Duration::from_millis(10)),
            Err(CloseRejected::NotOpen(message)) => panic!("close was refused: {message}"),
        }
    }
    panic!("timed out closing the discovery phase");
}

/// Close the phase and collect the revisions the wizard was handed.
pub(crate) fn close_and_join(
    session: &HostedInitSession,
    handle: std::thread::JoinHandle<Result<Vec<DiscoveryRevision>>>,
) -> Vec<DiscoveryRevision> {
    close_discovery_when_ready(session);
    handle
        .join()
        .expect("wizard thread")
        .expect("wizard result")
}

/// Drives one select to completion, handing back the raw driver result.
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

pub(crate) fn app_with_manager_and_store(
    manager: Arc<HostedInitManager>,
    secret_store: SharedSecretStore,
) -> Router {
    build_bootstrap_router(
        BootstrapState {
            token: Arc::new(TEST_TOKEN.to_owned()),
            allowed_origins: Arc::new(vec!["https://backend.example".to_owned()]),
            manager,
            native_config_mutation: Arc::new(TokioMutex::new(())),
            secret_store,
        },
        super::super::super::STARTER_MAX_REQUEST_BYTES,
    )
}

pub(crate) fn app_with_manager(manager: Arc<HostedInitManager>) -> (Router, tempfile::TempDir) {
    let (secret_store, dir) = test_shared_secret_store();
    (app_with_manager_and_store(manager, secret_store), dir)
}

pub(crate) fn app_with_session(session: Arc<HostedInitSession>) -> (Router, tempfile::TempDir) {
    let (secret_store, dir) = test_shared_secret_store();
    let manager = HostedInitManager::new(secret_store.clone());
    *lock_unpoisoned(&manager.active) = Some(session);
    (app_with_manager_and_store(manager, secret_store), dir)
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
        Some(value) => {
            builder = builder.header(http::header::CONTENT_TYPE, "application/json");
            Body::from(value.to_string())
        }
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

/// The category view a client folds from the live signal stream.
pub(crate) fn folded_state(session: &HostedInitSession) -> Value {
    let snapshot = session.status_snapshot();
    let awaiting = super::state_fold::awaiting_category(
        snapshot.pending_input.as_ref().map(|input| input.kind),
    );
    super::state_fold::fold_state(&signal_events(session), awaiting)
}

/// Back-compat name for the folded view.
pub(crate) fn latest_state(session: &HostedInitSession) -> Value {
    folded_state(session)
}

/// The fold applied to what `hello` carries, so a late joiner's view can be
/// compared against a full-stream client's.
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
