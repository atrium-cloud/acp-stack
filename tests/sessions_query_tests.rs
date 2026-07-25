#![cfg(feature = "test-fixtures")]

//! Read-side coverage for the session routes: list (agent sync, time bounds,
//! target resolution), the compact `-/status` summary, snapshot/changes, and
//! the not-found / unsupported-capability / auth-tier error paths.
//!
//! The placebo ACP fixture stands in for a real ACP agent;
//! `tests/acp_bridge_tests.rs` exercises the lower-level bridge layer.

mod common;

use std::time::Duration;

use acp_stack::config::{ArrayTargetConfig, Config};
use acp_stack::state::{NewPermissionRequest, NewPromptRecord, NewSessionRecord, PromptStatus};
use common::sessions::{Harness, admin_bearer, create_session, http, session_bearer};
use reqwest::StatusCode;
use serde_json::{Value, json};

#[tokio::test]
async fn sessions_list_syncs_agent_discovered_sessions() {
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

    assert_eq!(list["data"]["agent_sync"]["attempted"], true);
    assert_eq!(list["data"]["agent_sync"]["status"], "synced");
    assert_eq!(list["data"]["agent_sync"]["upserted"], 1);
    let listed = list["data"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["agent_session_id"] == "sess_listed_0")
        .expect("listed session present");
    assert!(listed["id"].as_str().is_some_and(|id| !id.is_empty()));
    assert_eq!(listed["status"], "available");
    assert_eq!(listed["title"], "listed session");
    let metadata: Value =
        serde_json::from_str(listed["metadata_json"].as_str().unwrap()).expect("metadata json");
    assert_eq!(metadata["agent_meta"]["origin"], "placebo-agent");
}

