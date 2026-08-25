#![cfg(feature = "test-fixtures")]

//! Cancellation correctness for `POST /v1/sessions/{id}/cancel`: the route
//! succeeds only when the agent actually settles the live turn as cancelled,
//! and a session may only carry one prompt at a time.

mod common;

use std::time::Duration;

use common::sessions::{Harness, create_session, http, prompt_count_for_session, session_bearer};
use reqwest::StatusCode;
use serde_json::{Value, json};

/// Comfortably above the placebo's own settle delays and well below the
/// supervisor's cancel budget, so a poll that runs out is a real failure.
const POLL_BUDGET: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(50);
/// The supervisor waits 20s for a settle before failing the cancel; the tests
/// that exercise the timeout need a longer leash than `POLL_BUDGET`.
const CANCEL_TIMEOUT_BUDGET: Duration = Duration::from_secs(40);
/// Comfortably above the placebo's 100ms settle delay: a turn still running after
/// this margin proves it did not inherit an earlier cancel's marker.
const AUTO_CANCEL_MARGIN: Duration = Duration::from_millis(600);

#[tokio::test]
async fn cancel_waits_for_a_delayed_agent_acknowledgement() {
    let harness = Harness::spawn_with(|config| {
        config.agent.args.extend([
            "--prompt-settle-cancel-after-ms".to_owned(),
            "300".to_owned(),
        ]);
    })
    .await;
    let session_id = create_session(&harness).await;
    let prompt_id = submit_prompt(&harness, &session_id, "cancel me").await;
    await_agent_entered_the_turn(&harness, &session_id).await;

    let cancel = cancel_session(&harness, &session_id).await;
    assert_eq!(cancel.status(), StatusCode::OK);

    // Read the row without polling: a successful cancel must not return before
    // the agent settled the turn.
    let record = prompt_record(&harness, &session_id, &prompt_id).await;
    assert_eq!(record["status"], "cancelled", "record = {record}");
    assert_eq!(record["stop_reason"], "cancelled", "record = {record}");
    // Only the agent's own prompt response carries the message id back, so an
    // acknowledged id proves the agent settled the turn rather than the
    // supervisor writing `cancelled` on its own.
    assert_eq!(record["message_id_acknowledged"], true, "record = {record}");
    assert!(
        session_event_kinds(&harness, &session_id)
            .await
            .contains(&"session.cancel_requested".to_owned())
    );
}

#[tokio::test]
async fn cancel_fails_when_the_agent_never_settles_the_turn() {
    let harness = Harness::spawn_with(|config| {
        config.agent.args.push("--prompt-never-settle".to_owned());
    })
    .await;
    let session_id = create_session(&harness).await;
    let prompt_id = submit_prompt(&harness, &session_id, "ignored cancel").await;
    await_agent_entered_the_turn(&harness, &session_id).await;

    let cancel = tokio::time::timeout(CANCEL_TIMEOUT_BUDGET, cancel_session(&harness, &session_id))
        .await
        .expect("cancel returned within the settle budget");
    assert_eq!(cancel.status(), StatusCode::BAD_GATEWAY);
    let body: Value = cancel.json().await.expect("cancel json");
    assert_eq!(body["error"]["code"], "agent.request_failed", "{body}");

    let record = prompt_record(&harness, &session_id, &prompt_id).await;
    assert_ne!(record["status"], "cancelled", "record = {record}");
    assert_eq!(record["status"], "running", "record = {record}");

    // The failed cancel left the turn live, so its handle is still registered
    // and a fresh prompt for the session is refused.
    let rejected = post_prompt(&harness, &session_id, "second turn").await;
    assert_eq!(rejected.status(), StatusCode::CONFLICT);
    let body: Value = rejected.json().await.expect("rejected json");
    assert_eq!(body["error"]["code"], "session.prompt_in_flight", "{body}");
}

#[tokio::test]
async fn cancel_fails_when_the_agent_finishes_the_turn_instead() {
    let harness = Harness::spawn_with(|config| {
        config
            .agent
            .args
            .extend(["--prompt-response-delay-ms".to_owned(), "1500".to_owned()]);
    })
    .await;
    let session_id = create_session(&harness).await;
    let prompt_id = submit_prompt(&harness, &session_id, "runs to completion").await;
    // This placebo mode parks the dispatch loop for the delay, so it reads the
    // cancel only after answering the turn on its own terms.
    await_agent_entered_the_turn(&harness, &session_id).await;

    let cancel = tokio::time::timeout(CANCEL_TIMEOUT_BUDGET, cancel_session(&harness, &session_id))
        .await
        .expect("cancel returned within the settle budget");
    assert_eq!(cancel.status(), StatusCode::BAD_GATEWAY);
    let body: Value = cancel.json().await.expect("cancel json");
    assert_eq!(body["error"]["code"], "agent.request_failed", "{body}");

    let record = prompt_record(&harness, &session_id, &prompt_id).await;
    assert_eq!(record["status"], "completed", "record = {record}");
    assert_eq!(record["stop_reason"], "end_turn", "record = {record}");
}

