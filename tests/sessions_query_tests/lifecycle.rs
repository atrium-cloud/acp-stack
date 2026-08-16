use crate::common::sessions::{Harness, admin_bearer, create_session, http, session_bearer};
use reqwest::StatusCode;
use serde_json::{Value, json};

#[tokio::test]
async fn available_session_must_be_loaded_before_prompting() {
    let harness = Harness::spawn().await;
    let client = http();
    let list: Value = client
        .get(format!("{}/v1/sessions", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("list")
        .json()
        .await
        .expect("list json");
    let session_id = list["data"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["agent_session_id"] == "sess_listed_0")
        .and_then(|session| session["id"].as_str())
        .expect("listed session local id");

    let response = client
        .post(format!(
            "{}/v1/sessions/{}/prompt",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({ "prompt": "hello agent" }))
        .send()
        .await
        .expect("prompt");
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body: Value = response.json().await.expect("prompt json");
    assert_eq!(body["error"]["code"], "session.not_active");
}

#[tokio::test]
async fn unsupported_capability_load_returns_501() {
    let harness = Harness::spawn_with(|config| {
        config.agent.args.push("--no-cap-load-session".into());
    })
    .await;
    let session_id = create_session(&harness).await;
    let client = http();

    let response = client
        .post(format!(
            "{}/v1/sessions/{}/load",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .header("Content-Type", "application/json")
        .body("{}")
        .send()
        .await
        .expect("load");
    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "agent.unsupported_capability");
}

#[tokio::test]
async fn unsupported_capability_resume_returns_501() {
    let harness = Harness::spawn_with(|config| {
        config.agent.args.push("--no-cap-resume-session".into());
    })
    .await;
    let session_id = create_session(&harness).await;
    let client = http();

    let response = client
        .post(format!(
            "{}/v1/sessions/{}/resume",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .header("Content-Type", "application/json")
        .body("{}")
        .send()
        .await
        .expect("resume");
    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "agent.unsupported_capability");
}

#[tokio::test]
async fn unsupported_capability_close_returns_501_and_leaves_session_active() {
    let harness = Harness::spawn_with(|config| {
        config.agent.args.push("--no-cap-close-session".into());
    })
    .await;
    let session_id = create_session(&harness).await;
    let client = http();

    let response = client
        .delete(format!("{}/v1/sessions/{}", harness.base_url, session_id))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("close");
    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "agent.unsupported_capability");

    let session: Value = client
        .get(format!("{}/v1/sessions/{}", harness.base_url, session_id))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("get json");
    assert_eq!(session["data"]["status"], "active");
}

#[tokio::test]
async fn session_routes_reject_admin_keys() {
    let harness = Harness::spawn().await;
    let client = http();
    let response = client
        .get(format!("{}/v1/sessions", harness.base_url))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("list");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "auth.wrong_kind");
}

#[tokio::test]
async fn unknown_session_returns_404() {
    let harness = Harness::spawn().await;
    let client = http();
    let response = client
        .get(format!(
            "{}/v1/sessions/sess_does_not_exist",
            harness.base_url
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("get");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "session.not_found");
}

#[tokio::test]
async fn unknown_prompt_returns_404() {
    let harness = Harness::spawn().await;
    let session_id = create_session(&harness).await;
    let client = http();
    let response = client
        .get(format!(
            "{}/v1/sessions/{}/prompts/prm_missing",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("get");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "prompt.not_found");
}
