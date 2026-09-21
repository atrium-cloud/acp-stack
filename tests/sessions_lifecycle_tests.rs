#![cfg(feature = "test-fixtures")]

//! Session lifecycle coverage: create/fork/load/resume/close, cwd containment,
//! and the full create → list → get → prompt → poll → close round trip.

mod common;

use std::time::Duration;

use acp_stack::config::{
    AgentConfigOptionValue, ArrayTargetConfig, Config, McpHttpServer, McpServerConfig,
    McpStdioServer,
};
use acp_stack::secrets::SecretStore;
use acp_stack::state::{NewPromptRecord, NewSessionRecord};
use common::sessions::{Harness, admin_bearer, create_session, http, session_bearer};
use reqwest::StatusCode;
use serde_json::{Value, json};

#[tokio::test]
async fn create_session_accepts_existing_cwd_under_workspace() {
    let harness = Harness::spawn().await;
    let inner = harness.workspace_root.join("inner");
    std::fs::create_dir(&inner).expect("inner dir");
    let response = http()
        .post(format!("{}/v1/sessions", harness.base_url))
        .header("Authorization", session_bearer())
        .json(&json!({ "cwd": inner.to_string_lossy() }))
        .send()
        .await
        .expect("create");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    let canonical_inner = inner.canonicalize().expect("canonical inner");
    assert_eq!(
        body["data"]["cwd"],
        canonical_inner.to_string_lossy().as_ref()
    );
}

#[tokio::test]
async fn create_session_rejects_symlink_cwd_escape() {
    let harness = Harness::spawn().await;
    let outside = tempfile::tempdir().expect("outside");
    let link = harness.workspace_root.join("outside-link");
    std::os::unix::fs::symlink(outside.path(), &link).expect("symlink");
    let response = http()
        .post(format!("{}/v1/sessions", harness.base_url))
        .header("Authorization", session_bearer())
        .json(&json!({ "cwd": link.to_string_lossy() }))
        .send()
        .await
        .expect("create");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "prompt.body_invalid");
}