#[tokio::test]
async fn a_second_prompt_is_rejected_while_the_turn_is_live() {
    let harness = Harness::spawn_with(|config| {
        config.agent.args.push("--prompt-never-settle".to_owned());
    })
    .await;
    let session_id = create_session(&harness).await;
    submit_prompt(&harness, &session_id, "first turn").await;

    let rejected = post_prompt(&harness, &session_id, "second turn").await;
    assert_eq!(rejected.status(), StatusCode::CONFLICT);
    let body: Value = rejected.json().await.expect("rejected json");
    assert_eq!(body["error"]["code"], "session.prompt_in_flight", "{body}");
    assert_eq!(
        prompt_count_for_session(&harness, &session_id).await,
        1,
        "the rejected prompt must not have created a row"
    );
}

#[tokio::test]
async fn a_prompt_after_a_successful_cancel_runs_until_its_own_cancel() {
    let harness = Harness::spawn_with(|config| {
        config.agent.args.extend([
            "--prompt-settle-cancel-after-ms".to_owned(),
            "100".to_owned(),
        ]);
    })
    .await;
    let session_id = create_session(&harness).await;
    submit_prompt(&harness, &session_id, "first turn").await;
    await_agent_entered_the_turn(&harness, &session_id).await;

    let cancel = cancel_session(&harness, &session_id).await;
    assert_eq!(cancel.status(), StatusCode::OK);

    // The first cancel settled the first turn and consumed its marker. A fresh prompt
    // is accepted and must NOT inherit that cancellation: it runs until a cancel of its
    // own. A marker left sticky would settle it as cancelled with no cancel targeting
    // it, so confirm it is still running past the placebo's 100ms settle delay.
    let second = submit_prompt(&harness, &session_id, "second turn").await;
    assert_eq!(prompt_count_for_session(&harness, &session_id).await, 2);
    await_prompt_status(&harness, &session_id, &second, "running").await;
    tokio::time::sleep(AUTO_CANCEL_MARGIN).await;
    let record = prompt_record(&harness, &session_id, &second).await;
    assert_eq!(record["status"], "running", "record = {record}");

    // A cancel aimed at the second turn settles it in kind, proving cancellation is
    // per-request rather than a one-time latch on the session.
    let cancel = cancel_session(&harness, &session_id).await;
    assert_eq!(cancel.status(), StatusCode::OK);
    let record = prompt_record(&harness, &session_id, &second).await;
    assert_eq!(record["status"], "cancelled", "record = {record}");
}

#[tokio::test]
async fn a_stalled_turn_does_not_block_cancelling_the_live_one() {
    let harness = Harness::spawn_with(|config| {
        config.agent.args.extend([
            "--prompt-settle-cancel-after-ms".to_owned(),
            "100".to_owned(),
        ]);
    })
    .await;
    let session_id = create_session(&harness).await;
    let stalled_prompt_id = submit_prompt(&harness, &session_id, "first turn").await;
    await_agent_entered_the_turn(&harness, &session_id).await;
    {
        let store = harness.state.lock().await;
        let stalled = store
            .mark_stalled_prompts(Duration::from_secs(0), "test forced stall")
            .expect("mark stalled");
        assert_eq!(stalled.len(), 1, "only the first turn should be swept");
    }

    // A stalled prompt is terminal, so the documented recovery (submit a new
    // prompt) works even though the swept turn's task is still parked.
    let live_prompt_id = submit_prompt(&harness, &session_id, "second turn").await;
    await_prompt_status(&harness, &session_id, &live_prompt_id, "running").await;

    let cancel = cancel_session(&harness, &session_id).await;
    assert_eq!(cancel.status(), StatusCode::OK);
    let live = prompt_record(&harness, &session_id, &live_prompt_id).await;
    assert_eq!(live["status"], "cancelled", "record = {live}");
    let stalled = prompt_record(&harness, &session_id, &stalled_prompt_id).await;
    assert_eq!(stalled["status"], "stalled", "record = {stalled}");
}

async fn post_prompt(harness: &Harness, session_id: &str, text: &str) -> reqwest::Response {
    http()
        .post(format!(
            "{}/v1/sessions/{}/prompt",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({ "prompt": text }))
        .send()
        .await
        .expect("submit prompt")
}

async fn submit_prompt(harness: &Harness, session_id: &str, text: &str) -> String {
    let response = post_prompt(harness, session_id, text).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("submit json");
    body["data"]["prompt_id"]
        .as_str()
        .expect("prompt id")
        .to_owned()
}

async fn cancel_session(harness: &Harness, session_id: &str) -> reqwest::Response {
    http()
        .post(format!(
            "{}/v1/sessions/{}/cancel",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("cancel request")
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

async fn session_event_kinds(harness: &Harness, session_id: &str) -> Vec<String> {
    let body: Value = http()
        .get(format!(
            "{}/v1/sessions/{}/events",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("session events")
        .json()
        .await
        .expect("session events json");
    body["data"]["events"]
        .as_array()
        .expect("events array")
        .iter()
        .filter_map(|event| event["kind"].as_str().map(str::to_owned))
        .collect()
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

/// Block until the placebo has persisted its first `session/update` chunk. That
/// proves the agent read the prompt request, which fixes the ordering between
/// the turn and the `session/cancel` notification the test sends next.
async fn await_agent_entered_the_turn(harness: &Harness, session_id: &str) {
    let deadline = tokio::time::Instant::now() + POLL_BUDGET;
    loop {
        if session_event_kinds(harness, session_id)
            .await
            .iter()
            .any(|kind| kind == "session.update")
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "agent never emitted a session update for `{session_id}`"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}
