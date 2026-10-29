#![cfg(feature = "test-fixtures")]

//! Subagent sessions an agent announces through ACP `subagent_update`, driven by
//! the placebo's `--announce-subagent`: each child streams into its own session,
//! its permission requests reach the operator, the client may not drive it, and
//! closing or deleting the root takes the tree with it.

mod common;

use std::time::Duration;

use common::sessions::{Harness, create_session, http, session_bearer};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

/// Comfortably above the placebo's own delays: a turn that has not settled by
/// then is a real failure, not a slow box.
const POLL_BUDGET: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(50);
/// Well under the supervisor's 20s cancel budget, so a cancel that only returns
/// after the budget expires fails this rather than passing slowly.
const CANCEL_NO_STALL_BUDGET: Duration = Duration::from_secs(10);

#[tokio::test]
async fn a_subagent_streams_into_its_own_session_and_its_permission_reaches_the_operator() {
    let harness = spawn(&["--announce-subagent", "--subagent-permission"]).await;
    let root = create_session(&harness).await;
    let prompt_id = submit_prompt(&harness, &root).await;

    let [child] = await_subagents(&harness, &root).await;
    let permission_id = await_pending_permission(&harness, &child).await;
    assert!(
        session_events(&harness, &child)
            .await
            .iter()
            .any(|event| event["payload_json"]
                .as_str()
                .is_some_and(|payload| payload.contains("subagent-chunk"))),
        "the subagent's chunk lands in its own log"
    );
    let approve = request(
        &harness,
        Method::POST,
        &format!("/v1/permissions/{permission_id}/approve"),
    )
    .await;
    assert_eq!(approve.status(), StatusCode::OK);

    await_prompt_status(&harness, &root, &prompt_id, "completed").await;
}

/// The root's cancel ends the subagent's work without answering its request;
/// once the subagent reports `idle` with `cancelled`, the client settles the
/// request, which is what lets the agent end the root's turn.
#[tokio::test]
async fn cancelling_the_root_settles_the_request_its_cancelled_subagent_left_open() {
    let harness = spawn(&["--announce-subagent", "--subagent-permission"]).await;
    let root = create_session(&harness).await;
    let prompt_id = submit_prompt(&harness, &root).await;
    let [child] = await_subagents(&harness, &root).await;
    let permission_id = await_pending_permission(&harness, &child).await;

    let cancel = tokio::time::timeout(
        CANCEL_NO_STALL_BUDGET,
        request(
            &harness,
            Method::POST,
            &format!("/v1/sessions/{root}/cancel"),
        ),
    )
    .await
    .expect("cancel returned well inside the settle budget");
    assert_eq!(cancel.status(), StatusCode::OK);

    let cancelled = body(
        request(
            &harness,
            Method::GET,
            "/v1/logs/permissions?kind=permission.cancelled",
        )
        .await,
    )
    .await;
    let reasons: Vec<Value> = cancelled["data"]["events"]
        .as_array()
        .expect("events array")
        .iter()
        .map(|event| {
            serde_json::from_str::<Value>(event["payload_json"].as_str().expect("payload"))
                .expect("payload json")
        })
        .filter(|payload| payload["permission_id"] == permission_id.as_str())
        .map(|payload| payload["reason"].clone())
        .collect();
    assert_eq!(reasons, [json!("subagent-cancelled")]);
    let prompt = body(
        request(
            &harness,
            Method::GET,
            &format!("/v1/sessions/{root}/prompts/{prompt_id}"),
        )
        .await,
    )
    .await;
    assert_eq!(prompt["data"]["stop_reason"], "cancelled", "{prompt}");
}

#[tokio::test]
async fn client_mutations_on_a_subagent_session_are_refused() {
    let harness = spawn(&["--announce-subagent"]).await;
    let root = create_session(&harness).await;
    let prompt_id = submit_prompt(&harness, &root).await;
    await_prompt_status(&harness, &root, &prompt_id, "completed").await;
    let [child] = await_subagents(&harness, &root).await;

    let read = request(&harness, Method::GET, &format!("/v1/sessions/{child}")).await;
    assert_eq!(read.status(), StatusCode::OK);
    for (method, route, payload) in [
        (Method::POST, "/prompt", json!({ "prompt": "hello" })),
        (Method::POST, "/commands", json!({ "command": "review" })),
        (Method::POST, "/load", json!({})),
        (Method::POST, "/resume", json!({})),
        (Method::POST, "/fork", json!({})),
        (
            Method::POST,
            "/config-options",
            json!({ "config_id": "model", "value": "other" }),
        ),
        (Method::POST, "/cancel", json!({})),
        (Method::POST, "/delete", json!({})),
        (Method::DELETE, "", json!({})),
    ] {
        let response = http()
            .request(
                method.clone(),
                format!("{}/v1/sessions/{child}{route}", harness.base_url),
            )
            .header("Authorization", session_bearer())
            .json(&payload)
            .send()
            .await
            .expect("mutation request");
        assert_eq!(response.status(), StatusCode::CONFLICT, "{method} {route}");
        let error = body(response).await;
        assert_eq!(
            error["error"]["code"], "session.subagent_restricted",
            "{method} {route}: {error}"
        );
    }
}

