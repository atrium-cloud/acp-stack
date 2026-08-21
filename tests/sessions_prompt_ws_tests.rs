#![cfg(feature = "test-fixtures")]

//! Prompt-path and websocket coverage for the session routes: the modality
//! gate that screens prompt content against the target model, live event
//! fanout over `/v1/ws`, and the prompt failure taxonomy persisted when the
//! agent's inference call fails or the prompt stalls.
//!
//! The placebo ACP fixture stands in for a real ACP agent;
//! `tests/acp_bridge_tests.rs` exercises the lower-level bridge layer.

mod common;

use std::time::Duration;

use acp_stack::config::ArrayTargetConfig;
use common::sessions::{
    Harness, admin_bearer, create_session, http, prompt_count_for_session, recv_matching_event,
    session_bearer, websocket_request,
};
use futures::{SinkExt, StreamExt};
use reqwest::StatusCode;
use serde_json::{Value, json};

#[tokio::test]
async fn prompt_gate_allows_text_prompt_for_known_text_model() {
    let model_id = "provider/text-only";
    let harness = Harness::spawn_with_models_cache(
        |config| {
            config.agent.model = Some(model_id.to_owned());
            config
                .agent
                .args
                .extend(["--model-config-option".to_owned(), model_id.to_owned()]);
        },
        json!({
            model_id: {
                "id": model_id,
                "modalities": { "input": ["text"] }
            }
        }),
    )
    .await;
    let session_id = create_session(&harness).await;

    let response = http()
        .post(format!(
            "{}/v1/sessions/{}/prompt",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({ "prompt": "text is fine" }))
        .send()
        .await
        .expect("prompt");

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn prompt_gate_rejects_image_for_known_text_model_without_prompt_row() {
    let model_id = "provider/text-only";
    let harness = Harness::spawn_with_models_cache(
        |config| {
            config.agent.model = Some(model_id.to_owned());
            config
                .agent
                .args
                .extend(["--model-config-option".to_owned(), model_id.to_owned()]);
        },
        json!({
            model_id: {
                "id": model_id,
                "modalities": { "input": ["text"] }
            }
        }),
    )
    .await;
    let session_id = create_session(&harness).await;

    let response = http()
        .post(format!(
            "{}/v1/sessions/{}/prompt",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({
            "prompt": [{
                "type": "image",
                "data": "aW1hZ2U=",
                "mimeType": "image/png"
            }]
        }))
        .send()
        .await
        .expect("prompt");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "prompt.unsupported_modality");
    assert_eq!(prompt_count_for_session(&harness, &session_id).await, 0);
}

#[tokio::test]
async fn prompt_gate_rejects_video_blob_for_known_text_model() {
    let model_id = "provider/text-only";
    let harness = Harness::spawn_with_models_cache(
        |config| {
            config.agent.model = Some(model_id.to_owned());
            config
                .agent
                .args
                .extend(["--model-config-option".to_owned(), model_id.to_owned()]);
        },
        json!({
            model_id: {
                "id": model_id,
                "modalities": { "input": ["text"] }
            }
        }),
    )
    .await;
    let session_id = create_session(&harness).await;

    let response = http()
        .post(format!(
            "{}/v1/sessions/{}/prompt",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({
            "prompt": [{
                "type": "resource",
                "resource": {
                    "blob": "dmlkZW8=",
                    "uri": "file:///clip.mp4",
                    "mimeType": "video/mp4"
                }
            }]
        }))
        .send()
        .await
        .expect("prompt");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "prompt.unsupported_modality");
}

#[tokio::test]
async fn prompt_gate_allows_pdf_blob_for_known_text_model() {
    let model_id = "provider/text-only";
    let harness = Harness::spawn_with_models_cache(
        |config| {
            config.agent.model = Some(model_id.to_owned());
            config
                .agent
                .args
                .extend(["--model-config-option".to_owned(), model_id.to_owned()]);
        },
        json!({
            model_id: {
                "id": model_id,
                "modalities": { "input": ["text"] }
            }
        }),
    )
    .await;
    let session_id = create_session(&harness).await;

    let response = http()
        .post(format!(
            "{}/v1/sessions/{}/prompt",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({
            "prompt": [{
                "type": "resource",
                "resource": {
                    "blob": "cGRm",
                    "uri": "file:///doc.pdf",
                    "mimeType": "application/pdf"
                }
            }]
        }))
        .send()
        .await
        .expect("prompt");

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn prompt_gate_allows_image_for_unknown_model() {
    let model_id = "provider/unlisted";
    let harness = Harness::spawn_with_models_cache(
        |config| {
            config.agent.model = Some(model_id.to_owned());
            config
                .agent
                .args
                .extend(["--model-config-option".to_owned(), model_id.to_owned()]);
        },
        json!({
            "provider/text-only": {
                "id": "provider/text-only",
                "modalities": { "input": ["text"] }
            }
        }),
    )
    .await;
    let session_id = create_session(&harness).await;

    let response = http()
        .post(format!(
            "{}/v1/sessions/{}/prompt",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({
            "prompt": [{
                "type": "image",
                "data": "aW1hZ2U=",
                "mimeType": "image/png"
            }]
        }))
        .send()
        .await
        .expect("prompt");

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn prompt_gate_uses_array_target_model_for_media_checks() {
    let primary_model = "provider/text-only";
    let secondary_model = "provider/vision";
    let harness = Harness::spawn_with_models_cache(
        |config| {
            config.array.enabled = true;
            config.agent.model = Some(primary_model.to_owned());
            config
                .agent
                .args
                .extend(["--model-config-option".to_owned(), primary_model.to_owned()]);
            let mut secondary = config.agent.clone();
            secondary.id = "codex".to_owned();
            secondary.name = "Codex".to_owned();
            secondary.model = Some(secondary_model.to_owned());
            secondary.args = vec![
                "acp".to_owned(),
                "--model-config-option".to_owned(),
                secondary_model.to_owned(),
            ];
            config.array.targets.push(ArrayTargetConfig {
                id: "codex".to_owned(),
                agent: secondary,
            });
        },
        json!({
            primary_model: {
                "id": primary_model,
                "modalities": { "input": ["text"] }
            },
            secondary_model: {
                "id": secondary_model,
                "modalities": { "input": ["text", "image"] }
            }
        }),
    )
    .await;
    let client = http();
    let start = client
        .post(format!("{}/v1/array/targets/codex/start", harness.base_url))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("start codex");
    assert_eq!(start.status(), StatusCode::OK);

    let create = client
        .post(format!("{}/v1/sessions", harness.base_url))
        .header("Authorization", session_bearer())
        .json(&json!({ "target": "codex" }))
        .send()
        .await
        .expect("create session");
    assert_eq!(create.status(), StatusCode::OK);
    let session_id = create.json::<Value>().await.expect("create json")["data"]["id"]
        .as_str()
        .expect("session id")
        .to_owned();

    let response = client
        .post(format!(
            "{}/v1/sessions/{}/prompt?target=codex",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({
            "prompt": [{
                "type": "image",
                "data": "aW1hZ2U=",
                "mimeType": "image/png"
            }]
        }))
        .send()
        .await
        .expect("prompt");

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn websocket_streams_live_session_update_events() {
    let harness = Harness::spawn().await;
    let session_id = create_session(&harness).await;
    let request = websocket_request(&harness, session_bearer());
    let (mut ws, response) = tokio_tungstenite::connect_async(request)
        .await
        .expect("websocket connects");
    assert_eq!(response.status().as_u16(), 101);

    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        json!({
            "type": "subscribe",
            "topics": [format!("sessions.{session_id}")]
        })
        .to_string()
        .into(),
    ))
    .await
    .expect("subscribe");

    let client = http();
    let submit = client
        .post(format!(
            "{}/v1/sessions/{}/prompt",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({ "prompt": "stream me" }))
        .send()
        .await
        .expect("submit");
    assert_eq!(submit.status(), StatusCode::OK);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut received = None;
    while tokio::time::Instant::now() < deadline {
        let Some(message) = tokio::time::timeout(Duration::from_secs(1), ws.next())
            .await
            .expect("ws message before timeout")
        else {
            break;
        };
        let message = message.expect("ws message ok");
        let tokio_tungstenite::tungstenite::Message::Text(text) = message else {
            continue;
        };
        let event: Value = serde_json::from_str(&text).expect("event json");
        if event["type"] == "event"
            && event["topic"] == format!("sessions.{session_id}")
            && event["payload"]["kind"] == "session.update"
        {
            received = Some(event);
            break;
        }
    }
    let event = received.expect("session.update websocket event");
    assert!(event["id"].as_str().unwrap_or("").starts_with("evt_"));
    assert!(
        event["createdAt"].as_str().unwrap_or("").contains('T'),
        "createdAt should be an RFC3339 timestamp"
    );
    assert!(
        event["payload"].to_string().contains("chunk-"),
        "event payload = {event}"
    );
}

#[tokio::test]
async fn websocket_rejects_admin_key() {
    let harness = Harness::spawn().await;
    let request = websocket_request(&harness, admin_bearer());
    let err = tokio_tungstenite::connect_async(request)
        .await
        .expect_err("admin key must not upgrade session websocket");
    match err {
        tokio_tungstenite::tungstenite::Error::Http(response) => {
            assert_eq!(
                response.status().as_u16(),
                StatusCode::UNAUTHORIZED.as_u16()
            );
        }
        other => panic!("expected HTTP 401, got {other:?}"),
    }
}

#[tokio::test]
async fn append_session_event_fans_out_to_session_and_logs_topics() {
    let harness = Harness::spawn().await;
    let session_id = create_session(&harness).await;

    // One subscriber per topic; the bug we are guarding against silently
    // dropped session-topic delivery while logs-topic delivery still worked.
    let session_request = websocket_request(&harness, session_bearer());
    let (mut session_ws, _) = tokio_tungstenite::connect_async(session_request)
        .await
        .expect("session websocket connects");
    session_ws
        .send(tokio_tungstenite::tungstenite::Message::Text(
            json!({
                "type": "subscribe",
                "topics": [format!("sessions.{session_id}")]
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("session subscribe");

    let logs_request = websocket_request(&harness, session_bearer());
    let (mut logs_ws, _) = tokio_tungstenite::connect_async(logs_request)
        .await
        .expect("logs websocket connects");
    logs_ws
        .send(tokio_tungstenite::tungstenite::Message::Text(
            json!({
                "type": "subscribe",
                "topics": ["logs"]
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("logs subscribe");

    // The WS server processes subscribe frames inside the same select! arm as
    // event fanout, so a state write that happens before the server has
    // observed the subscribe frame is silently dropped on the broadcast end.
    // Poll the connections endpoint until both topics show as subscribed.
    let subscribe_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if tokio::time::Instant::now() > subscribe_deadline {
            panic!("ws subscriptions never registered");
        }
        let connections: Value = http()
            .get(format!("{}/v1/ws/connections", harness.base_url))
            .header("Authorization", session_bearer())
            .send()
            .await
            .expect("ws connections")
            .json()
            .await
            .expect("ws connections json");
        let topics_present: Vec<String> = connections["data"]["connections"]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .flat_map(|connection| {
                connection["topics"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|topic| topic.as_str().map(str::to_owned))
            })
            .collect();
        if topics_present
            .iter()
            .any(|topic| topic == &format!("sessions.{session_id}"))
            && topics_present.iter().any(|topic| topic == "logs")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Direct state write so the assertion targets the publish site, not the
    // bridge plumbing.
    {
        let store = harness.state.lock().await;
        store
            .append_session_event_with_source(
                &session_id,
                "info",
                "session.update",
                acp_stack::state::EVENT_SOURCE_ACP,
                "ACP session update",
                r#"{"seq":42}"#,
            )
            .expect("event inserted");
    }

    let session_event = recv_matching_event(
        &mut session_ws,
        &format!("sessions.{session_id}"),
        "session.update",
    )
    .await
    .expect("session.update on sessions.{id} topic");
    let session_payload: Value =
        serde_json::from_value(session_event["payload"]["data"].clone()).expect("session data");
    assert_eq!(session_payload["seq"], 42);

    let logs_event = recv_matching_event(&mut logs_ws, "logs", "session.update")
        .await
        .expect("session.update on logs topic");
    assert_eq!(logs_event["payload"]["data"]["kind"], "session.update");
}

/// Phase 2: when the agent's `session/prompt` JSON-RPC failure carries an
/// embedded HTTP status (e.g. `503 Service Unavailable`), the supervisor
/// classifies it as an inference-5xx failure, persists the structured detail
/// envelope, and emits a `prompt.inference_failed` session event. The raw
/// upstream message — including the URL and secret-looking token below — must
/// never reach the persisted `error_message`, `failure_detail_json`, or event
/// payload.
#[tokio::test]
async fn prompt_inference_5xx_persists_taxonomy_and_emits_event() {
    let injected_message = "upstream call to https://api.openai.com/v1/chat?key=sk-secret returned 503 Service Unavailable";
    let harness = Harness::spawn_with(|config| {
        config
            .agent
            .args
            .extend(["--prompt-inference-error".into(), injected_message.into()]);
    })
    .await;
    let session_id = create_session(&harness).await;

    let client = http();
    let submit: Value = client
        .post(format!(
            "{}/v1/sessions/{}/prompt",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({ "prompt": "ping the upstream" }))
        .send()
        .await
        .expect("submit")
        .json()
        .await
        .expect("submit json");
    let prompt_id = submit["data"]["prompt_id"]
        .as_str()
        .expect("prompt id")
        .to_owned();

    // Poll the prompt row until it lands in a terminal status; the inference
    // failure path settles as `errored`.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let terminal = loop {
        if std::time::Instant::now() > deadline {
            panic!("prompt never settled");
        }
        let state = harness.state.lock().await;
        let prompt = state.get_prompt(&prompt_id).expect("prompt lookup");
        drop(state);
        if let Some(record) = prompt
            && matches!(record.status.as_str(), "errored" | "stalled" | "cancelled")
        {
            break record;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    assert_eq!(terminal.status, "errored");
    assert_eq!(
        terminal.error_code.as_deref(),
        Some("agent.inference_5xx"),
        "expected inference_5xx error_code, got {:?}",
        terminal.error_code,
    );
    assert_eq!(
        terminal.failure_class.as_deref(),
        Some("inference_5xx"),
        "expected failure_class inference_5xx, got {:?}",
        terminal.failure_class,
    );

    let detail = terminal
        .failure_detail_json
        .as_deref()
        .expect("failure_detail_json present");
    let detail_value: Value = serde_json::from_str(detail).expect("detail json");
    assert_eq!(detail_value["status_code"], 503);
    assert_eq!(detail_value["reason_category"], "service_unavailable");

    // The persisted error_message must NOT contain any portion of the raw
    // upstream string (URL substring, secret-looking token, raw status text).
    let error_message = terminal
        .error_message
        .as_deref()
        .expect("public message present");
    assert!(
        !error_message.contains("503 Service Unavailable"),
        "raw status text leaked into error_message: {error_message}"
    );
    assert!(
        !error_message.contains("api.openai.com"),
        "url leaked into error_message: {error_message}"
    );
    assert!(
        !error_message.contains("sk-secret"),
        "secret-looking token leaked into error_message: {error_message}"
    );

    // Same invariant applied to `failure_detail_json` and `error_code` — a
    // future refactor that pipes raw upstream text into the JSON detail or the
    // error code must be caught here.
    assert!(
        !detail.contains("503 Service Unavailable")
            && !detail.contains("api.openai.com")
            && !detail.contains("sk-secret"),
        "raw upstream text leaked into failure_detail_json: {detail}"
    );
    let error_code = terminal.error_code.as_deref().expect("error_code present");
    assert!(
        !error_code.contains("api.openai.com") && !error_code.contains("sk-secret"),
        "raw upstream text leaked into error_code: {error_code}"
    );

    // A session-scoped event with kind `prompt.inference_failed` must exist
    // for this session and carry the structured payload.
    let state = harness.state.lock().await;
    let events = state
        .query_session_events(&session_id, None, 100)
        .expect("session events");
    drop(state);
    let inference_event = events
        .iter()
        .find(|event| event.kind == "prompt.inference_failed")
        .expect("prompt.inference_failed event present");
    let payload_value: Value =
        serde_json::from_str(&inference_event.payload_json).expect("event payload json");
    assert_eq!(payload_value["status_code"], 503);
    assert_eq!(payload_value["reason_category"], "service_unavailable");
    assert_eq!(payload_value["prompt_id"], prompt_id);
    // And neither the message nor the payload should leak the URL/secret.
    assert!(!inference_event.message.contains("openai"));
    assert!(!inference_event.message.contains("sk-secret"));
    assert!(!inference_event.payload_json.contains("openai"));
    assert!(!inference_event.payload_json.contains("sk-secret"));
}

#[tokio::test]
async fn stalled_prompt_suppresses_late_terminal_failure_event() {
    const DELAY_MS: u64 = 1000;
    let injected_message = "upstream returned 503 Service Unavailable";
    let harness = Harness::spawn_with(|config| {
        config.agent.args.extend([
            "--prompt-inference-error-after-update".into(),
            injected_message.into(),
            "--prompt-response-delay-ms".into(),
            DELAY_MS.to_string(),
        ]);
    })
    .await;
    let session_id = create_session(&harness).await;

    let submit: Value = http()
        .post(format!(
            "{}/v1/sessions/{}/prompt",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({ "prompt": "race a stalled prompt" }))
        .send()
        .await
        .expect("submit")
        .json()
        .await
        .expect("submit json");
    let prompt_id = submit["data"]["prompt_id"]
        .as_str()
        .expect("prompt id")
        .to_owned();

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if std::time::Instant::now() > deadline {
            panic!("prompt never reached running");
        }
        let state = harness.state.lock().await;
        let status = state
            .get_prompt(&prompt_id)
            .expect("prompt lookup")
            .map(|record| record.status);
        drop(state);
        if status.as_deref() == Some("running") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    {
        let state = harness.state.lock().await;
        let stalled = state
            .mark_stalled_prompts(Duration::from_secs(0), "test forced stall")
            .expect("mark stalled");
        assert!(
            stalled.iter().any(|(id, _)| id == &prompt_id),
            "forced stall should include submitted prompt, got {stalled:?}"
        );
    }

    tokio::time::sleep(Duration::from_millis(DELAY_MS + 250)).await;

    let state = harness.state.lock().await;
    let prompt = state
        .get_prompt(&prompt_id)
        .expect("prompt lookup")
        .expect("prompt exists");
    assert_eq!(prompt.status, "stalled");
    assert_eq!(
        prompt.failure_class.as_deref(),
        Some(acp_stack::state::FailureClass::Stalled.as_str())
    );
    let events = state
        .query_session_events(&session_id, None, 100)
        .expect("session events");
    drop(state);
    assert!(
        events
            .iter()
            .all(|event| event.kind != "prompt.inference_failed" && event.kind != "prompt.errored"),
        "late terminal failure event should be suppressed after stalled transition, got {events:?}"
    );
}

#[tokio::test]
async fn operator_disconnect_records_supplied_reason() {
    let harness = Harness::spawn().await;
    let request = websocket_request(&harness, session_bearer());
    let (_ws, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("websocket connects");
    let connection_id = await_ws_connection_id(&harness, &[]).await;

    let response = http()
        .post(format!("{}/v1/ws/connections/disconnect", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&json!({
            "connection_ids": [connection_id.clone()],
            "reason": "rotating the session key"
        }))
        .send()
        .await
        .expect("disconnect");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("disconnect json");
    assert_eq!(body["data"]["requested"], 1);

    let payload = await_disconnect_payload(&harness, &connection_id).await;
    // The machine cause and the operator's text are separate fields: the
    // former stays a closed vocabulary, the latter is free-form.
    assert_eq!(payload["reason"], "operator_disconnect");
    assert_eq!(payload["operator_reason"], "rotating the session key");
}

#[tokio::test]
async fn operator_disconnect_without_reason_omits_operator_reason() {
    let harness = Harness::spawn().await;
    let request = websocket_request(&harness, session_bearer());
    let (_ws, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("websocket connects");
    let connection_id = await_ws_connection_id(&harness, &[]).await;

    let response = http()
        .post(format!("{}/v1/ws/connections/disconnect", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&json!({ "connection_ids": [connection_id.clone()] }))
        .send()
        .await
        .expect("disconnect");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("disconnect json");
    assert_eq!(body["data"]["requested"], 1);

    let payload = await_disconnect_payload(&harness, &connection_id).await;
    assert_eq!(payload["reason"], "operator_disconnect");
    assert!(
        payload.get("operator_reason").is_none(),
        "operator_reason must be absent, not null, when no reason was supplied: {payload}"
    );
}

#[tokio::test]
async fn session_disconnect_records_supplied_reason() {
    let harness = Harness::spawn().await;
    let session_id = create_session(&harness).await;
    let topic = format!("sessions.{session_id}");
    let request = websocket_request(&harness, session_bearer());
    let (mut ws, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("websocket connects");
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        json!({ "type": "subscribe", "topics": [topic.clone()] })
            .to_string()
            .into(),
    ))
    .await
    .expect("subscribe");
    let connection_id = await_ws_connection_id(&harness, std::slice::from_ref(&topic)).await;

    let response = http()
        .post(format!("{}/v1/ws/sessions/disconnect", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&json!({
            "session_ids": [session_id],
            "reason": "session handed to another operator"
        }))
        .send()
        .await
        .expect("disconnect");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("disconnect json");
    assert_eq!(body["data"]["requested"], 1);

    let payload = await_disconnect_payload(&harness, &connection_id).await;
    assert_eq!(payload["reason"], "operator_disconnect");
    assert_eq!(
        payload["operator_reason"],
        "session handed to another operator"
    );
}

/// Poll `/v1/ws/connections` until a connection carrying every topic in
/// `required_topics` is listed, and return its id. Neither the registry insert
/// nor the subscribe frame is observable the moment the client call returns —
/// both are processed on the server's connection task.
async fn await_ws_connection_id(harness: &Harness, required_topics: &[String]) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let listing: Value = http()
            .get(format!("{}/v1/ws/connections", harness.base_url))
            .header("Authorization", session_bearer())
            .send()
            .await
            .expect("ws connections")
            .json()
            .await
            .expect("ws connections json");
        let matched = listing["data"]["connections"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|connection| {
                let topics: Vec<&str> = connection["topics"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                required_topics
                    .iter()
                    .all(|required| topics.contains(&required.as_str()))
            })
            .and_then(|connection| connection["connection_id"].as_str())
            .map(str::to_owned);
        if let Some(connection_id) = matched {
            return connection_id;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "websocket connection never registered"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Poll the durable event log for the `ws.client_disconnected` row belonging to
/// `connection_id` and return its payload.
async fn await_disconnect_payload(harness: &Harness, connection_id: &str) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        {
            let state = harness.state.lock().await;
            let events = state
                .query_events(acp_stack::state::LogFilter {
                    limit: 50,
                    kind: Some("ws.client_disconnected"),
                    ..acp_stack::state::LogFilter::default()
                })
                .expect("query ws lifecycle events");
            let matched = events.iter().find_map(|event| {
                let payload: Value =
                    serde_json::from_str(&event.payload_json).expect("payload json");
                (payload["connection_id"] == connection_id).then_some(payload)
            });
            if let Some(payload) = matched {
                return payload;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "ws.client_disconnected was never persisted for {connection_id}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
