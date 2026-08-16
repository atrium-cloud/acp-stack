use crate::common::sessions::{Harness, create_session, http, session_bearer};
use acp_stack::state::NewSessionRecord;
use reqwest::StatusCode;
use serde_json::Value;

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
