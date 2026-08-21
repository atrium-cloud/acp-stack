use std::time::Duration;

use crate::common::sessions::{Harness, create_session, http, session_bearer};
use acp_stack::state::{NewPromptRecord, NewSessionRecord};
use reqwest::StatusCode;
use serde_json::{Value, json};

#[tokio::test]
async fn session_update_notifications_land_in_events_table() {
    let harness = Harness::spawn().await;
    let session_id = create_session(&harness).await;
    let client = http();
    let submit: Value = client
        .post(format!(
            "{}/v1/sessions/{}/prompt",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({ "prompt": "ping" }))
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

    // Wait for terminal status before querying state — the writer task
    // settles the prompt row, and only then are all the session.update rows
    // guaranteed to have flushed.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if std::time::Instant::now() > deadline {
            panic!("prompt did not settle");
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
        if matches!(
            poll["data"]["status"].as_str().unwrap_or(""),
            "completed" | "errored" | "cancelled"
        ) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Query the state store directly to assert events landed.
    let store = harness.state.lock().await;
    let events = store
        .query_session_events(&session_id, None, 100)
        .expect("session events");
    drop(store);
    let updates = events.iter().filter(|e| e.kind == "session.update").count();
    assert!(
        updates >= 2,
        "expected >=2 session.update rows, saw {updates}"
    );
}

#[tokio::test]
async fn sessions_snapshot_returns_session_in_flight_prompts_and_recent_events() {
    let harness = Harness::spawn_with(|config| {
        // Disable the bridge's `session/list` capability so the placebo agent
        // path leaves the state untouched after start; we want a clean slate
        // to seed deterministic snapshot fixtures.
        config.agent.args.push("--no-cap-list-session".into());
    })
    .await;
    let session_id = "sess_snapshot".to_owned();
    {
        let store = harness.state.lock().await;
        store
            .insert_session(NewSessionRecord {
                id: session_id.clone(),
                agent_id: "placebo".to_owned(),
                cwd: "/tmp/snap".to_owned(),
                title: Some("snap".to_owned()),
                metadata_json: "{}".to_owned(),
            })
            .expect("session inserted");
        store
            .insert_prompt(NewPromptRecord {
                id: "prm_inflight".to_owned(),
                session_id: session_id.clone(),
                prompt_json: r#"[{"type":"text","text":"hi"}]"#.to_owned(),
            })
            .expect("prompt inserted");
        for index in 0..3 {
            store
                .append_session_event_with_source(
                    &session_id,
                    "info",
                    "session.update",
                    acp_stack::state::EVENT_SOURCE_ACP,
                    "ACP session update",
                    &format!(r#"{{"seq":{index}}}"#),
                )
                .expect("event inserted");
        }
    }

    let body: Value = http()
        .get(format!(
            "{}/v1/sessions/{}/snapshot",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("snapshot")
        .json()
        .await
        .expect("snapshot json");

    assert_eq!(body["data"]["session"]["id"], session_id);
    assert_eq!(body["data"]["session"]["status"], "active");
    let in_flight = body["data"]["in_flight_prompts"]
        .as_array()
        .expect("in_flight_prompts array");
    assert_eq!(in_flight.len(), 1);
    assert_eq!(in_flight[0]["id"], "prm_inflight");
    assert_eq!(in_flight[0]["status"], "pending");

    let events = body["data"]["recent_events"]
        .as_array()
        .expect("recent_events array");
    assert_eq!(events.len(), 3);
    // Newest-first: the third event we appended carries `"seq":2`.
    let head_payload: Value = serde_json::from_str(events[0]["payload_json"].as_str().unwrap())
        .expect("head payload json");
    assert_eq!(head_payload["seq"], 2);
    let tail_payload: Value = serde_json::from_str(events[2]["payload_json"].as_str().unwrap())
        .expect("tail payload json");
    assert_eq!(tail_payload["seq"], 0);

    let last_event_id = body["data"]["last_event_id"]
        .as_str()
        .expect("last_event_id present");
    assert_eq!(last_event_id, events[0]["id"].as_str().unwrap());
    // No advertisement yet: the typed list is present and empty.
    assert_eq!(body["data"]["available_commands"], json!([]));
}

#[tokio::test]
async fn sessions_snapshot_surfaces_available_commands_and_tolerates_garbage_metadata() {
    let harness = Harness::spawn_with(|config| {
        config.agent.args.push("--no-cap-list-session".into());
    })
    .await;
    let advertised = "sess_snap_commands".to_owned();
    let garbage = "sess_snap_garbage".to_owned();
    {
        let store = harness.state.lock().await;
        store
            .insert_session(NewSessionRecord {
                id: advertised.clone(),
                agent_id: "placebo".to_owned(),
                cwd: "/tmp/snapcmd".to_owned(),
                title: None,
                metadata_json: "{}".to_owned(),
            })
            .expect("session inserted");
        store
            .replace_session_available_commands(
                &advertised,
                &[acp_stack::state::SessionAvailableCommand {
                    name: "compact".to_owned(),
                    description: "Summarize the conversation".to_owned(),
                    input_hint: Some("optional instructions".to_owned()),
                }],
            )
            .expect("commands stored");
        // A wrong-shaped stored value must degrade to an empty list, not 500.
        store
            .insert_session(NewSessionRecord {
                id: garbage.clone(),
                agent_id: "placebo".to_owned(),
                cwd: "/tmp/snapgarbage".to_owned(),
                title: None,
                metadata_json: r#"{"available_commands":"not-a-list"}"#.to_owned(),
            })
            .expect("session inserted");
    }

    let body: Value = http()
        .get(format!(
            "{}/v1/sessions/{}/snapshot",
            harness.base_url, advertised
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("snapshot")
        .json()
        .await
        .expect("snapshot json");
    let commands = body["data"]["available_commands"]
        .as_array()
        .expect("available_commands array");
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0]["name"], "compact");
    assert_eq!(commands[0]["description"], "Summarize the conversation");
    assert_eq!(commands[0]["input_hint"], "optional instructions");

    let response = http()
        .get(format!(
            "{}/v1/sessions/{}/snapshot",
            harness.base_url, garbage
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("snapshot");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("snapshot json");
    assert_eq!(body["data"]["available_commands"], json!([]));
}

#[tokio::test]
async fn sessions_snapshot_returns_404_for_unknown_session() {
    let harness = Harness::spawn().await;
    let response = http()
        .get(format!(
            "{}/v1/sessions/sess_does_not_exist/snapshot",
            harness.base_url
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("snapshot");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn sessions_changes_returns_an_empty_ephemeral_snapshot_and_validates_target() {
    let harness = Harness::spawn_with(|config| {
        config.agent.args.push("--no-cap-list-session".into());
    })
    .await;
    let session_id = "sess_changes_empty".to_owned();
    {
        let store = harness.state.lock().await;
        store
            .insert_session(NewSessionRecord {
                id: session_id.clone(),
                agent_id: "placebo".to_owned(),
                cwd: "/tmp/changes".to_owned(),
                title: None,
                metadata_json: "{}".to_owned(),
            })
            .expect("session inserted");
    }

    let response = http()
        .get(format!(
            "{}/v1/sessions/{}/changes",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("changes request");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("changes JSON");
    assert_eq!(body["data"]["session_id"], session_id);
    assert_eq!(body["data"]["revision"], 0);
    assert_eq!(body["data"]["truncated"], false);
    assert_eq!(body["data"]["tool_calls"], json!([]));
    assert_eq!(
        body["data"]["generation"]
            .as_str()
            .expect("generation")
            .len(),
        32
    );

    let wrong_target = http()
        .get(format!(
            "{}/v1/sessions/{}/changes?target_id=wrong",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("wrong target request");
    assert_eq!(wrong_target.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn sessions_changes_returns_404_for_unknown_session() {
    let harness = Harness::spawn().await;
    let response = http()
        .get(format!(
            "{}/v1/sessions/sess_does_not_exist/changes",
            harness.base_url
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("changes request");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn sessions_snapshot_caps_recent_events_at_50() {
    let harness = Harness::spawn_with(|config| {
        config.agent.args.push("--no-cap-list-session".into());
    })
    .await;
    let session_id = "sess_snapshot_cap".to_owned();
    {
        let store = harness.state.lock().await;
        store
            .insert_session(NewSessionRecord {
                id: session_id.clone(),
                agent_id: "placebo".to_owned(),
                cwd: "/tmp/cap".to_owned(),
                title: None,
                metadata_json: "{}".to_owned(),
            })
            .expect("session inserted");
        for index in 0..75 {
            store
                .append_session_event_with_source(
                    &session_id,
                    "info",
                    "session.update",
                    acp_stack::state::EVENT_SOURCE_ACP,
                    "ACP session update",
                    &format!(r#"{{"seq":{index}}}"#),
                )
                .expect("event inserted");
        }
    }

    let body: Value = http()
        .get(format!(
            "{}/v1/sessions/{}/snapshot",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("snapshot")
        .json()
        .await
        .expect("snapshot json");

    let events = body["data"]["recent_events"]
        .as_array()
        .expect("recent_events array");
    assert_eq!(events.len(), 50);
    // The cap should keep the newest 50, so the head still carries `"seq":74`.
    let head_payload: Value = serde_json::from_str(events[0]["payload_json"].as_str().unwrap())
        .expect("head payload json");
    assert_eq!(head_payload["seq"], 74);
}
