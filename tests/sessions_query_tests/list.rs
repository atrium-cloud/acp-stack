use crate::common::sessions::{Harness, create_session, http, session_bearer};
use acp_stack::state::NewSessionRecord;
use reqwest::StatusCode;
use serde_json::{Value, json};

#[tokio::test]
async fn session_change_feed_pages_ascending_after_the_cursor_and_matches_the_list() {
    let harness = Harness::spawn_with(|config| {
        // Without `session/list` the list route leaves the seeded rows alone.
        config.agent.args.push("--no-cap-list-session".into());
    })
    .await;
    let baseline: Value = http()
        .get(format!("{}/v1/sessions/-/changes", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("baseline feed")
        .json()
        .await
        .expect("baseline feed json");
    let start = baseline["data"]["head"].as_u64().expect("head");
    {
        let store = harness.state.lock().await;
        for id in ["sess_feed_route_a", "sess_feed_route_b"] {
            store
                .insert_session(NewSessionRecord {
                    id: id.to_owned(),
                    agent_id: "placebo".to_owned(),
                    cwd: "/tmp/feed".to_owned(),
                    title: None,
                    metadata_json: "{}".to_owned(),
                })
                .expect("session inserted");
        }
        for _ in 0..2 {
            store
                .append_session_event("sess_feed_route_a", "info", "session.update", "", "{}")
                .expect("event appended");
        }
    }
    let feed = |query: String| {
        let url = format!("{}/v1/sessions/-/changes{query}", harness.base_url);
        async move {
            let response = http()
                .get(url)
                .header("Authorization", session_bearer())
                .send()
                .await
                .expect("feed request");
            assert_eq!(response.status(), StatusCode::OK);
            response.json::<Value>().await.expect("feed json")
        }
    };

    let page = feed(format!("?after={start}")).await;
    assert_eq!(
        page["data"]["changes"],
        json!([
            { "session_id": "sess_feed_route_b", "change_seq": start + 2, "deleted": false },
            { "session_id": "sess_feed_route_a", "change_seq": start + 4, "deleted": false },
        ])
    );
    assert_eq!(page["data"]["head"], start + 4);
    assert_eq!(page["data"]["pruned_through"], 0);
    let epoch = page["data"]["feed_epoch"]
        .as_str()
        .expect("feed_epoch")
        .to_owned();
    assert_eq!(epoch.len(), 22, "base64url of 16 bytes: {epoch}");
    assert_eq!(
        baseline["data"]["feed_epoch"], epoch,
        "one process keeps one epoch"
    );
    let after_b = feed(format!("?after={}", start + 2)).await;
    assert_eq!(
        after_b["data"]["changes"],
        json!([{ "session_id": "sess_feed_route_a", "change_seq": start + 4, "deleted": false }])
    );
    let first = feed(format!("?after={start}&limit=1")).await;
    assert_eq!(
        first["data"]["changes"].as_array().expect("changes").len(),
        1
    );
    let clamped = feed("?limit=5000".to_owned()).await;
    assert_eq!(clamped["data"]["head"], start + 4);
    // A zero limit still returns the next change rather than an empty page.
    let floored = feed(format!("?after={start}&limit=0")).await;
    assert_eq!(
        floored["data"]["changes"],
        json!([{ "session_id": "sess_feed_route_b", "change_seq": start + 2, "deleted": false }])
    );

    let list: Value = http()
        .get(format!("{}/v1/sessions", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("list")
        .json()
        .await
        .expect("list json");
    let listed = |id: &str| -> (u64, u64) {
        let session = list["data"]["sessions"]
            .as_array()
            .expect("sessions array")
            .iter()
            .find(|session| session["id"] == id)
            .unwrap_or_else(|| panic!("{id} listed"));
        (
            session["change_seq"].as_u64().expect("change_seq"),
            session["event_seq"].as_u64().expect("event_seq"),
        )
    };
    assert_eq!(listed("sess_feed_route_a"), (start + 4, 2));
    assert_eq!(listed("sess_feed_route_b"), (start + 2, 0));
    assert_eq!(list["data"]["feed_epoch"], epoch);

    let status: Value = http()
        .get(format!("{}/v1/sessions/-/status", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("status")
        .json()
        .await
        .expect("status json");
    let row = status["data"]["sessions"]
        .as_array()
        .expect("status sessions")
        .iter()
        .find(|session| session["id"] == "sess_feed_route_a")
        .expect("session in the status window");
    assert_eq!(row["change_seq"], start + 4);
    assert_eq!(row["event_seq"], 2);
    assert_eq!(status["data"]["feed_epoch"], epoch);
}

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
