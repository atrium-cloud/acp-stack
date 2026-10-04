#![cfg(feature = "test-fixtures")]

//! Stale-prompt coverage against a live placebo agent: a long-running tool call
//! is not stale, the agent's own result replaces a `stalled` verdict, and an
//! adapter that dies mid-tool still settles its turn. The harness runs no
//! sweeper task, so each test drives the sweep directly.

mod common;

use std::time::Duration;

use acp_stack::state::{PromptRecord, PromptStaleThresholds};
use common::sessions::{Harness, create_session, http, session_bearer};
use common::state::uniform_stale_thresholds;
use reqwest::StatusCode;
use serde_json::{Value, json};

const TOOL_CALL_HOLD_MS: &str = "1500";
const POLL_BUDGET: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(25);
const SWEEP_REASON: &str = "test sweep";

/// Every in-flight prompt is past `quiet`, while an open tool call still holds
/// it to an hour.
fn open_tool_call_thresholds() -> PromptStaleThresholds {
    PromptStaleThresholds {
        quiet: Duration::ZERO,
        open_tool_call: Duration::from_secs(3_600),
    }
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

/// Block until the agent's `tool_call` notification is durable, so a sweep
/// that follows sees the open call.
async fn await_tool_call_persisted(harness: &Harness, session_id: &str) {
    let deadline = tokio::time::Instant::now() + POLL_BUDGET;
    loop {
        let events = {
            let state = harness.state.lock().await;
            state
                .query_session_events(session_id, None, 100)
                .expect("session events")
        };
        let persisted = events.iter().any(|event| {
            event.source == "acp"
                && serde_json::from_str::<Value>(&event.payload_json)
                    .is_ok_and(|payload| payload["update"]["sessionUpdate"] == "tool_call")
        });
        if persisted {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "tool_call never persisted: {events:?}"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn await_terminal_prompt(harness: &Harness, prompt_id: &str, want: &str) -> PromptRecord {
    let deadline = tokio::time::Instant::now() + POLL_BUDGET;
    loop {
        let record = {
            let state = harness.state.lock().await;
            state
                .get_prompt(prompt_id)
                .expect("prompt lookup")
                .expect("prompt exists")
        };
        if record.status == want {
            return record;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "prompt never reached `{want}`: {record:?}"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn session_event_kinds(harness: &Harness, session_id: &str) -> Vec<(String, Value)> {
    let state = harness.state.lock().await;
    state
        .query_session_events(session_id, None, 200)
        .expect("session events")
        .into_iter()
        .map(|event| {
            let payload = serde_json::from_str(&event.payload_json).expect("payload json");
            (event.kind, payload)
        })
        .collect()
}

#[tokio::test]
async fn a_silent_tool_call_keeps_its_prompt_from_stalling() {
    let harness = Harness::spawn_with(|config| {
        config.agent.args.extend([
            "--prompt-tool-call-hold-ms".to_owned(),
            TOOL_CALL_HOLD_MS.to_owned(),
        ]);
    })
    .await;
    let session_id = create_session(&harness).await;
    let prompt_id = submit_prompt(&harness, &session_id, "run a long command").await;
    await_tool_call_persisted(&harness, &session_id).await;

    {
        let state = harness.state.lock().await;
        let (stuck, _) = state
            .count_stuck_prompts(open_tool_call_thresholds())
            .expect("count stuck prompts");
        assert_eq!(stuck, 0, "an open tool call must not read as stuck");
        let stalled = state
            .mark_stalled_prompts(open_tool_call_thresholds(), SWEEP_REASON)
            .expect("sweep");
        assert!(stalled.is_empty(), "swept {stalled:?}");
    }

    let record = await_terminal_prompt(&harness, &prompt_id, "completed").await;
    assert_eq!(record.stop_reason.as_deref(), Some("end_turn"));
    let kinds = session_event_kinds(&harness, &session_id).await;
    assert!(
        kinds
            .iter()
            .all(|(kind, _)| kind != "prompt.stall_resolved"),
        "{kinds:?}"
    );
}

#[tokio::test]
async fn the_agent_result_replaces_a_stall() {
    let harness = Harness::spawn_with(|config| {
        config.agent.args.extend([
            "--prompt-tool-call-hold-ms".to_owned(),
            TOOL_CALL_HOLD_MS.to_owned(),
        ]);
    })
    .await;
    let session_id = create_session(&harness).await;
    let prompt_id = submit_prompt(&harness, &session_id, "run a long command").await;
    await_tool_call_persisted(&harness, &session_id).await;

    {
        let state = harness.state.lock().await;
        let stalled = state
            .mark_stalled_prompts(uniform_stale_thresholds(Duration::ZERO), SWEEP_REASON)
            .expect("sweep");
        assert_eq!(stalled.len(), 1, "swept {stalled:?}");
        assert_eq!(stalled[0].prompt_id, prompt_id);
    }

    let record = await_terminal_prompt(&harness, &prompt_id, "completed").await;
    assert_eq!(record.stop_reason.as_deref(), Some("end_turn"));
    assert!(record.failure_class.is_none(), "{record:?}");
    assert!(record.error_code.is_none(), "{record:?}");
    let kinds = session_event_kinds(&harness, &session_id).await;
    let resolved: Vec<&Value> = kinds
        .iter()
        .filter(|(kind, _)| kind == "prompt.stall_resolved")
        .map(|(_, payload)| payload)
        .collect();
    assert_eq!(resolved.len(), 1, "{kinds:?}");
    assert_eq!(resolved[0]["prompt_id"], prompt_id.as_str());
    assert_eq!(resolved[0]["status"], "completed");
    assert_eq!(resolved[0]["stop_reason"], "end_turn");
}

#[tokio::test]
async fn an_adapter_that_exits_mid_tool_settles_its_prompt() {
    let harness = Harness::spawn_with(|config| {
        config.agent.restart = "never".to_owned();
        config
            .agent
            .args
            .push("--prompt-tool-call-then-exit".to_owned());
    })
    .await;
    let session_id = create_session(&harness).await;
    let prompt_id = submit_prompt(&harness, &session_id, "run a command and crash").await;

    // No sweep runs here, so only adapter death can settle the turn.
    let record = await_terminal_prompt(&harness, &prompt_id, "errored").await;
    assert_ne!(
        record.failure_class.as_deref(),
        Some("stalled"),
        "{record:?}"
    );
}