#[tokio::test]
async fn sessions_list_skips_agent_discovered_cwd_outside_workspace() {
    let outside = tempfile::tempdir().expect("outside");
    let harness = Harness::spawn_with(|config| {
        config.agent.args.extend([
            "--listed-cwd".to_owned(),
            outside.path().to_string_lossy().into_owned(),
        ]);
    })
    .await;

    let list: Value = http()
        .get(format!("{}/v1/sessions", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("list")
        .json()
        .await
        .expect("list json");

    assert_eq!(list["data"]["agent_sync"]["attempted"], true);
    assert_eq!(list["data"]["agent_sync"]["status"], "synced");
    assert_eq!(list["data"]["agent_sync"]["upserted"], 0);
    let listed = list["data"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["id"] == "sess_listed_0");
    assert!(listed.is_none(), "invalid listed cwd must be skipped");
}

#[tokio::test]
async fn sessions_list_preserves_active_local_sessions() {
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

    let active = list["data"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["id"].as_str() == Some(session_id.as_str()))
        .expect("created session present");
    assert_eq!(active["status"], "active");
    assert_eq!(list["data"]["agent_sync"]["updated"], 1);
}

#[tokio::test]
async fn sessions_list_works_when_agent_list_is_unsupported() {
    let harness = Harness::spawn_with(|config| {
        config.agent.args.push("--no-cap-list-session".into());
    })
    .await;
    let client = http();

    let response = client
        .get(format!("{}/v1/sessions", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("list");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("list json");
    assert_eq!(body["data"]["agent_sync"]["attempted"], false);
    assert_eq!(body["data"]["agent_sync"]["status"], "unsupported");
    assert!(body["data"]["sessions"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn sessions_list_filters_by_since_and_until() {
    let harness = Harness::spawn_with(|config| {
        config.agent.args.push("--no-cap-list-session".into());
    })
    .await;
    {
        let store = harness.state.lock().await;
        store
            .upsert_listed_sessions(vec![
                acp_stack::state::ListedSessionRecord {
                    id: "sess_old".to_owned(),
                    agent_session_id: "sess_old".to_owned(),
                    agent_id: "placebo".to_owned(),
                    cwd: "/tmp/old".to_owned(),
                    title: None,
                    updated_at: Some("2026-01-01T00:00:00Z".to_owned()),
                    metadata_json: "{}".to_owned(),
                },
                acp_stack::state::ListedSessionRecord {
                    id: "sess_mid".to_owned(),
                    agent_session_id: "sess_mid".to_owned(),
                    agent_id: "placebo".to_owned(),
                    cwd: "/tmp/mid".to_owned(),
                    title: None,
                    updated_at: Some("2026-02-01T00:00:00Z".to_owned()),
                    metadata_json: "{}".to_owned(),
                },
                acp_stack::state::ListedSessionRecord {
                    id: "sess_new".to_owned(),
                    agent_session_id: "sess_new".to_owned(),
                    agent_id: "placebo".to_owned(),
                    cwd: "/tmp/new".to_owned(),
                    title: None,
                    updated_at: Some("2026-03-01T00:00:00Z".to_owned()),
                    metadata_json: "{}".to_owned(),
                },
            ])
            .expect("sessions inserted");
    }
    let client = http();
    let body: Value = client
        .get(format!(
            "{}/v1/sessions?since=2026-01-15T00%3A00%3A00Z&until=2026-02-15T00%3A00%3A00Z",
            harness.base_url
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("list")
        .json()
        .await
        .expect("list json");

    let ids: Vec<&str> = body["data"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["sess_mid"]);
}

#[tokio::test]
async fn sessions_list_rejects_malformed_bounds() {
    let harness = Harness::spawn_with(|config| {
        config.agent.args.push("--no-cap-list-session".into());
    })
    .await;
    let response = http()
        .get(format!("{}/v1/sessions?since=not-a-time", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("list");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "request.invalid_param");
}

#[tokio::test]
async fn sessions_list_rejects_duration_before_unix_epoch() {
    let harness = Harness::spawn_with(|config| {
        config.agent.args.push("--no-cap-list-session".into());
    })
    .await;
    let response = http()
        .get(format!(
            "{}/v1/sessions?range=999999999999999999y",
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
async fn sessions_list_resolves_missing_explicit_bound_to_session_span() {
    let harness = Harness::spawn_with(|config| {
        config.agent.args.push("--no-cap-list-session".into());
    })
    .await;
    {
        let store = harness.state.lock().await;
        store
            .upsert_listed_sessions(vec![
                acp_stack::state::ListedSessionRecord {
                    id: "sess_first".to_owned(),
                    agent_session_id: "sess_first".to_owned(),
                    agent_id: "placebo".to_owned(),
                    cwd: "/tmp/first".to_owned(),
                    title: None,
                    updated_at: Some("2026-02-01T00:00:00Z".to_owned()),
                    metadata_json: "{}".to_owned(),
                },
                acp_stack::state::ListedSessionRecord {
                    id: "sess_latest".to_owned(),
                    agent_session_id: "sess_latest".to_owned(),
                    agent_id: "placebo".to_owned(),
                    cwd: "/tmp/latest".to_owned(),
                    title: None,
                    updated_at: Some("2026-02-02T00:00:00Z".to_owned()),
                    metadata_json: "{}".to_owned(),
                },
            ])
            .expect("sessions inserted");
    }

    let body: Value = http()
        .get(format!(
            "{}/v1/sessions?resolve_bounds=true&until=2026-02-01T12%3A00%3A00Z",
            harness.base_url
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("list")
        .json()
        .await
        .expect("list json");
    let ids: Vec<&str> = body["data"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["sess_first"]);

    let body: Value = http()
        .get(format!(
            "{}/v1/sessions?resolve_bounds=true&since=2026-02-01T12%3A00%3A00Z",
            harness.base_url
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("list")
        .json()
        .await
        .expect("list json");
    let ids: Vec<&str> = body["data"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["sess_latest"]);
}

#[tokio::test]
async fn sessions_list_range_counts_from_request_time() {
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
        .get(format!("{}/v1/sessions?range=30m", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("list")
        .json()
        .await
        .expect("list json");

    let ids: Vec<&str> = body["data"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["sess_active"]);
}

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