#[tokio::test]
async fn closing_and_deleting_the_root_take_its_subagents_with_it() {
    let harness = spawn(&["--announce-subagent", "--subagent-nested"]).await;
    let root = create_session(&harness).await;
    let prompt_id = submit_prompt(&harness, &root).await;
    await_prompt_status(&harness, &root, &prompt_id, "completed").await;
    let tree: [String; 2] = await_subagents(&harness, &root).await;

    let close = request(&harness, Method::DELETE, &format!("/v1/sessions/{root}")).await;
    assert_eq!(close.status(), StatusCode::OK);
    for id in &tree {
        let session =
            body(request(&harness, Method::GET, &format!("/v1/sessions/{id}")).await).await;
        assert_eq!(session["data"]["status"], "closed", "{session}");
    }

    let delete = body(
        request(
            &harness,
            Method::POST,
            &format!("/v1/sessions/{root}/delete"),
        )
        .await,
    )
    .await;
    assert_eq!(delete["data"]["deleted"], true, "{delete}");
    for id in &tree {
        let gone = request(&harness, Method::GET, &format!("/v1/sessions/{id}")).await;
        assert_eq!(gone.status(), StatusCode::GONE, "{id}");
    }
}

async fn spawn(flags: &[&str]) -> Harness {
    Harness::spawn_with(|config| {
        config
            .agent
            .args
            .extend(flags.iter().map(|flag| (*flag).to_owned()));
    })
    .await
}

async fn request(harness: &Harness, method: Method, path: &str) -> reqwest::Response {
    http()
        .request(method, format!("{}{path}", harness.base_url))
        .header("Authorization", session_bearer())
        .json(&json!({}))
        .send()
        .await
        .expect("request")
}

async fn body(response: reqwest::Response) -> Value {
    response.json().await.expect("response json")
}

async fn submit_prompt(harness: &Harness, session_id: &str) -> String {
    let response = http()
        .post(format!(
            "{}/v1/sessions/{session_id}/prompt",
            harness.base_url
        ))
        .header("Authorization", session_bearer())
        .json(&json!({ "prompt": "delegate this" }))
        .send()
        .await
        .expect("submit prompt");
    assert_eq!(response.status(), StatusCode::OK);
    body(response).await["data"]["prompt_id"]
        .as_str()
        .expect("prompt id")
        .to_owned()
}

/// Local ids of the subagents in the tree `root` heads, once there are `N`.
async fn await_subagents<const N: usize>(harness: &Harness, root: &str) -> [String; N] {
    let deadline = tokio::time::Instant::now() + POLL_BUDGET;
    loop {
        let lookup_root = root.to_owned();
        let ids: Vec<String> = harness
            .state
            .read(move |store| store.subagent_sessions(&lookup_root))
            .await
            .expect("subagent sessions")
            .into_iter()
            .map(|session| session.id)
            .collect();
        if let Ok(ids) = <[String; N]>::try_from(ids) {
            return ids;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "`{root}` never announced {N} subagents"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn await_pending_permission(harness: &Harness, subject_id: &str) -> String {
    let deadline = tokio::time::Instant::now() + POLL_BUDGET;
    loop {
        let pending = body(request(harness, Method::GET, "/v1/permissions/pending").await).await;
        if let Some(row) = pending["data"]["permissions"]
            .as_array()
            .expect("permissions array")
            .iter()
            .find(|row| row["source"] == "acp" && row["subject_id"].as_str() == Some(subject_id))
        {
            return row["id"].as_str().expect("permission id").to_owned();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no pending permission for `{subject_id}`"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn session_events(harness: &Harness, session_id: &str) -> Vec<Value> {
    body(
        request(
            harness,
            Method::GET,
            &format!("/v1/sessions/{session_id}/events"),
        )
        .await,
    )
    .await["data"]["events"]
        .as_array()
        .expect("events array")
        .clone()
}

async fn await_prompt_status(harness: &Harness, session_id: &str, prompt_id: &str, want: &str) {
    let deadline = tokio::time::Instant::now() + POLL_BUDGET;
    loop {
        let record = body(
            request(
                harness,
                Method::GET,
                &format!("/v1/sessions/{session_id}/prompts/{prompt_id}"),
            )
            .await,
        )
        .await;
        if record["data"]["status"] == want {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "prompt `{prompt_id}` never reached `{want}`: {record}"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}