#[tokio::test]
async fn create_session_applies_model_with_custom_config_option_id() {
    let harness = Harness::spawn_with(|config| {
        config.agent.args.extend([
            "--model-config-option".to_owned(),
            "deepseek/deepseek-v4-flash".to_owned(),
            "--model-config-option-id".to_owned(),
            "agent-model".to_owned(),
            "--expect-model-config".to_owned(),
            "deepseek/deepseek-v4-flash".to_owned(),
        ]);
        config.agent.model = Some("deepseek/deepseek-v4-flash".to_owned());
    })
    .await;
    let session_id = create_session(&harness).await;
    let response = http()
        .post(format!(
            "{}/v1/sessions/{}/prompt",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({ "prompt": "model should already be set" }))
        .send()
        .await
        .expect("prompt");
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn create_session_applies_native_mode_via_set_mode() {
    // A mode advertised only in the native `modes` field (not a config option) is
    // applied through `session/set_mode`, so it must land applied, not `ignored`.
    // The placebo rejects an unadvertised mode id, so a wrong id on the wire would
    // fail the create instead of passing silently.
    let harness = Harness::spawn_with(|config| {
        config.agent.args.extend([
            "--session-mode".to_owned(),
            "default".to_owned(),
            "--session-mode".to_owned(),
            "dont_ask".to_owned(),
            "--session-mode-current".to_owned(),
            "default".to_owned(),
            // The placebo rejects a prompt unless this mode was applied through
            // session/set_mode first, so a silent no-op in the NativeMode arm
            // fails the prompt below instead of passing on an empty `ignored`.
            "--expect-mode".to_owned(),
            "dont_ask".to_owned(),
        ]);
        config.agent.mode = Some("dont_ask".to_owned());
    })
    .await;

    let response = http()
        .post(format!("{}/v1/sessions", harness.base_url))
        .header("Authorization", session_bearer())
        .json(&json!({}))
        .send()
        .await
        .expect("create");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    // Empty `ignored` is omitted from the JSON, so absent means nothing was
    // ignored; a softened mode would appear here as an `agent.mode` entry.
    let applied = body["data"]["ignored"]
        .as_array()
        .is_none_or(|entries| entries.is_empty());
    assert!(
        applied,
        "native mode must apply via session/set_mode, not be ignored: {body}"
    );

    // Prove the mode was actually applied on the wire, not merely resolved: the
    // placebo fails this prompt unless `dont_ask` reached it via session/set_mode.
    let session_id = body["data"]["id"].as_str().expect("session id").to_owned();
    let prompt = http()
        .post(format!(
            "{}/v1/sessions/{}/prompt",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({ "prompt": "mode should already be set" }))
        .send()
        .await
        .expect("prompt");
    assert_eq!(prompt.status(), StatusCode::OK);
}

#[tokio::test]
async fn full_lifecycle_create_list_get_prompt_poll_close() {
    let harness = Harness::spawn().await;
    let client = http();

    let session_id = create_session(&harness).await;

    let list: Value = client
        .get(format!("{}/v1/sessions", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("list")
        .json()
        .await
        .expect("list json");
    let ids: Vec<&str> = list["data"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["id"].as_str())
        .collect();
    assert!(ids.contains(&session_id.as_str()), "list = {ids:?}");

    let got: Value = client
        .get(format!("{}/v1/sessions/{}", harness.base_url, session_id))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("get json");
    assert_eq!(got["data"]["id"], session_id);
    assert_eq!(got["data"]["status"], "active");

    let submit: Value = client
        .post(format!(
            "{}/v1/sessions/{}/prompt",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({ "prompt": "hello agent" }))
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
    let message_id = submit["data"]["message_id"]
        .as_str()
        .expect("prompt message id")
        .to_owned();

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let final_status = loop {
        if std::time::Instant::now() > deadline {
            panic!("prompt never settled");
        }
        let poll: Value = client
            .get(format!(
                "{}/v1/sessions/{}/prompts/{}",
                harness.base_url, session_id, prompt_id
            ))
            .header("Authorization", session_bearer())
            .send()
            .await
            .expect("poll")
            .json()
            .await
            .expect("poll json");
        let status = poll["data"]["status"].as_str().unwrap_or("").to_owned();
        if matches!(status.as_str(), "completed" | "errored" | "cancelled") {
            break poll["data"].clone();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(final_status["status"], "completed");
    assert_eq!(final_status["stop_reason"], "end_turn");
    assert_eq!(final_status["message_id"], message_id);
    assert_eq!(final_status["message_id_acknowledged"], true);

    // The fake agent emits two `session/update` notifications per prompt.
    let events: Value = client
        .get(format!(
            "{}/v1/sessions/{}/events",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("events")
        .json()
        .await
        .expect("events json");
    let kinds: Vec<&str> = events["data"]["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["kind"].as_str())
        .collect();
    assert!(
        kinds.iter().filter(|k| **k == "session.update").count() >= 2,
        "expected >=2 session.update events, saw {kinds:?}"
    );
    assert!(kinds.contains(&"session.created"), "kinds = {kinds:?}");

    let close = client
        .delete(format!("{}/v1/sessions/{}", harness.base_url, session_id))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("close");
    assert_eq!(close.status(), StatusCode::OK);
    let close_body: Value = close.json().await.expect("close json");
    assert_eq!(close_body["data"]["status"], "closed");
}

/// Budget for the placebo to settle a turn; generous enough to absorb a loaded
/// CI box, short enough that a missing event fails rather than hangs.
const PROMPT_SETTLE_BUDGET: Duration = Duration::from_secs(10);
const PROMPT_SETTLE_POLL_INTERVAL: Duration = Duration::from_millis(100);

#[tokio::test]
async fn the_accepted_prompt_is_logged_as_a_user_chunk_before_the_agent_output() {
    let harness = Harness::spawn().await;
    let client = http();
    let session_id = create_session(&harness).await;
    let session: Value = client
        .get(format!("{}/v1/sessions/{}", harness.base_url, session_id))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("get session")
        .json()
        .await
        .expect("get session json");
    let agent_session_id = session["data"]["agent_session_id"]
        .as_str()
        .expect("agent session id")
        .to_owned();

    let prompt_text = "log me as a user chunk";
    let submit: Value = client
        .post(format!(
            "{}/v1/sessions/{}/prompt",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({ "prompt": prompt_text }))
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
    let message_id = submit["data"]["message_id"]
        .as_str()
        .expect("prompt message id")
        .to_owned();
    await_prompt_settled(&harness, &session_id, &prompt_id).await;

    let events = session_events(&harness, &session_id).await;
    let user_chunks: Vec<&Value> = events
        .iter()
        .filter(|event| {
            event["kind"] == "session.update"
                && event["source"] == "system"
                && chunk_kind(event) == Some("user_message_chunk".to_owned())
        })
        .collect();
    assert_eq!(
        user_chunks.len(),
        1,
        "expected exactly one user chunk, saw {events:#?}"
    );
    let user_chunk = user_chunks[0];
    assert_eq!(user_chunk["level"], "info", "event = {user_chunk}");
    let payload: Value = serde_json::from_str(
        user_chunk["payload_json"]
            .as_str()
            .expect("payload_json string"),
    )
    .expect("payload json");
    assert_eq!(
        payload["sessionId"], agent_session_id,
        "payload = {payload}"
    );
    assert_eq!(payload["update"]["content"]["type"], "text");
    assert_eq!(payload["update"]["content"]["text"], prompt_text);
    assert_eq!(payload["update"]["messageId"], message_id);
    assert_eq!(
        payload["update"]["_meta"]["acpStack"]["promptId"], prompt_id,
        "payload = {payload}"
    );

    // Ordering is the point of the durable row: a transcript replayed from the
    // log must open on the user's turn, not on the agent's first chunk.
    let user_index = events
        .iter()
        .position(|event| event["id"] == user_chunk["id"])
        .expect("user chunk position");
    let first_agent_index = events
        .iter()
        .position(|event| event["kind"] == "session.update" && event["source"] == "acp")
        .expect("the placebo streamed at least one agent chunk");
    assert!(
        user_index < first_agent_index,
        "user chunk must precede agent output, events = {events:#?}"
    );

    // Replaying the log in order yields both sides of the conversation.
    let replay: Vec<(String, String)> = events
        .iter()
        .filter_map(|event| {
            let kind = chunk_kind(event)?;
            let payload: Value =
                serde_json::from_str(event["payload_json"].as_str()?).unwrap_or(Value::Null);
            let text = payload["update"]["content"]["text"].as_str()?.to_owned();
            Some((kind, text))
        })
        .collect();
    assert_eq!(
        replay.first(),
        Some(&("user_message_chunk".to_owned(), prompt_text.to_owned())),
        "replay = {replay:?}"
    );
    assert!(
        replay.iter().any(|(kind, _)| kind == "agent_message_chunk"),
        "replay = {replay:?}"
    );
}

#[tokio::test]
async fn the_next_user_chunk_lands_after_every_agent_chunk_of_the_previous_turn() {
    let harness = Harness::spawn().await;
    let client = http();
    let session_id = create_session(&harness).await;

    // Back-to-back turns: the second is submitted the moment the first settles,
    // which is when the first turn's trailing chunks are most likely still in
    // the sink's writer queue.
    for prompt_text in ["first turn", "second turn"] {
        let submit: Value = client
            .post(format!(
                "{}/v1/sessions/{}/prompt",
                harness.base_url, session_id
            ))
            .header("Authorization", session_bearer())
            .json(&json!({ "prompt": prompt_text }))
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
        await_prompt_settled(&harness, &session_id, &prompt_id).await;
    }

    // The placebo streams two agent chunks per turn, so the log reads as two
    // complete exchanges with the second user turn after the first turn's tail.
    let expected = vec![
        "user_message_chunk",
        "agent_message_chunk",
        "agent_message_chunk",
        "user_message_chunk",
        "agent_message_chunk",
        "agent_message_chunk",
    ];
    // The second turn's tail may still be in the writer queue when its prompt
    // row settles, so wait for the full count before checking the order.
    let deadline = tokio::time::Instant::now() + PROMPT_SETTLE_BUDGET;
    loop {
        let events = session_events(&harness, &session_id).await;
        let replay: Vec<String> = events
            .iter()
            .filter(|event| event["kind"] == "session.update")
            .filter_map(chunk_kind)
            .collect();
        if replay.len() >= expected.len() || tokio::time::Instant::now() >= deadline {
            assert_eq!(replay, expected, "events = {events:#?}");
            return;
        }
        tokio::time::sleep(PROMPT_SETTLE_POLL_INTERVAL).await;
    }
}

/// The `sessionUpdate` discriminator of an event whose payload is a verbatim
/// ACP `session/update` notification.
fn chunk_kind(event: &Value) -> Option<String> {
    let payload: Value = serde_json::from_str(event["payload_json"].as_str()?).ok()?;
    Some(payload["update"]["sessionUpdate"].as_str()?.to_owned())
}

async fn session_events(harness: &Harness, session_id: &str) -> Vec<Value> {
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
        .clone()
}

async fn await_prompt_settled(harness: &Harness, session_id: &str, prompt_id: &str) {
    let deadline = tokio::time::Instant::now() + PROMPT_SETTLE_BUDGET;
    loop {
        let poll: Value = http()
            .get(format!(
                "{}/v1/sessions/{}/prompts/{}",
                harness.base_url, session_id, prompt_id
            ))
            .header("Authorization", session_bearer())
            .send()
            .await
            .expect("poll")
            .json()
            .await
            .expect("poll json");
        if poll["data"]["status"] == "completed" {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "prompt never completed: {poll}"
        );
        tokio::time::sleep(PROMPT_SETTLE_POLL_INTERVAL).await;
    }
}

#[tokio::test]
async fn delete_session_removes_the_row_and_repeats_silently() {
    let harness = Harness::spawn().await;
    let client = http();
    let session_id = create_session(&harness).await;

    let delete = client
        .post(format!(
            "{}/v1/sessions/{}/delete",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("delete");
    assert_eq!(delete.status(), StatusCode::OK);
    let delete_body: Value = delete.json().await.expect("delete json");
    assert_eq!(delete_body["data"]["session_id"], session_id.as_str());
    assert_eq!(delete_body["data"]["deleted"], true);

    let get = client
        .get(format!("{}/v1/sessions/{}", harness.base_url, session_id))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("get");
    assert_eq!(get.status(), StatusCode::NOT_FOUND);

    // Repeats and unknown ids succeed silently per ACP session/delete.
    let repeat = client
        .post(format!(
            "{}/v1/sessions/{}/delete",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("repeat delete");
    assert_eq!(repeat.status(), StatusCode::OK);
    let repeat_body: Value = repeat.json().await.expect("repeat json");
    assert_eq!(repeat_body["data"]["deleted"], false);
}

#[tokio::test]
async fn delete_session_reports_unsupported_capability_and_keeps_the_row() {
    let harness = Harness::spawn_with(|config| {
        config.agent.args.push("--no-cap-delete-session".to_owned());
    })
    .await;
    let client = http();
    let session_id = create_session(&harness).await;

    let delete = client
        .post(format!(
            "{}/v1/sessions/{}/delete",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("delete");
    assert_eq!(delete.status(), StatusCode::NOT_IMPLEMENTED);
    let body: Value = delete.json().await.expect("delete json");
    assert_eq!(body["error"]["code"], "agent.unsupported_capability");

    let get = client
        .get(format!("{}/v1/sessions/{}", harness.base_url, session_id))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("get");
    assert_eq!(get.status(), StatusCode::OK);
}

#[tokio::test]
async fn fork_session_records_parent_lineage() {
    let harness = Harness::spawn().await;
    let client = http();
    let session_id = create_session(&harness).await;

    let forked: Value = client
        .post(format!(
            "{}/v1/sessions/{}/fork",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({}))
        .send()
        .await
        .expect("fork")
        .json()
        .await
        .expect("fork json");
    let child_id = forked["data"]["id"].as_str().expect("child id");

    let state = harness.state.lock().await;
    let child = state
        .get_session(child_id)
        .expect("child lookup")
        .expect("child exists");
    let metadata: Value = serde_json::from_str(&child.metadata_json).expect("metadata json");
    assert_eq!(metadata["fork"]["parent_session_id"], session_id);
    assert_eq!(metadata["fork"]["strategy"], "acp_native");
    assert!(metadata["fork"]["message_id"].is_null());
}

#[tokio::test]
async fn fork_session_forwards_message_breakpoint_to_placebo() {
    const BREAKPOINT_MESSAGE_ID: &str = "00000000-0000-4000-8000-000000000001";

    let harness = Harness::spawn_with(|config| {
        config.agent.args.extend([
            "--expect-fork-message-id".to_owned(),
            BREAKPOINT_MESSAGE_ID.to_owned(),
        ]);
    })
    .await;
    let client = http();
    let session_id = create_session(&harness).await;

    {
        let state = harness.state.lock().await;
        state
            .insert_prompt_with_message_id(
                NewPromptRecord {
                    id: "prm_fork_breakpoint".to_owned(),
                    session_id: session_id.clone(),
                    prompt_json: r#"[{"type":"text","text":"fork breakpoint"}]"#.to_owned(),
                },
                Some(BREAKPOINT_MESSAGE_ID.to_owned()),
            )
            .expect("prompt inserted");
        state
            .acknowledge_prompt_message_id("prm_fork_breakpoint", BREAKPOINT_MESSAGE_ID)
            .expect("prompt message id acknowledged");
    }

    let forked: Value = client
        .post(format!(
            "{}/v1/sessions/{}/fork",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({ "message_id": BREAKPOINT_MESSAGE_ID }))
        .send()
        .await
        .expect("fork")
        .json()
        .await
        .expect("fork json");
    let child_id = forked["data"]["id"].as_str().expect("child id");

    let state = harness.state.lock().await;
    let child = state
        .get_session(child_id)
        .expect("child lookup")
        .expect("child exists");
    let metadata: Value = serde_json::from_str(&child.metadata_json).expect("metadata json");
    assert_eq!(metadata["fork"]["parent_session_id"], session_id);
    assert_eq!(metadata["fork"]["strategy"], "acp_native");
    assert_eq!(metadata["fork"]["message_id"], BREAKPOINT_MESSAGE_ID);
}

#[tokio::test]
async fn create_session_lazily_starts_a_never_started_agent() {
    // Regression: after `acps init` nothing had ever spawned the agent, so
    // every session call answered `agent.not_running`.
    let harness = Harness::spawn_without_agent_start(|_| {}).await;
    assert_eq!(harness.agent_process_state().await, "stopped");

    let session_id = create_session(&harness).await;

    assert!(!session_id.is_empty());
    assert_eq!(harness.agent_process_state().await, "running");
}

#[tokio::test]
async fn restart_never_opts_a_target_out_of_lazy_start() {
    let harness = Harness::spawn_without_agent_start(|config| {
        config.agent.restart = "never".to_owned();
    })
    .await;

    let response = http()
        .post(format!("{}/v1/sessions", harness.base_url))
        .header("Authorization", session_bearer())
        .json(&json!({}))
        .send()
        .await
        .expect("create");

    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "agent.not_running");
    assert_eq!(harness.agent_process_state().await, "stopped");
}

#[tokio::test]
async fn prompt_lazily_restarts_an_agent_that_went_away() {
    let harness = Harness::spawn().await;
    let session_id = create_session(&harness).await;
    harness.stop_agent().await;
    assert_eq!(harness.agent_process_state().await, "stopped");

    // What matters here is that the request brought the agent back; the
    // re-attach that makes the prompt itself land is covered below.
    let response = http()
        .post(format!(
            "{}/v1/sessions/{}/prompt",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({ "prompt": "are you back?" }))
        .send()
        .await
        .expect("prompt");
    assert_ne!(response.status(), StatusCode::CONFLICT);
    assert_eq!(harness.agent_process_state().await, "running");
}

/// The placebo flag mirrors a restarted adapter: it answers `session/prompt`
/// with `invalidParams` for any session its own process never opened.
fn reject_unopened_session_prompt(config: &mut Config) {
    config
        .agent
        .args
        .push("--reject-unopened-session-prompt".to_owned());
}

async fn submit_prompt_response(
    harness: &Harness,
    session_id: &str,
    text: &str,
) -> reqwest::Response {
    http()
        .post(format!(
            "{}/v1/sessions/{}/prompt",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({ "prompt": text }))
        .send()
        .await
        .expect("prompt")
}

async fn submit_and_settle(harness: &Harness, session_id: &str, text: &str) {
    let response = submit_prompt_response(harness, session_id, text).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("prompt json");
    let prompt_id = body["data"]["prompt_id"].as_str().expect("prompt id");
    await_prompt_settled(harness, session_id, prompt_id).await;
}

async fn event_kind_count(harness: &Harness, session_id: &str, kind: &str) -> usize {
    session_events(harness, session_id)
        .await
        .iter()
        .filter(|event| event["kind"] == kind)
        .count()
}

#[tokio::test]
async fn prompting_a_demoted_session_resumes_it_before_dispatch() {
    let harness = Harness::spawn_with(reject_unopened_session_prompt).await;
    let session_id = create_session(&harness).await;
    submit_and_settle(&harness, &session_id, "first turn").await;

    // Stops demote the row to `available`, and the next prompt lazily starts a
    // fresh adapter that has never held this session.
    harness.stop_agent().await;
    submit_and_settle(&harness, &session_id, "second turn after restart").await;

    let events = session_events(&harness, &session_id).await;
    let resumed = events
        .iter()
        .position(|event| event["kind"] == "session.resumed")
        .unwrap_or_else(|| panic!("expected a session.resumed event, saw {events:#?}"));
    let demoted = events
        .iter()
        .position(|event| event["kind"] == "session.available")
        .unwrap_or_else(|| panic!("expected a session.available event, saw {events:#?}"));
    assert!(
        demoted < resumed,
        "the re-attach must follow the demotion, events = {events:#?}"
    );

    let session: Value = http()
        .get(format!("{}/v1/sessions/{}", harness.base_url, session_id))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("get session")
        .json()
        .await
        .expect("get session json");
    assert_eq!(session["data"]["status"], "active", "{session}");
}

#[tokio::test]
async fn a_warm_session_is_prompted_without_a_second_attach() {
    let harness = Harness::spawn_with(reject_unopened_session_prompt).await;
    let session_id = create_session(&harness).await;

    for text in ["first turn", "second turn", "third turn"] {
        submit_and_settle(&harness, &session_id, text).await;
    }

    // `session/new` already attached the session on this adapter, so no turn
    // pays for a re-attach round trip.
    assert_eq!(
        event_kind_count(&harness, &session_id, "session.resumed").await,
        0
    );
    assert_eq!(
        event_kind_count(&harness, &session_id, "session.loaded").await,
        0
    );
}

#[tokio::test]
async fn a_warm_prompt_does_not_resolve_mcp_servers() {
    let home = tempfile::tempdir().expect("home tempdir");
    SecretStore::open_or_create(home.path()).expect("secret store initializes");
    let harness = Harness::spawn_with_and_home(
        |config| {
            config.mcp.servers = declared_mcp_servers();
        },
        home.path().to_path_buf(),
    )
    .await;
    let session_id = create_session(&harness).await;

    // MCP servers are an input to the re-attach path only. A secret store
    // that becomes unreadable after the session is open must not fail a
    // prompt for a session the running adapter already holds.
    std::fs::remove_file(acp_stack::secrets::secret_store_path(home.path()))
        .expect("remove secret store");
    submit_and_settle(&harness, &session_id, "warm turn").await;
}

#[tokio::test]
async fn prompting_a_demoted_session_loads_it_when_resume_is_unadvertised() {
    let harness = Harness::spawn_with(|config| {
        reject_unopened_session_prompt(config);
        config.agent.args.push("--no-cap-resume-session".to_owned());
    })
    .await;
    let session_id = create_session(&harness).await;
    harness.stop_agent().await;

    submit_and_settle(&harness, &session_id, "load me back").await;

    assert_eq!(
        event_kind_count(&harness, &session_id, "session.loaded").await,
        1
    );
    assert_eq!(
        event_kind_count(&harness, &session_id, "session.resumed").await,
        0
    );
}

#[tokio::test]
async fn a_prompt_fails_fast_when_the_agent_can_neither_resume_nor_load() {
    let harness = Harness::spawn_with(|config| {
        reject_unopened_session_prompt(config);
        config.agent.args.extend([
            "--no-cap-resume-session".to_owned(),
            "--no-cap-load-session".to_owned(),
        ]);
    })
    .await;
    let session_id = create_session(&harness).await;
    harness.stop_agent().await;

    let response = submit_prompt_response(&harness, &session_id, "nowhere to attach").await;
    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    let body: Value = response.json().await.expect("prompt json");
    assert_eq!(
        body["error"]["code"], "session.reattach_unsupported",
        "{body}"
    );

    // The refusal happens before any row is written, so no prompt is left for a
    // client to poll and no turn was dispatched to the agent.
    assert_eq!(
        common::sessions::prompt_count_for_session(&harness, &session_id).await,
        0
    );
}

#[tokio::test]
async fn load_and_resume_reject_closed_sessions() {
    let harness = Harness::spawn().await;
    let session_id = create_session(&harness).await;
    let client = http();

    let close = client
        .delete(format!("{}/v1/sessions/{}", harness.base_url, session_id))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("close");
    assert_eq!(close.status(), StatusCode::OK);

    for route in ["load", "resume"] {
        let response = client
            .post(format!(
                "{}/v1/sessions/{}/{}",
                harness.base_url, session_id, route
            ))
            .header("Authorization", session_bearer())
            .json(&json!({}))
            .send()
            .await
            .expect("session lifecycle request");
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body: Value = response.json().await.expect("json");
        assert_eq!(body["error"]["code"], "session.closed");
    }
}

#[tokio::test]
async fn close_session_on_secondary_target_survives_array_off() {
    // Regression: close/cancel bypass the Array-enabled gate, so `acps array
    // off` never strands a live session on a non-primary target.
    let harness = Harness::spawn_with(|config| {
        config.array.enabled = true;
        let mut secondary = config.agent.clone();
        secondary.id = "codex".to_owned();
        secondary.name = "Codex".to_owned();
        config.array.targets.push(ArrayTargetConfig {
            id: "codex".to_owned(),
            agent: secondary,
        });
    })
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
        .post(format!("{}/v1/sessions?target=codex", harness.base_url))
        .header("Authorization", session_bearer())
        .header("Content-Type", "application/json")
        .body("{}")
        .send()
        .await
        .expect("create session");
    assert_eq!(create.status(), StatusCode::OK);
    let session_id = create.json::<Value>().await.expect("create json")["data"]["id"]
        .as_str()
        .expect("session id")
        .to_owned();

    // Toggle Array off by rewriting the on-disk config; handlers re-read it.
    let mut disabled = Config::load_from_path(&harness.config_path).expect("load config");
    disabled.array.enabled = false;
    std::fs::write(
        &harness.config_path,
        disabled.to_canonical_toml().expect("canonical config"),
    )
    .expect("rewrite config");

    let close = client
        .delete(format!("{}/v1/sessions/{}", harness.base_url, session_id))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("close session");
    assert_eq!(close.status(), StatusCode::OK);
    let close_body: Value = close.json().await.expect("close json");
    assert_eq!(close_body["data"]["status"], "closed");
}

#[tokio::test]
async fn stored_session_cwd_must_remain_under_workspace_for_reuse() {
    let harness = Harness::spawn().await;
    let outside = tempfile::tempdir().expect("outside");
    {
        let state = harness.state.lock().await;
        state
            .insert_session(NewSessionRecord {
                id: "sess_bad_cwd".to_owned(),
                agent_id: "placebo".to_owned(),
                cwd: outside.path().to_string_lossy().into_owned(),
                title: None,
                metadata_json: "{}".to_owned(),
            })
            .expect("session inserted");
    }

    let response = http()
        .post(format!(
            "{}/v1/sessions/{}/load",
            harness.base_url, "sess_bad_cwd"
        ))
        .header("Authorization", session_bearer())
        .json(&json!({}))
        .send()
        .await
        .expect("load");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "prompt.body_invalid");
}

#[tokio::test]
async fn stored_inner_cwd_is_valid_for_load_resume_and_fork() {
    let harness = Harness::spawn().await;
    let inner = harness.workspace_root.join("stored-inner");
    std::fs::create_dir(&inner).expect("inner dir");
    {
        let state = harness.state.lock().await;
        state
            .insert_session(NewSessionRecord {
                id: "sess_valid_cwd".to_owned(),
                agent_id: "placebo".to_owned(),
                cwd: inner.to_string_lossy().into_owned(),
                title: None,
                metadata_json: "{}".to_owned(),
            })
            .expect("session inserted");
    }

    let client = http();
    for route in ["load", "resume", "fork"] {
        let response = client
            .post(format!(
                "{}/v1/sessions/{}/{}",
                harness.base_url, "sess_valid_cwd", route
            ))
            .header("Authorization", session_bearer())
            .json(&json!({}))
            .send()
            .await
            .expect("session lifecycle request");
        assert_eq!(response.status(), StatusCode::OK);
    }
}

#[tokio::test]
async fn explicit_load_and_resume_cwd_is_persisted_after_agent_success() {
    let harness = Harness::spawn().await;
    let session_id = create_session(&harness).await;
    let load_cwd = harness.workspace_root.join("load-cwd");
    let resume_cwd = harness.workspace_root.join("resume-cwd");
    std::fs::create_dir(&load_cwd).expect("load cwd");
    std::fs::create_dir(&resume_cwd).expect("resume cwd");
    let client = http();

    let load_body: Value = client
        .post(format!(
            "{}/v1/sessions/{}/load",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({ "cwd": load_cwd.to_string_lossy() }))
        .send()
        .await
        .expect("load")
        .json()
        .await
        .expect("load json");
    let canonical_load = load_cwd.canonicalize().expect("canonical load cwd");
    assert_eq!(
        load_body["data"]["cwd"],
        canonical_load.to_string_lossy().as_ref()
    );

    let resume_body: Value = client
        .post(format!(
            "{}/v1/sessions/{}/resume",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({ "cwd": resume_cwd.to_string_lossy() }))
        .send()
        .await
        .expect("resume")
        .json()
        .await
        .expect("resume json");
    let canonical_resume = resume_cwd.canonicalize().expect("canonical resume cwd");
    assert_eq!(
        resume_body["data"]["cwd"],
        canonical_resume.to_string_lossy().as_ref()
    );

    let state = harness.state.lock().await;
    let stored = state
        .get_session(&session_id)
        .expect("session lookup")
        .expect("session exists");
    assert_eq!(stored.cwd, canonical_resume.to_string_lossy());
}

#[cfg(unix)]
#[tokio::test]
async fn stored_session_cwd_symlink_escape_is_rejected_before_reuse() {
    let harness = Harness::spawn().await;
    let inner = harness.workspace_root.join("stored-cwd");
    std::fs::create_dir(&inner).expect("inner dir");
    {
        let state = harness.state.lock().await;
        state
            .insert_session(NewSessionRecord {
                id: "sess_changed_cwd".to_owned(),
                agent_id: "placebo".to_owned(),
                cwd: inner.to_string_lossy().into_owned(),
                title: None,
                metadata_json: "{}".to_owned(),
            })
            .expect("session inserted");
    }
    std::fs::remove_dir(&inner).expect("remove inner");
    let outside = tempfile::tempdir().expect("outside");
    std::os::unix::fs::symlink(outside.path(), &inner).expect("replace with symlink");

    let response = http()
        .post(format!(
            "{}/v1/sessions/{}/resume",
            harness.base_url, "sess_changed_cwd"
        ))
        .header("Authorization", session_bearer())
        .json(&json!({}))
        .send()
        .await
        .expect("resume");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "prompt.body_invalid");
}

fn declared_mcp_servers() -> Vec<McpServerConfig> {
    vec![
        McpServerConfig::Stdio(McpStdioServer {
            name: "local-stdio".into(),
            command: "/bin/sh".into(),
            args: vec![],
            env: vec![],
        }),
        McpServerConfig::Http(McpHttpServer {
            name: "remote-http".into(),
            url: "https://example.invalid/mcp".into(),
            headers: vec![],
        }),
    ]
}

#[tokio::test]
async fn no_mcp_advertisement_skips_every_server_including_stdio() {
    // With no MCP capability advertised, even the stdio server is skipped.
    let home = tempfile::tempdir().expect("home tempdir");
    SecretStore::open_or_create(home.path()).expect("secret store initializes");

    let harness = Harness::spawn_with_and_home(
        |config| {
            config.mcp.servers = declared_mcp_servers();
        },
        home.path().to_path_buf(),
    )
    .await;

    let session_id = create_session(&harness).await;

    let events = {
        let store = harness.state.lock().await;
        store
            .query_session_events(&session_id, None, 50)
            .expect("session events")
    };
    assert!(
        events
            .iter()
            .all(|event| event.kind != "mcp.session_attached"),
        "{events:?}"
    );
    let skipped: Vec<_> = events
        .iter()
        .filter(|event| event.kind == "mcp.session_skipped")
        .collect();
    assert_eq!(skipped.len(), 1, "{events:?}");
    assert!(
        skipped[0].payload_json.contains("local-stdio")
            && skipped[0].payload_json.contains("remote-http"),
        "{events:?}"
    );
}

#[tokio::test]
async fn attached_mcp_event_lists_servers_for_an_mcp_capable_agent() {
    // With `mcpCapabilities.http` advertised, both the stdio baseline and the
    // HTTP server are sent.
    let home = tempfile::tempdir().expect("home tempdir");
    SecretStore::open_or_create(home.path()).expect("secret store initializes");

    let harness = Harness::spawn_with_and_home(
        |config| {
            config.agent.args.push("--cap-mcp-http".to_owned());
            config.mcp.servers = declared_mcp_servers();
        },
        home.path().to_path_buf(),
    )
    .await;

    let session_id = create_session(&harness).await;

    let events = {
        let store = harness.state.lock().await;
        store
            .query_session_events(&session_id, None, 50)
            .expect("session events")
    };
    let attached: Vec<_> = events
        .iter()
        .filter(|event| event.kind == "mcp.session_attached")
        .collect();
    assert_eq!(attached.len(), 1, "{events:?}");
    let payload: Value =
        serde_json::from_str(&attached[0].payload_json).expect("attached payload json");
    assert_eq!(
        payload["server_names"],
        json!(["local-stdio", "remote-http"])
    );
    assert!(
        events
            .iter()
            .all(|event| event.kind != "mcp.session_skipped"),
        "{events:?}"
    );
}

#[tokio::test]
async fn codex_openrouter_effort_pinned_on_disk_is_not_an_ignore_record() {
    // Codex with OpenRouter carries the effort in its own config file; the
    // adapter advertises no effort option there, so no ACP set is attempted.
    let home = tempfile::tempdir().expect("home tempdir");
    let mut store = SecretStore::open_or_create(home.path()).expect("secret store initializes");
    store
        .set_many([("OPENROUTER_API_KEY", "test-openrouter-key")])
        .expect("flat secret should be stored");
    let mut catalog = store.provider_credentials().clone();
    catalog.insert(
        "openrouter".to_owned(),
        acp_stack::secrets::ProviderCredentialSet::aliasless(
            acp_stack::secrets::ProviderCredential::new(
                std::collections::BTreeMap::from([(
                    "OPENROUTER_API_KEY".to_owned(),
                    "test-openrouter-key".to_owned(),
                )]),
                std::collections::BTreeMap::new(),
            ),
        ),
    );
    store
        .replace_provider_credentials(catalog, &[])
        .expect("provider credential should be stored");
    let harness = Harness::spawn_with_and_home(
        |config| {
            config.agent.id = "codex".to_owned();
            config.agent.env = vec!["OPENROUTER_API_KEY".to_owned()];
            config.agent.provider = Some(acp_stack::config::AgentProviderConfig {
                id: "openrouter".to_owned(),
                model: Some("deepseek/deepseek-v4-flash".to_owned()),
                api_key_ref: Some("OPENROUTER_API_KEY".to_owned()),
                custom: None,
            });
            config.agent.effort = Some("high".to_owned());
        },
        home.path().to_path_buf(),
    )
    .await;

    let response = http()
        .post(format!("{}/v1/sessions", harness.base_url))
        .header("Authorization", session_bearer())
        .json(&json!({}))
        .send()
        .await
        .expect("create");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    assert!(
        body["data"].get("ignored").is_none(),
        "a disk-pinned effort must not be reported as ignored: {body}"
    );
}

#[tokio::test]
async fn unadvertised_mode_model_and_effort_are_ignored_not_fatal() {
    // A config-declared value the agent never advertises must degrade to an
    // ignored record, not fail session create with `agent.config_provision`.
    let harness = Harness::spawn_with(|config| {
        config.agent.mode = Some("plan".to_owned());
        config.agent.model = Some("deepseek/deepseek-v4-flash".to_owned());
        config.agent.effort = Some("high".to_owned());
    })
    .await;

    let response = http()
        .post(format!("{}/v1/sessions", harness.base_url))
        .header("Authorization", session_bearer())
        .json(&json!({}))
        .send()
        .await
        .expect("create");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    let ignored = body["data"]["ignored"].as_array().expect("ignored array");
    let features: Vec<&str> = ignored
        .iter()
        .filter_map(|entry| entry["feature"].as_str())
        .collect();
    assert_eq!(
        features,
        ["agent.mode", "agent.model", "agent.effort"],
        "{body}"
    );
    assert_eq!(ignored[0]["value"], "plan");
    assert_eq!(ignored[1]["value"], "deepseek/deepseek-v4-flash");
    assert_eq!(ignored[2]["value"], "high");

    let session_id = body["data"]["id"].as_str().expect("session id").to_owned();
    let events = {
        let store = harness.state.lock().await;
        store
            .query_session_events(&session_id, None, 50)
            .expect("session events")
    };
    let capability_events: Vec<_> = events
        .iter()
        .filter(|event| event.kind == "session.capability_ignored")
        .collect();
    assert_eq!(capability_events.len(), 1, "{events:?}");
    assert!(
        capability_events[0].payload_json.contains("agent.mode")
            && capability_events[0].payload_json.contains("agent.model")
            && capability_events[0].payload_json.contains("agent.effort"),
        "{events:?}"
    );

    let prompt = http()
        .post(format!(
            "{}/v1/sessions/{}/prompt",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({ "prompt": "still works on agent defaults" }))
        .send()
        .await
        .expect("prompt");
    assert_eq!(prompt.status(), StatusCode::OK);
}

#[tokio::test]
async fn config_options_are_applied_snapshotted_and_settable() {
    // The placebo advertises one select (`persona`) and one boolean (`fast`);
    // the unadvertised `ghost` entry must degrade to an ignored record.
    let harness = Harness::spawn_with(|config| {
        config.agent.args.extend([
            "--config-option-select".to_owned(),
            "persona=default:default,researcher".to_owned(),
            "--config-option-boolean".to_owned(),
            "fast@model_config=false".to_owned(),
        ]);
        config
            .agent
            .config_options
            .insert("fast".to_owned(), AgentConfigOptionValue::Bool(true));
        config.agent.config_options.insert(
            "persona".to_owned(),
            AgentConfigOptionValue::Text("researcher".to_owned()),
        );
        config.agent.config_options.insert(
            "ghost".to_owned(),
            AgentConfigOptionValue::Text("anything".to_owned()),
        );
    })
    .await;

    let response = http()
        .post(format!("{}/v1/sessions", harness.base_url))
        .header("Authorization", session_bearer())
        .json(&json!({}))
        .send()
        .await
        .expect("create");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    let ignored = body["data"]["ignored"].as_array().expect("ignored array");
    assert_eq!(ignored.len(), 1, "{body}");
    assert_eq!(ignored[0]["feature"], "agent.config_option");
    assert_eq!(ignored[0]["option_id"], "ghost");
    assert_eq!(ignored[0]["value"], "anything");
    let session_id = body["data"]["id"].as_str().expect("session id").to_owned();

    let response = http()
        .get(format!(
            "{}/v1/sessions/{}/config-options",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("get config options");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    let options = body["data"]["config_options"]
        .as_array()
        .expect("options array");
    let fast = options
        .iter()
        .find(|option| option["id"] == "fast")
        .expect("fast option");
    assert_eq!(fast["type"], "boolean");
    assert_eq!(fast["category"], "model_config");
    assert_eq!(fast["current_value"], json!(true), "{body}");
    let persona = options
        .iter()
        .find(|option| option["id"] == "persona")
        .expect("persona option");
    assert_eq!(persona["type"], "select");
    assert_eq!(persona["current_value"], "researcher", "{body}");
    assert!(persona.get("category").is_none(), "{body}");
    assert!(body["data"]["updated_at"].is_string());

    let response = http()
        .post(format!(
            "{}/v1/sessions/{}/config-options",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({ "config_id": "persona", "value": "default" }))
        .send()
        .await
        .expect("set config option");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    let persona = body["data"]["config_options"]
        .as_array()
        .expect("options array")
        .iter()
        .find(|option| option["id"] == "persona")
        .cloned()
        .expect("persona option");
    assert_eq!(persona["current_value"], "default", "{body}");

    let response = http()
        .post(format!(
            "{}/v1/sessions/{}/config-options",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({ "config_id": "nope", "value": "x" }))
        .send()
        .await
        .expect("set unknown option");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let response = http()
        .post(format!(
            "{}/v1/sessions/{}/config-options",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({ "config_id": "fast", "value": "on" }))
        .send()
        .await
        .expect("set mismatched kind");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let response = http()
        .post(format!(
            "{}/v1/sessions/{}/config-options",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({ "config_id": "fast", "value": false }))
        .send()
        .await
        .expect("set boolean option");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    let fast = body["data"]["config_options"]
        .as_array()
        .expect("options array")
        .iter()
        .find(|option| option["id"] == "fast")
        .cloned()
        .expect("fast option");
    assert_eq!(fast["current_value"], json!(false), "{body}");
}

#[tokio::test]
async fn config_option_notifications_refresh_the_stored_snapshot() {
    // The placebo answers `session/set_config_option` with an empty list and
    // carries the refreshed state only in a `config_option_update`
    // notification, so the GET can only see it via notification projection.
    let harness = Harness::spawn_with(|config| {
        config.agent.args.extend([
            "--config-option-select".to_owned(),
            "persona=default:default,researcher".to_owned(),
            "--emit-config-option-update".to_owned(),
            "--set-config-option-responds-empty".to_owned(),
        ]);
    })
    .await;

    let response = http()
        .post(format!("{}/v1/sessions", harness.base_url))
        .header("Authorization", session_bearer())
        .json(&json!({}))
        .send()
        .await
        .expect("create");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    let session_id = body["data"]["id"].as_str().expect("session id").to_owned();

    let response = http()
        .post(format!(
            "{}/v1/sessions/{}/config-options",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({ "config_id": "persona", "value": "researcher" }))
        .send()
        .await
        .expect("set config option");
    assert_eq!(response.status(), StatusCode::OK);
    // An empty agent answer must fall back to the stored snapshot.
    let body: Value = response.json().await.expect("json");
    assert!(
        !body["data"]["config_options"]
            .as_array()
            .expect("options array")
            .is_empty(),
        "{body}"
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let response = http()
            .get(format!(
                "{}/v1/sessions/{}/config-options",
                harness.base_url, session_id
            ))
            .header("Authorization", session_bearer())
            .send()
            .await
            .expect("get config options");
        let body: Value = response.json().await.expect("json");
        let current = body["data"]["config_options"]
            .as_array()
            .and_then(|options| {
                options
                    .iter()
                    .find(|option| option["id"] == "persona")
                    .map(|option| option["current_value"].clone())
            });
        if current == Some(json!("researcher")) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "notification never refreshed the snapshot: {body}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn advertised_model_is_still_applied_without_ignore_records() {
    // When the agent does advertise the configured model, nothing is ignored.
    let harness = Harness::spawn_with(|config| {
        config.agent.args.extend([
            "--model-config-option".to_owned(),
            "deepseek/deepseek-v4-flash".to_owned(),
            "--model-config-option-id".to_owned(),
            "agent-model".to_owned(),
            "--expect-model-config".to_owned(),
            "deepseek/deepseek-v4-flash".to_owned(),
        ]);
        config.agent.model = Some("deepseek/deepseek-v4-flash".to_owned());
    })
    .await;

    let response = http()
        .post(format!("{}/v1/sessions", harness.base_url))
        .header("Authorization", session_bearer())
        .json(&json!({}))
        .send()
        .await
        .expect("create");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    assert!(
        body["data"].get("ignored").is_none(),
        "ignored must be omitted when empty: {body}"
    );

    let session_id = body["data"]["id"].as_str().expect("session id").to_owned();
    let events = {
        let store = harness.state.lock().await;
        store
            .query_session_events(&session_id, None, 50)
            .expect("session events")
    };
    assert!(
        events
            .iter()
            .all(|event| event.kind != "session.capability_ignored"),
        "{events:?}"
    );
}
