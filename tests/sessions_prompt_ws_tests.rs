#![cfg(feature = "test-fixtures")]

//! Prompt-path and websocket coverage for the session routes: the modality gate, `/v1/ws` event fanout, and the persisted prompt failure taxonomy.

mod common;

use std::time::Duration;

use acp_stack::config::ArrayTargetConfig;
use acp_stack::events::EVENT_CHANNEL_CAPACITY;
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
        // The accepted user prompt streams on this topic under the same kind,
        // so the agent's own chunk is selected by its update discriminator.
        if event["type"] == "event"
            && event["topic"] == format!("sessions.{session_id}")
            && event["payload"]["kind"] == "session.update"
            && event["payload"]["data"]["update"]["sessionUpdate"] == "agent_message_chunk"
        {
            received = Some(event);
            break;
        }
    }
    let event = received.expect("session.update websocket event");
    assert_eq!(event["payload"]["source"], "acp", "event = {event}");
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

    // One subscriber per topic: the guarded-against bug dropped session-topic delivery while
    // logs-topic delivery still worked.
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

    // The WS server handles subscribe frames in the same select! arm as event fanout, so a state
    // write landing before the subscribe frame is observed is dropped on the broadcast end.
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

    // Direct state write so the assertion targets the publish site, not the bridge plumbing.
    let appended = {
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
            .expect("event inserted")
    };
    let appended_seq = appended.seq.expect("session event carries seq");

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
    assert_eq!(session_event["seq"], appended_seq);

    let logs_event = recv_matching_event(&mut logs_ws, "logs", "session.update")
        .await
        .expect("session.update on logs topic");
    assert_eq!(logs_event["payload"]["data"]["kind"], "session.update");
    assert_eq!(logs_event["seq"], appended_seq);
}

/// The raw upstream message, including the URL and secret-looking token below, must never reach the persisted `error_message`, `failure_detail_json`, or event payload.
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

    // The persisted error_message must NOT contain any portion of the raw upstream string.
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

    // Same invariant for `failure_detail_json` and `error_code`.
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
    assert!(!inference_event.message.contains("openai"));
    assert!(!inference_event.message.contains("sk-secret"));
    assert!(!inference_event.payload_json.contains("openai"));
    assert!(!inference_event.payload_json.contains("sk-secret"));
}

#[tokio::test]
async fn late_agent_failure_replaces_stalled_prompt() {
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
            .mark_stalled_prompts(
                common::state::uniform_stale_thresholds(Duration::ZERO),
                "test forced stall",
            )
            .expect("mark stalled");
        assert!(
            stalled.iter().any(|prompt| prompt.prompt_id == prompt_id),
            "forced stall should include submitted prompt, got {stalled:?}"
        );
    }

    tokio::time::sleep(Duration::from_millis(DELAY_MS + 250)).await;

    let state = harness.state.lock().await;
    let prompt = state
        .get_prompt(&prompt_id)
        .expect("prompt lookup")
        .expect("prompt exists");
    assert_eq!(prompt.status, "errored");
    assert_eq!(
        prompt.failure_class.as_deref(),
        Some(acp_stack::state::FailureClass::Inference5xx.as_str())
    );
    let events = state
        .query_session_events(&session_id, None, 100)
        .expect("session events");
    drop(state);
    // The direct sweep call writes no `prompt.stalled` row; the sweeper task does.
    let kinds: Vec<&str> = events.iter().map(|event| event.kind.as_str()).collect();
    let resolved_at = kinds
        .iter()
        .position(|kind| *kind == "prompt.stall_resolved");
    let failed_at = kinds
        .iter()
        .position(|kind| *kind == "prompt.inference_failed");
    assert!(
        matches!((resolved_at, failed_at), (Some(resolved), Some(failed)) if resolved < failed),
        "the stall resolution must precede the agent's failure event, got {kinds:?}"
    );
    let resolved: Value =
        serde_json::from_str(&events[resolved_at.expect("resolution event")].payload_json)
            .expect("resolution payload");
    assert_eq!(resolved["prompt_id"], prompt_id.as_str());
    assert_eq!(resolved["status"], "errored");
}

const PROMPT_USAGE_JSON: &str = r#"{"totalTokens":1500,"inputTokens":200,"outputTokens":300,"cachedReadTokens":900,"cachedWriteTokens":100}"#;

