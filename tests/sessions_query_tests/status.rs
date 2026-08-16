use std::time::Duration;

use crate::common::sessions::{Harness, http, session_bearer};
use acp_stack::config::{ArrayTargetConfig, Config};
use acp_stack::state::{NewPermissionRequest, NewPromptRecord, NewSessionRecord, PromptStatus};
use reqwest::StatusCode;
use serde_json::Value;

#[tokio::test]
async fn sessions_status_returns_compact_active_summary() {
    let harness = Harness::spawn_with(|config| {
        config.agent.args.push("--no-cap-list-session".into());
    })
    .await;
    {
        let store = harness.state.lock().await;
        store
            .insert_session(NewSessionRecord {
                id: "sess_active".to_owned(),
                agent_id: "placebo".to_owned(),
                cwd: "/tmp/active".to_owned(),
                title: Some("active title".to_owned()),
                metadata_json: r#"{"secretish":"not returned"}"#.to_owned(),
            })
            .expect("session inserted");
        store
            .append_session_event_with_source(
                "sess_active",
                "info",
                "session.update",
                acp_stack::state::EVENT_SOURCE_ACP,
                "ACP session update",
                "{}",
            )
            .expect("event inserted");
    }

    let body: Value = http()
        .get(format!("{}/v1/sessions/-/status", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("status")
        .json()
        .await
        .expect("status json");

    assert_eq!(body["data"]["active_count"], 1);
    assert_eq!(body["data"]["session_count"], 1);
    assert_eq!(body["data"]["window"], "8h");
    assert!(body["data"]["window_start"].is_string());
    assert!(body["data"]["window_end"].is_string());
    let session = &body["data"]["sessions"][0];
    assert_eq!(session["id"], "sess_active");
    assert_eq!(session["state"], "idle");
    assert_eq!(session["last_activity_from"], "agent");
    assert_eq!(session["recent"], true);
    assert!(session.get("metadata_json").is_none());
}

#[tokio::test]
async fn sessions_status_defaults_to_primary_target() {
    let harness = Harness::spawn_with(|config| {
        config.agent.args.push("--no-cap-list-session".into());
    })
    .await;
    {
        let store = harness.state.lock().await;
        store
            .insert_session(NewSessionRecord {
                id: "sess_primary".to_owned(),
                agent_id: "placebo".to_owned(),
                cwd: "/tmp/primary".to_owned(),
                title: None,
                metadata_json: "{}".to_owned(),
            })
            .expect("primary session inserted");
        store
            .insert_session_for_target(
                "placebo-secondary",
                "acp_secondary".to_owned(),
                NewSessionRecord {
                    id: "sess_secondary".to_owned(),
                    agent_id: "placebo-secondary".to_owned(),
                    cwd: "/tmp/secondary".to_owned(),
                    title: None,
                    metadata_json: "{}".to_owned(),
                },
            )
            .expect("secondary session inserted");
    }

    let body: Value = http()
        .get(format!("{}/v1/sessions/-/status", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("status")
        .json()
        .await
        .expect("status json");

    let ids: Vec<&str> = body["data"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["sess_primary"]);
}

#[tokio::test]
async fn sessions_target_obeys_array_off_written_after_daemon_start() {
    let harness = Harness::spawn_with(|config| {
        config.array.enabled = true;
        let mut secondary = config.agent.clone();
        secondary.id = "placebo-secondary".to_owned();
        secondary.name = "Placebo Secondary".to_owned();
        config.array.targets.push(ArrayTargetConfig {
            id: "placebo-secondary".to_owned(),
            agent: secondary,
        });
    })
    .await;
    let mut updated =
        Config::load_from_path(&harness.config_path).expect("config should load from disk");
    updated.array.enabled = false;
    std::fs::write(
        &harness.config_path,
        updated.to_canonical_toml().expect("canonical config"),
    )
    .expect("config should be rewritten");

    let response = http()
        .get(format!(
            "{}/v1/sessions?target=placebo-secondary",
            harness.base_url
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("list");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "request.invalid_param");
}

#[tokio::test]
async fn sessions_status_marks_old_activity_idle() {
    let harness = Harness::spawn_with(|config| {
        config.agent.args.push("--no-cap-list-session".into());
    })
    .await;
    {
        let store = harness.state.lock().await;
        store
            .insert_session(NewSessionRecord {
                id: "sess_active".to_owned(),
                agent_id: "placebo".to_owned(),
                cwd: "/tmp/active".to_owned(),
                title: None,
                metadata_json: "{}".to_owned(),
            })
            .expect("session inserted");
    }

    let body: Value = http()
        .get(format!(
            "{}/v1/sessions/-/status?threshold=0s",
            harness.base_url
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("status")
        .json()
        .await
        .expect("status json");

    assert_eq!(body["data"]["sessions"][0]["recent"], false);
}

#[tokio::test]
async fn sessions_status_rejects_malformed_threshold() {
    let harness = Harness::spawn().await;
    let response = http()
        .get(format!(
            "{}/v1/sessions/-/status?threshold=not-a-duration",
            harness.base_url
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("status");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "request.invalid_param");
}

#[tokio::test]
async fn sessions_status_rejects_window_outside_bounds() {
    let harness = Harness::spawn().await;
    let response = http()
        .get(format!(
            "{}/v1/sessions/-/status?window=1000h",
            harness.base_url
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("status");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "request.invalid_param");
}

#[tokio::test]
async fn sessions_status_reports_turn_states() {
    let harness = Harness::spawn_with(|config| {
        config.agent.args.push("--no-cap-list-session".into());
    })
    .await;
    {
        let store = harness.state.lock().await;
        for session_id in [
            "sess_prompt_sent",
            "sess_working",
            "sess_done",
            "sess_error",
            "sess_permission",
        ] {
            store
                .insert_session(NewSessionRecord {
                    id: session_id.to_owned(),
                    agent_id: "placebo".to_owned(),
                    cwd: format!("/tmp/{session_id}"),
                    title: None,
                    metadata_json: "{}".to_owned(),
                })
                .expect("session inserted");
            let prompt_id = format!("prm_{session_id}");
            store
                .insert_prompt(NewPromptRecord {
                    id: prompt_id.clone(),
                    session_id: session_id.to_owned(),
                    prompt_json: "[]".to_owned(),
                })
                .expect("prompt inserted");
            store
                .update_prompt_status(
                    &prompt_id,
                    PromptStatus::Running,
                    None,
                    None,
                    None,
                    None,
                    None,
                )
                .expect("prompt running");
        }

        std::thread::sleep(Duration::from_millis(2));
        for session_id in ["sess_working", "sess_permission"] {
            store
                .append_session_event_with_source(
                    session_id,
                    "info",
                    "session.update",
                    acp_stack::state::EVENT_SOURCE_ACP,
                    "ACP session update",
                    "{}",
                )
                .expect("session update");
        }
        store
            .update_prompt_status(
                "prm_sess_done",
                PromptStatus::Completed,
                Some("end_turn"),
                None,
                None,
                None,
                None,
            )
            .expect("prompt completed");
        store
            .update_prompt_status(
                "prm_sess_error",
                PromptStatus::Errored,
                None,
                Some("agent.request_failed"),
                Some("failed"),
                None,
                None,
            )
            .expect("prompt errored");
        store
            .append_permission_request(NewPermissionRequest {
                source: "acp",
                requester: Some("agent"),
                subject_id: Some("sess_permission"),
                detail_json: "{}",
                expires_at: None,
            })
            .expect("permission inserted");
    }

    let body: Value = http()
        .get(format!(
            "{}/v1/sessions/-/status?window=1h",
            harness.base_url
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("status")
        .json()
        .await
        .expect("status json");
    let sessions = body["data"]["sessions"].as_array().expect("sessions array");
    let state_for = |id: &str| {
        sessions
            .iter()
            .find(|session| session["id"] == id)
            .and_then(|session| session["state"].as_str())
            .unwrap_or_else(|| panic!("missing state for {id}; body={body}"))
    };

    assert_eq!(state_for("sess_prompt_sent"), "prompt_sent");
    assert_eq!(state_for("sess_working"), "working");
    assert_eq!(state_for("sess_done"), "done");
    assert_eq!(state_for("sess_error"), "error");
    assert_eq!(state_for("sess_permission"), "permission_required");
    let permission_session = sessions
        .iter()
        .find(|session| session["id"] == "sess_permission")
        .expect("permission session");
    assert!(permission_session["permission"]["id"].is_string());
}
