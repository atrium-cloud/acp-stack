use crate::common::sessions::{Harness, admin_bearer, create_session, http, session_bearer};
use acp_stack::state::SessionAvailableCommand;
use reqwest::StatusCode;
use serde_json::{Value, json};

fn advertised_fixture() -> Vec<SessionAvailableCommand> {
    vec![
        SessionAvailableCommand {
            name: "compact".to_owned(),
            description: "Summarize the conversation".to_owned(),
            input_hint: Some("optional instructions".to_owned()),
        },
        SessionAvailableCommand {
            name: "init".to_owned(),
            description: "Create AGENTS.md".to_owned(),
            input_hint: None,
        },
    ]
}

#[tokio::test]
async fn sessions_commands_lists_stored_advertisement() {
    let harness = Harness::spawn().await;
    let session_id = create_session(&harness).await;
    let client = http();

    // Before any advertisement the list is empty and updated_at is null.
    let body: Value = client
        .get(format!(
            "{}/v1/sessions/{}/commands",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("commands get")
        .json()
        .await
        .expect("commands json");
    assert_eq!(body["data"]["available_commands"], json!([]));
    assert!(body["data"]["updated_at"].is_null());

    {
        let store = harness.state.lock().await;
        store
            .replace_session_available_commands(&session_id, &advertised_fixture())
            .expect("commands stored");
    }

    let body: Value = client
        .get(format!(
            "{}/v1/sessions/{}/commands",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("commands get")
        .json()
        .await
        .expect("commands json");
    let commands = body["data"]["available_commands"]
        .as_array()
        .expect("commands array");
    assert_eq!(commands.len(), 2);
    assert_eq!(commands[0]["name"], "compact");
    assert_eq!(commands[0]["input_hint"], "optional instructions");
    assert!(commands[1].get("input_hint").is_none());
    assert!(body["data"]["updated_at"].is_string());
}

#[tokio::test]
async fn sessions_commands_run_submits_prompt_with_advisory_flag() {
    let harness = Harness::spawn().await;
    let session_id = create_session(&harness).await;
    let client = http();

    // No list ever advertised: the flag is omitted and the prompt submits.
    let body: Value = client
        .post(format!(
            "{}/v1/sessions/{}/commands",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({ "command": "compact" }))
        .send()
        .await
        .expect("run")
        .json()
        .await
        .expect("run json");
    assert!(body["data"]["prompt_id"].is_string());
    assert!(body["data"].get("advertised").is_none());

    {
        let store = harness.state.lock().await;
        store
            .replace_session_available_commands(&session_id, &advertised_fixture())
            .expect("commands stored");
    }

    // A leading slash is normalized even behind surrounding whitespace; the
    // composed prompt text carries exactly one slash plus the args.
    let body: Value = client
        .post(format!(
            "{}/v1/sessions/{}/commands",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({ "command": " /compact ", "args": "keep decisions" }))
        .send()
        .await
        .expect("run")
        .json()
        .await
        .expect("run json");
    assert_eq!(body["data"]["advertised"], true);
    let prompt_id = body["data"]["prompt_id"].as_str().expect("prompt id");
    {
        let store = harness.state.lock().await;
        let prompt = store
            .get_prompt(prompt_id)
            .expect("prompt lookup")
            .expect("prompt exists");
        let blocks: Value = serde_json::from_str(&prompt.prompt_json).expect("prompt json");
        assert_eq!(blocks[0]["text"], "/compact keep decisions");
    }

    // Unadvertised commands still submit, flagged advisory-false.
    let body: Value = client
        .post(format!(
            "{}/v1/sessions/{}/commands",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({ "command": "definitely-not-advertised" }))
        .send()
        .await
        .expect("run")
        .json()
        .await
        .expect("run json");
    assert_eq!(body["data"]["advertised"], false);
    assert!(body["data"]["prompt_id"].is_string());
}

#[tokio::test]
async fn sessions_commands_run_rejects_empty_name_and_admin_key() {
    let harness = Harness::spawn().await;
    let session_id = create_session(&harness).await;
    let client = http();

    let empty = client
        .post(format!(
            "{}/v1/sessions/{}/commands",
            harness.base_url, session_id
        ))
        .header("Authorization", session_bearer())
        .json(&json!({ "command": "/" }))
        .send()
        .await
        .expect("empty run");
    assert_eq!(empty.status(), StatusCode::BAD_REQUEST);

    let wrong_kind = client
        .get(format!(
            "{}/v1/sessions/{}/commands",
            harness.base_url, session_id
        ))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("admin get");
    assert_eq!(wrong_kind.status(), StatusCode::UNAUTHORIZED);
    let body: Value = wrong_kind.json().await.expect("json");
    assert_eq!(body["error"]["code"], "auth.wrong_kind");
}
