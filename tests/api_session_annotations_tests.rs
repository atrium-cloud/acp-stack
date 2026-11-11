//! `POST /v1/sessions/-/annotations` on the in-process server.

use reqwest::StatusCode;
use serde_json::{Value, json};

use acp_stack::state::{
    EVENT_KIND_SESSION_ANNOTATION, EVENT_SOURCE_API, NewPromptRecord, NewSessionRecord,
    PromptStatus,
};

mod common;
use common::api::{ADMIN_KEY, SESSION_KEY, ServerHarness};

const ANNOTATIONS_PATH: &str = "/v1/sessions/-/annotations";

/// A session under `agent_session_id` with one prompt in `status`.
async fn seed_session(
    harness: &ServerHarness,
    session_id: &'static str,
    agent_session_id: &'static str,
    status: PromptStatus,
) {
    harness
        .state
        .write(move |store| {
            store.insert_session_for_target(
                "opencode",
                agent_session_id.to_owned(),
                NewSessionRecord {
                    id: session_id.to_owned(),
                    agent_id: "opencode".to_owned(),
                    cwd: format!("/tmp/{session_id}"),
                    title: None,
                    metadata_json: "{}".to_owned(),
                },
            )?;
            let prompt_id = acp_stack::state::next_prompt_id();
            store.insert_prompt(NewPromptRecord {
                id: prompt_id.clone(),
                session_id: session_id.to_owned(),
                prompt_json: "[]".to_owned(),
            })?;
            store.update_prompt_status(&prompt_id, status, None, None, None, None, None)?;
            Ok(())
        })
        .await
        .expect("session seeded");
}

async fn annotate(harness: &ServerHarness, bearer: &str, body: Value) -> (StatusCode, Value) {
    let response = reqwest::Client::new()
        .post(format!("{}{ANNOTATIONS_PATH}", harness.base_url))
        .bearer_auth(bearer)
        .json(&body)
        .send()
        .await
        .expect("send");
    let status = response.status();
    (status, response.json().await.expect("envelope"))
}

fn annotation(agent_session_id: &str, kind: &str, data: Value) -> Value {
    json!({ "agent_session_id": agent_session_id, "kind": kind, "data": data })
}

#[tokio::test]
async fn an_annotation_is_logged_on_the_turn_in_flight_under_its_own_kind() {
    let harness = ServerHarness::spawn().await;
    seed_session(&harness, "sess_live", "agent_live", PromptStatus::Running).await;
    let data = json!({ "verb": "create", "task_id": "task_1", "title": "Inbox digest" });

    // A caller names the annotation's kind, never the row's: this one is no settle.
    let (status, answer) = annotate(
        &harness,
        SESSION_KEY,
        annotation("agent_live", "prompt.settled", data.clone()),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["data"]["session_id"], "sess_live");
    let event = &answer["data"]["event"];
    assert_eq!(event["kind"], EVENT_KIND_SESSION_ANNOTATION);
    assert_eq!(event["source"], EVENT_SOURCE_API);
    let events: Value = reqwest::Client::new()
        .get(format!("{}/v1/sessions/sess_live/events", harness.base_url))
        .bearer_auth(SESSION_KEY)
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("envelope");
    let logged = events["data"]["events"]
        .as_array()
        .and_then(|events| events.last())
        .expect("the session logs the annotation");
    assert_eq!(logged["id"], event["id"]);
    assert_eq!(logged["seq"], event["seq"]);
    let payload: Value = serde_json::from_str(logged["payload_json"].as_str().expect("payload"))
        .expect("payload JSON");
    assert_eq!(payload, json!({ "kind": "prompt.settled", "data": data }));
}

#[tokio::test]
async fn the_route_refuses_an_annotation_it_cannot_place_or_accept() {
    let harness = ServerHarness::spawn().await;
    seed_session(&harness, "sess_live", "agent_live", PromptStatus::Running).await;
    seed_session(
        &harness,
        "sess_settled",
        "agent_settled",
        PromptStatus::Completed,
    )
    .await;
    let data = json!({ "verb": "pause", "task_id": "task_1", "title": "Inbox digest" });
    for (bearer, body, expected_status, expected_code) in [
        (
            ADMIN_KEY,
            annotation("agent_live", "thekit.schedule", data.clone()),
            StatusCode::UNAUTHORIZED,
            "auth.wrong_kind",
        ),
        (
            SESSION_KEY,
            annotation("agent_unknown", "thekit.schedule", data.clone()),
            StatusCode::NOT_FOUND,
            "session.not_found",
        ),
        (
            SESSION_KEY,
            annotation("agent_settled", "thekit.schedule", data.clone()),
            StatusCode::CONFLICT,
            "session.no_turn_in_flight",
        ),
        (
            SESSION_KEY,
            annotation("agent_live", "thekit", data.clone()),
            StatusCode::BAD_REQUEST,
            "request.invalid_param",
        ),
        (
            SESSION_KEY,
            annotation("agent_live", "thekit.schedule", json!("pause")),
            StatusCode::BAD_REQUEST,
            "request.invalid_param",
        ),
    ] {
        let (status, answer) = annotate(&harness, bearer, body.clone()).await;
        assert_eq!(status, expected_status, "{body}: {answer}");
        assert_eq!(answer["error"]["code"], expected_code, "{body}: {answer}");
    }
}
