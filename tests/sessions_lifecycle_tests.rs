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
use common::HomeEnvGuard;
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

    // The prompt itself may fail (the prior session id died with the process);
    // what matters is that the request brought the agent back.
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
    let _home_guard = HomeEnvGuard::set(home.path());
    SecretStore::open_or_create(home.path()).expect("secret store initializes");

    let harness = Harness::spawn_with(|config| {
        config.mcp.servers = declared_mcp_servers();
    })
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
    let _home_guard = HomeEnvGuard::set(home.path());
    SecretStore::open_or_create(home.path()).expect("secret store initializes");

    let harness = Harness::spawn_with(|config| {
        config.agent.args.push("--cap-mcp-http".to_owned());
        config.mcp.servers = declared_mcp_servers();
    })
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
