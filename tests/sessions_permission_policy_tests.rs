#![cfg(feature = "test-fixtures")]

//! `permissions.acp_prompt_action` at the route level: an agent's
//! `session/request_permission` is either answered as it arrives or left for a
//! decision, and the turn follows accordingly.

mod common;

use std::time::Duration;

use common::sessions::{Harness, create_session, http, session_bearer};
use reqwest::StatusCode;
use serde_json::{Value, json};

/// Comfortably above the placebo's own delays: a turn that has not settled by
/// then is a real failure, not a slow box.
const POLL_BUDGET: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(50);
/// Long enough to prove a request under `ask` is still waiting rather than
/// merely slow to be answered.
const PENDING_MARGIN: Duration = Duration::from_millis(600);

/// Under `approve`, the agent's request is decided on arrival, so a turn that
/// parks on a permission runs to completion with no operator in the loop.
#[tokio::test]
async fn an_agent_permission_is_answered_on_arrival_and_the_turn_proceeds() {
    let harness = Harness::spawn_with(|config| {
        config
            .agent
            .args
            .push("--prompt-await-permission".to_owned());
        config.permissions.acp_prompt_action = Some("approve".to_owned());
    })
    .await;
    let session_id = create_session(&harness).await;
    let prompt_id = submit_prompt(&harness, &session_id, "ask before acting").await;

    await_prompt_status(&harness, &session_id, &prompt_id, "completed").await;
    let record = prompt_record(&harness, &session_id, &prompt_id).await;
    assert_eq!(record["stop_reason"], "end_turn", "record = {record}");
    assert!(
        pending_permissions(&harness).await.is_empty(),
        "an answered request never waits"
    );

    let approved = permission_events(&harness, "permission.approved").await;
    assert_eq!(approved.len(), 1, "{approved:?}");
    assert_eq!(approved[0]["deciding_principal"], "policy", "{approved:?}");
    assert_eq!(
        approved[0]["reason"], "auto-approved by policy",
        "{approved:?}"
    );
}

/// The default leaves the request for a decision, so the turn stays open until
/// one arrives. This is the behavior an absent knob preserves.
#[tokio::test]
async fn an_agent_permission_waits_for_a_decision_by_default() {
    let harness = Harness::spawn_with(|config| {
        config
            .agent
            .args
            .push("--prompt-await-permission".to_owned());
    })
    .await;
    let session_id = create_session(&harness).await;
    let prompt_id = submit_prompt(&harness, &session_id, "ask before acting").await;

    let permission_id = await_pending_permission(&harness, &session_id).await;
    tokio::time::sleep(PENDING_MARGIN).await;
    let record = prompt_record(&harness, &session_id, &prompt_id).await;
    assert_eq!(record["status"], "running", "record = {record}");

    // The operator's approval is what releases the turn.
    let approve = http()
        .post(format!(
            "{}/v1/permissions/{permission_id}/approve",
            harness.base_url
        ))
        .header("Authorization", session_bearer())
        .json(&json!({}))
        .send()
        .await
        .expect("approve request");
    assert_eq!(approve.status(), StatusCode::OK);
    await_prompt_status(&harness, &session_id, &prompt_id, "completed").await;
}

async fn submit_prompt(harness: &Harness, session_id: &str, text: &str) -> String {
    let response = http()
        .post(format!(
            "{}/v1/sessions/{}/prompt",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({ "prompt": text }))
        .send()
        .await
        .expect("submit prompt");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("submit json");
    body["data"]["prompt_id"]
        .as_str()
        .expect("prompt id")
        .to_owned()
}

async fn prompt_record(harness: &Harness, session_id: &str, prompt_id: &str) -> Value {
    let body: Value = http()
        .get(format!(
            "{}/v1/sessions/{}/prompts/{}",
            harness.base_url, session_id, prompt_id
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("prompt status")
        .json()
        .await
        .expect("prompt status json");
    body["data"].clone()
}

async fn await_prompt_status(harness: &Harness, session_id: &str, prompt_id: &str, want: &str) {
    let deadline = tokio::time::Instant::now() + POLL_BUDGET;
    loop {
        let record = prompt_record(harness, session_id, prompt_id).await;
        if record["status"] == want {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "prompt `{prompt_id}` never reached `{want}`: {record}"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn pending_permissions(harness: &Harness) -> Vec<Value> {
    let body: Value = http()
        .get(format!("{}/v1/permissions/pending", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("pending permissions")
        .json()
        .await
        .expect("pending permissions json");
    body["data"]["permissions"]
        .as_array()
        .expect("permissions array")
        .clone()
}

async fn await_pending_permission(harness: &Harness, session_id: &str) -> String {
    let deadline = tokio::time::Instant::now() + POLL_BUDGET;
    loop {
        let pending = pending_permissions(harness).await;
        if let Some(row) = pending
            .iter()
            .find(|row| row["source"] == "acp" && row["subject_id"].as_str() == Some(session_id))
        {
            return row["id"].as_str().expect("permission id").to_owned();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no pending permission for `{session_id}`"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Parsed payloads of the durable permission events of one kind.
async fn permission_events(harness: &Harness, kind: &str) -> Vec<Value> {
    let body: Value = http()
        .get(format!(
            "{}/v1/logs/permissions?kind={kind}",
            harness.base_url
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("permission events")
        .json()
        .await
        .expect("permission events json");
    body["data"]["events"]
        .as_array()
        .expect("events array")
        .iter()
        .map(|event| {
            serde_json::from_str(event["payload_json"].as_str().expect("payload"))
                .expect("payload json")
        })
        .collect()
}