/// Submit a prompt and wait for it to settle, returning its id and final status.
async fn submit_and_settle(harness: &Harness, session_id: &str, text: &str) -> (String, String) {
    let submit: Value = http()
        .post(format!(
            "{}/v1/sessions/{}/prompt",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({ "prompt": text }))
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
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        assert!(std::time::Instant::now() < deadline, "prompt never settled");
        let state = harness.state.lock().await;
        let status = state
            .get_prompt(&prompt_id)
            .expect("prompt lookup")
            .map(|record| record.status);
        drop(state);
        if let Some(status) = status
            && !matches!(status.as_str(), "pending" | "running")
        {
            return (prompt_id, status);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn prompt_usage_is_recorded_after_the_turn() {
    let harness = Harness::spawn_with(|config| {
        config
            .agent
            .args
            .extend(["--prompt-usage".into(), PROMPT_USAGE_JSON.into()]);
    })
    .await;
    let session_id = create_session(&harness).await;

    let (prompt_id, status) = submit_and_settle(&harness, &session_id, "count my tokens").await;
    assert_eq!(status, "completed");

    let state = harness.state.lock().await;
    let events = state
        .query_session_events(&session_id, None, 100)
        .expect("session events");
    drop(state);
    let usage_positions: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, event)| event.kind == "prompt.usage_reported")
        .map(|(position, _)| position)
        .collect();
    assert_eq!(usage_positions.len(), 1, "events = {events:?}");
    let last_agent_update = events
        .iter()
        .rposition(|event| event.kind == "session.update" && event.source == "acp")
        .expect("agent streamed the turn");
    assert!(
        usage_positions[0] > last_agent_update,
        "usage must follow the turn's last chunk: {events:?}"
    );
    let usage_event = &events[usage_positions[0]];
    assert_eq!(usage_event.source, "acp");
    let payload: Value = serde_json::from_str(&usage_event.payload_json).expect("payload json");
    assert_eq!(
        payload,
        json!({
            "prompt_id": prompt_id,
            "total_tokens": 1500,
            "input_tokens": 200,
            "output_tokens": 300,
            "cached_read_tokens": 900,
            "cached_write_tokens": 100,
        })
    );
}

/// The usage row lands after the turn's last write, so its source decides who
/// the status view says acted last.
#[tokio::test]
async fn prompt_usage_leaves_the_agent_as_last_actor() {
    let harness = Harness::spawn_with(|config| {
        config
            .agent
            .args
            .extend(["--prompt-usage".into(), PROMPT_USAGE_JSON.into()]);
    })
    .await;
    let session_id = create_session(&harness).await;

    let (_, status) = submit_and_settle(&harness, &session_id, "who spoke last").await;
    assert_eq!(status, "completed");

    let body: Value = http()
        .get(format!("{}/v1/sessions/-/status", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("status")
        .json()
        .await
        .expect("status json");
    let session = body["data"]["sessions"]
        .as_array()
        .expect("sessions array")
        .iter()
        .find(|session| session["id"] == session_id.as_str())
        .expect("session in status");
    assert_eq!(session["last_activity_from"], "agent", "{session}");
}

#[tokio::test]
async fn prompt_without_usage_records_no_usage_event() {
    let harness = Harness::spawn().await;
    let session_id = create_session(&harness).await;

    let (_, status) = submit_and_settle(&harness, &session_id, "no usage here").await;
    assert_eq!(status, "completed");

    let state = harness.state.lock().await;
    let events = state
        .query_session_events(&session_id, None, 100)
        .expect("session events");
    drop(state);
    assert!(
        events
            .iter()
            .all(|event| event.kind != "prompt.usage_reported"),
        "events = {events:?}"
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
    // Separate fields: `reason` is a closed vocabulary, `operator_reason` is free-form.
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

#[tokio::test(flavor = "current_thread")]
async fn lagging_subscriber_is_closed_to_reconnect() {
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

    // The test runtime is single-threaded, so the connection task cannot drain
    // the channel while this burst holds the thread: every write lands unread,
    // and one past capacity puts the subscriber behind.
    {
        let store = harness.state.lock().await;
        for sequence in 0..=EVENT_CHANNEL_CAPACITY {
            store
                .append_session_event_with_source(
                    &session_id,
                    "info",
                    "session.update",
                    acp_stack::state::EVENT_SOURCE_ACP,
                    "ACP session update",
                    &format!(r#"{{"seq":{sequence}}}"#),
                )
                .expect("event inserted");
        }
    }

    let close = await_close_frame(&mut ws).await;
    assert_eq!(
        close.code,
        tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Again
    );
    assert_eq!(close.reason.as_str(), "lagged");

    let payload = await_disconnect_payload(&harness, &connection_id).await;
    assert_eq!(payload["reason"], "lagged");
}

/// Poll `/v1/ws/connections` until a connection carrying every `required_topics` entry is listed. Neither the registry insert nor the subscribe frame is observable when the client call returns; both run on the server's connection task.
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

/// Read until the server's close frame, skipping any event frames sent before it.
async fn await_close_frame(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> tokio_tungstenite::tungstenite::protocol::CloseFrame {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let message = tokio::time::timeout(remaining, ws.next())
            .await
            .expect("close frame before timeout")
            .expect("socket open until the close frame")
            .expect("ws message ok");
        if let tokio_tungstenite::tungstenite::Message::Close(frame) = message {
            return frame.expect("close frame carries a code");
        }
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
