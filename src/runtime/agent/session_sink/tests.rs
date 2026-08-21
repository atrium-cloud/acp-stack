#[test]
fn extract_usage_payload_picks_up_top_level_usage_object() {
    let payload =
        r#"{"usage": {"input_tokens": 12, "output_tokens": 34, "context_window_max": 200000}}"#;
    let usage = super::extract_usage_payload("sess_x", payload).expect("usage should be extracted");
    assert_eq!(usage["input_tokens"].as_i64(), Some(12));
    assert_eq!(usage["output_tokens"].as_i64(), Some(34));
    assert_eq!(usage["context_window_max"].as_i64(), Some(200000));
    assert_eq!(usage["session_id"].as_str(), Some("sess_x"));
}

#[test]
fn extract_usage_payload_walks_nested_paths() {
    let payload = r#"{"update": {"usage": {"input_tokens": 5}}}"#;
    let usage = super::extract_usage_payload("sess_y", payload).expect("usage should be extracted");
    assert_eq!(usage["input_tokens"].as_i64(), Some(5));
    // Output tokens absent — must NOT be serialized rather than written as 0.
    assert!(usage.get("output_tokens").is_none());
}

#[test]
fn extract_usage_payload_normalizes_standard_usage_update() {
    let payload = r#"{"sessionId":"agent_sess","update":{"sessionUpdate":"usage_update","used":4096,"size":32768,"cost":{"amount":1.25,"currency":"USD"}}}"#;
    let usage =
        super::extract_usage_payload("sess_local", payload).expect("standard usage extracted");
    assert_eq!(usage["session_id"], "sess_local");
    assert_eq!(usage["context_window_used"], 4096);
    assert_eq!(usage["context_window_max"], 32768);
    assert_eq!(usage["cost_amount"], 1.25);
    assert_eq!(usage["cost_currency"], "USD");
    assert!(usage.get("input_tokens").is_none());
}

#[test]
fn extract_usage_payload_returns_none_when_shape_unknown() {
    assert!(super::extract_usage_payload("sess_z", "{}").is_none());
    assert!(super::extract_usage_payload("sess_z", r#"{"update":{"foo":"bar"}}"#).is_none());
    assert!(super::extract_usage_payload("sess_z", "not-json").is_none());
}

#[test]
fn extract_usage_payload_rejects_negative_numbers() {
    let payload = r#"{"usage": {"input_tokens": -5, "output_tokens": 3}}"#;
    let usage = super::extract_usage_payload("s", payload).expect("partial usage");
    // Negative tokens were dropped; output tokens preserved.
    assert!(usage.get("input_tokens").is_none());
    assert_eq!(usage["output_tokens"].as_i64(), Some(3));
}

#[test]
fn extract_execute_tool_call_lifts_command_from_raw_input() {
    // Serialized shape of an ACP `tool_call` update from a built-in
    // shell tool (Claude Code / OpenCode bash convention).
    let payload = r#"{"sessionId":"sess_1","update":{"sessionUpdate":"tool_call","toolCallId":"call_1","title":"uname -a","kind":"execute","status":"in_progress","rawInput":{"command":"uname -a","description":"print kernel info"}}}"#;
    let event = super::extract_execute_tool_call("sess_local", payload)
        .expect("execute tool call extracted");
    assert_eq!(event["session_id"].as_str(), Some("sess_local"));
    assert_eq!(event["tool_call_id"].as_str(), Some("call_1"));
    assert_eq!(event["status"].as_str(), Some("in_progress"));
    assert_eq!(event["title"].as_str(), Some("uname -a"));
    assert_eq!(event["command"].as_str(), Some("uname -a"));
}

#[test]
fn extract_execute_tool_call_accepts_updates_that_restate_kind() {
    let payload = r#"{"sessionId":"sess_1","update":{"sessionUpdate":"tool_call_update","toolCallId":"call_1","kind":"execute","status":"completed"}}"#;
    let event = super::extract_execute_tool_call("sess_local", payload)
        .expect("execute tool call update extracted");
    assert_eq!(event["status"].as_str(), Some("completed"));
    // No rawInput on this transition: command absent, not empty.
    assert!(event.get("command").is_none());
}

#[test]
fn extract_execute_tool_call_ignores_other_updates() {
    // Non-execute tool kind.
    let read_call = r#"{"update":{"sessionUpdate":"tool_call","toolCallId":"call_2","kind":"read","status":"pending"}}"#;
    assert!(super::extract_execute_tool_call("s", read_call).is_none());
    // Update without a restated kind (ACP only requires kind on the
    // initial tool_call) must not fire.
    let bare_update = r#"{"update":{"sessionUpdate":"tool_call_update","toolCallId":"call_1","status":"completed"}}"#;
    assert!(super::extract_execute_tool_call("s", bare_update).is_none());
    // Non-tool-call updates and garbage.
    let chunk = r#"{"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}}}"#;
    assert!(super::extract_execute_tool_call("s", chunk).is_none());
    assert!(super::extract_execute_tool_call("s", "not-json").is_none());
}

use crate::state::{NewPromptRecord, NewSessionRecord, PromptStatus, StateStore};
use rusqlite::params;

#[test]
fn touch_running_prompt_advances_updated_at_on_in_flight_row() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state open");
    store.migrate().expect("migrate");
    store
        .insert_session(NewSessionRecord {
            id: "sess_touch".to_owned(),
            agent_id: "fake".to_owned(),
            cwd: "/tmp".to_owned(),
            title: None,
            metadata_json: "{}".to_owned(),
        })
        .expect("session inserted");
    store
        .insert_prompt(NewPromptRecord {
            id: "prm_touch".to_owned(),
            session_id: "sess_touch".to_owned(),
            prompt_json: "[]".to_owned(),
        })
        .expect("prompt inserted");
    store
        .update_prompt_status(
            "prm_touch",
            PromptStatus::Running,
            None,
            None,
            None,
            None,
            None,
        )
        .expect("prompt flipped to running");

    // Force `updated_at` into the past so the re-touch is visible
    // even at sub-second resolution. Without this the wall-clock
    // delta between insert and touch is too small for the string
    // comparison to be reliable.
    let aged = "2020-01-01T00:00:00.000000000Z";
    let connection =
        rusqlite::Connection::open(store.path()).expect("open sqlite for age override");
    connection
        .execute(
            "UPDATE prompts SET updated_at = ?1 WHERE id = ?2",
            params![aged, "prm_touch"],
        )
        .expect("force-set updated_at");
    drop(connection);

    super::touch_running_prompt(&store, "sess_touch").expect("re-touch should succeed");

    let prompt = store
        .get_prompt("prm_touch")
        .expect("prompt lookup")
        .expect("prompt exists");
    assert_ne!(
        prompt.updated_at, aged,
        "touch_running_prompt must advance updated_at"
    );
    assert_eq!(
        prompt.status, "running",
        "touch must preserve the running status"
    );
}

#[tokio::test]
async fn writer_persists_derived_tool_execute_event() {
    use crate::runtime::agent::session_sink::{SessionEventSink, StateStoreSessionSink};
    use std::sync::Arc;
    use tokio::sync::Mutex as TokioMutex;

    let tempdir = tempfile::tempdir().expect("tempdir");
    let store = StateStore::open(tempdir.path().join("state.sqlite")).expect("state open");
    store.migrate().expect("migrate");
    store
        .insert_session_for_target(
            "target_a",
            "agent_sess_1".to_owned(),
            NewSessionRecord {
                id: "sess_local".to_owned(),
                agent_id: "target_a".to_owned(),
                cwd: "/tmp".to_owned(),
                title: None,
                metadata_json: "{}".to_owned(),
            },
        )
        .expect("session inserted");
    let state = Arc::new(TokioMutex::new(store));

    let sink = StateStoreSessionSink::new("target_a".to_owned(), state.clone());
    let payload = r#"{"sessionId":"agent_sess_1","update":{"sessionUpdate":"tool_call","toolCallId":"call_1","title":"uname -a","kind":"execute","status":"in_progress","rawInput":{"command":"uname -a"}}}"#;
    sink.append("agent_sess_1", "session.update", payload).await;
    sink.flush().await;

    let guard = state.lock().await;
    let derived = guard
        .query_events(crate::state::LogFilter {
            limit: 10,
            kind: Some("tool.execute"),
            source: Some("acp"),
            ..Default::default()
        })
        .expect("query derived events");
    assert_eq!(derived.len(), 1, "expected one derived tool.execute event");
    assert!(derived[0].payload_json.contains("\"command\":\"uname -a\""));
    assert!(
        derived[0]
            .payload_json
            .contains("\"session_id\":\"sess_local\"")
    );
    // The verbatim session.update row is still written alongside.
    let verbatim = guard
        .query_events(crate::state::LogFilter {
            limit: 10,
            kind: Some("session.update"),
            source: Some("acp"),
            ..Default::default()
        })
        .expect("query verbatim events");
    assert_eq!(verbatim.len(), 1, "expected the verbatim session.update");
}

#[tokio::test]
async fn writer_persists_normalized_standard_usage_event() {
    use crate::runtime::agent::session_sink::{SessionEventSink, StateStoreSessionSink};
    use std::sync::Arc;
    use tokio::sync::Mutex as TokioMutex;

    let tempdir = tempfile::tempdir().expect("tempdir");
    let store = StateStore::open(tempdir.path().join("state.sqlite")).expect("state open");
    store.migrate().expect("migrate");
    store
        .insert_session_for_target(
            "target_a",
            "agent_sess_1".to_owned(),
            NewSessionRecord {
                id: "sess_local".to_owned(),
                agent_id: "target_a".to_owned(),
                cwd: "/tmp".to_owned(),
                title: None,
                metadata_json: "{}".to_owned(),
            },
        )
        .expect("session inserted");
    let state = Arc::new(TokioMutex::new(store));
    let sink = StateStoreSessionSink::new("target_a".to_owned(), state.clone());
    let payload = r#"{"sessionId":"agent_sess_1","update":{"sessionUpdate":"usage_update","used":2048,"size":8192,"cost":{"amount":0.75,"currency":"EUR"}}}"#;
    sink.append("agent_sess_1", "session.update", payload).await;
    sink.flush().await;

    let guard = state.lock().await;
    let events = guard
        .query_events(crate::state::LogFilter {
            limit: 10,
            kind: Some("usage.reported"),
            source: Some("acp"),
            ..Default::default()
        })
        .expect("query usage events");
    assert_eq!(events.len(), 1);
    let usage: serde_json::Value =
        serde_json::from_str(&events[0].payload_json).expect("usage JSON");
    assert_eq!(usage["context_window_used"], 2048);
    assert_eq!(usage["context_window_max"], 8192);
    assert_eq!(usage["cost_amount"], 0.75);
    assert_eq!(usage["cost_currency"], "EUR");
}

#[test]
fn session_info_updates_patch_title_and_agent_metadata() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let store = StateStore::open(tempdir.path().join("state.sqlite")).expect("state open");
    store.migrate().expect("migrate");
    store
        .insert_session_for_target(
            "target_a",
            "agent_sess_1".to_owned(),
            NewSessionRecord {
                id: "sess_local".to_owned(),
                agent_id: "target_a".to_owned(),
                cwd: "/tmp".to_owned(),
                title: Some("original".to_owned()),
                metadata_json: r#"{"preserved":true,"agent_meta":{"old":true}}"#.to_owned(),
            },
        )
        .expect("session inserted");
    let metadata_update = serde_json::json!({
        "sessionId": "agent_sess_1",
        "update": {
            "sessionUpdate": "session_info_update",
            "updatedAt": "2026-07-20T01:02:03Z",
            "_meta": {"origin": "agent"}
        }
    });
    super::project_session_info_update(&store, "sess_local", &metadata_update.to_string())
        .expect("metadata projection");
    {
        let session = store
            .get_session("sess_local")
            .expect("session lookup")
            .expect("session exists");
        assert_eq!(session.title.as_deref(), Some("original"));
        assert_ne!(session.updated_at, "2026-07-20T01:02:03Z");
        let metadata: serde_json::Value =
            serde_json::from_str(&session.metadata_json).expect("metadata JSON");
        assert_eq!(metadata["preserved"], true);
        assert_eq!(metadata["agent_updated_at"], "2026-07-20T01:02:03Z");
        assert_eq!(metadata["agent_meta"]["origin"], "agent");
    }

    let clear_update = serde_json::json!({
        "sessionId": "agent_sess_1",
        "update": {
            "sessionUpdate": "session_info_update",
            "title": null,
            "updatedAt": null
        }
    });
    super::project_session_info_update(&store, "sess_local", &clear_update.to_string())
        .expect("clear projection");
    {
        let session = store
            .get_session("sess_local")
            .expect("session lookup")
            .expect("session exists");
        assert_eq!(session.title, None);
        let metadata: serde_json::Value =
            serde_json::from_str(&session.metadata_json).expect("metadata JSON");
        assert!(metadata["agent_updated_at"].is_null());
        assert_eq!(metadata["agent_meta"]["origin"], "agent");
        assert_eq!(metadata["preserved"], true);
    }

    let title_update = serde_json::json!({
        "sessionId": "agent_sess_1",
        "update": {
            "sessionUpdate": "session_info_update",
            "title": "renamed"
        }
    });
    super::project_session_info_update(&store, "sess_local", &title_update.to_string())
        .expect("title projection");
    let session = store
        .get_session("sess_local")
        .expect("session lookup")
        .expect("session exists");
    assert_eq!(session.title.as_deref(), Some("renamed"));
    let metadata: serde_json::Value =
        serde_json::from_str(&session.metadata_json).expect("metadata JSON");
    assert!(metadata["agent_updated_at"].is_null());
    assert_eq!(metadata["agent_meta"]["origin"], "agent");
    assert_eq!(metadata["preserved"], true);
}

#[test]
fn available_commands_updates_replace_stored_list() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let store = StateStore::open(tempdir.path().join("state.sqlite")).expect("state open");
    store.migrate().expect("migrate");
    store
        .insert_session_for_target(
            "target_a",
            "agent_sess_1".to_owned(),
            NewSessionRecord {
                id: "sess_local".to_owned(),
                agent_id: "target_a".to_owned(),
                cwd: "/tmp".to_owned(),
                title: None,
                metadata_json: r#"{"preserved":true,"agent_meta":{"old":true}}"#.to_owned(),
            },
        )
        .expect("session inserted");

    let advertise = serde_json::json!({
        "sessionId": "agent_sess_1",
        "update": {
            "sessionUpdate": "available_commands_update",
            "availableCommands": [
                {
                    "name": "compact",
                    "description": "Summarize the conversation",
                    "input": {"hint": "optional instructions"},
                    "_meta": {"opaque": true}
                },
                {"name": "init", "description": "Create AGENTS.md"}
            ]
        }
    });
    super::project_available_commands_update(&store, "sess_local", &advertise.to_string())
        .expect("commands projection");
    {
        let session = store
            .get_session("sess_local")
            .expect("session lookup")
            .expect("session exists");
        let metadata: serde_json::Value =
            serde_json::from_str(&session.metadata_json).expect("metadata JSON");
        let commands = metadata["available_commands"]
            .as_array()
            .expect("commands array");
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0]["name"], "compact");
        assert_eq!(commands[0]["description"], "Summarize the conversation");
        assert_eq!(commands[0]["input_hint"], "optional instructions");
        // `_meta` is dropped by the compact projection.
        assert!(commands[0].get("_meta").is_none());
        // No input spec means no hint key at all.
        assert_eq!(commands[1]["name"], "init");
        assert!(commands[1].get("input_hint").is_none());
        assert!(metadata["available_commands_updated_at"].is_string());
        // Unrelated metadata keys survive the write.
        assert_eq!(metadata["preserved"], true);
        assert_eq!(metadata["agent_meta"]["old"], true);
    }

    // Latest-wins: a second update replaces rather than merges.
    let replace = serde_json::json!({
        "sessionId": "agent_sess_1",
        "update": {
            "sessionUpdate": "available_commands_update",
            "availableCommands": [
                {"name": "review", "description": "Review changes"}
            ]
        }
    });
    super::project_available_commands_update(&store, "sess_local", &replace.to_string())
        .expect("replace projection");
    {
        let session = store
            .get_session("sess_local")
            .expect("session lookup")
            .expect("session exists");
        let metadata: serde_json::Value =
            serde_json::from_str(&session.metadata_json).expect("metadata JSON");
        let commands = metadata["available_commands"]
            .as_array()
            .expect("commands array");
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0]["name"], "review");
    }

    // An empty list is a legitimate advertisement and clears the stored list.
    let clear = serde_json::json!({
        "sessionId": "agent_sess_1",
        "update": {
            "sessionUpdate": "available_commands_update",
            "availableCommands": []
        }
    });
    super::project_available_commands_update(&store, "sess_local", &clear.to_string())
        .expect("clear projection");
    let session = store
        .get_session("sess_local")
        .expect("session lookup")
        .expect("session exists");
    let metadata: serde_json::Value =
        serde_json::from_str(&session.metadata_json).expect("metadata JSON");
    assert_eq!(
        metadata["available_commands"]
            .as_array()
            .expect("commands array")
            .len(),
        0
    );
}

#[test]
fn available_commands_projection_ignores_other_updates_and_truncates_over_cap() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let store = StateStore::open(tempdir.path().join("state.sqlite")).expect("state open");
    store.migrate().expect("migrate");
    store
        .insert_session_for_target(
            "target_a",
            "agent_sess_1".to_owned(),
            NewSessionRecord {
                id: "sess_local".to_owned(),
                agent_id: "target_a".to_owned(),
                cwd: "/tmp".to_owned(),
                title: None,
                metadata_json: "{}".to_owned(),
            },
        )
        .expect("session inserted");

    // Non-matching payloads are a no-op, not an error.
    let chunk = r#"{"sessionId":"agent_sess_1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}}}"#;
    super::project_available_commands_update(&store, "sess_local", chunk).expect("chunk no-op");
    let bare = r#"{"sessionId":"agent_sess_1"}"#;
    super::project_available_commands_update(&store, "sess_local", bare).expect("bare no-op");
    {
        let session = store
            .get_session("sess_local")
            .expect("session lookup")
            .expect("session exists");
        let metadata: serde_json::Value =
            serde_json::from_str(&session.metadata_json).expect("metadata JSON");
        assert!(metadata.get("available_commands").is_none());
    }

    let commands: Vec<serde_json::Value> = (0..crate::state::MAX_SESSION_AVAILABLE_COMMANDS + 5)
        .map(|index| serde_json::json!({"name": format!("cmd{index}"), "description": ""}))
        .collect();
    let oversized = serde_json::json!({
        "sessionId": "agent_sess_1",
        "update": {
            "sessionUpdate": "available_commands_update",
            "availableCommands": commands
        }
    });
    super::project_available_commands_update(&store, "sess_local", &oversized.to_string())
        .expect("oversized projection");
    let session = store
        .get_session("sess_local")
        .expect("session lookup")
        .expect("session exists");
    let metadata: serde_json::Value =
        serde_json::from_str(&session.metadata_json).expect("metadata JSON");
    assert_eq!(
        metadata["available_commands"]
            .as_array()
            .expect("commands array")
            .len(),
        crate::state::MAX_SESSION_AVAILABLE_COMMANDS
    );
}

#[tokio::test]
async fn writer_projects_available_commands_and_keeps_raw_event() {
    use crate::runtime::agent::session_sink::{SessionEventSink, StateStoreSessionSink};
    use std::sync::Arc;
    use tokio::sync::Mutex as TokioMutex;

    let tempdir = tempfile::tempdir().expect("tempdir");
    let store = StateStore::open(tempdir.path().join("state.sqlite")).expect("state open");
    store.migrate().expect("migrate");
    store
        .insert_session_for_target(
            "target_a",
            "agent_sess_1".to_owned(),
            NewSessionRecord {
                id: "sess_local".to_owned(),
                agent_id: "target_a".to_owned(),
                cwd: "/tmp".to_owned(),
                title: None,
                metadata_json: "{}".to_owned(),
            },
        )
        .expect("session inserted");
    let state = Arc::new(TokioMutex::new(store));
    let sink = StateStoreSessionSink::new("target_a".to_owned(), state.clone());
    let payload = r#"{"sessionId":"agent_sess_1","update":{"sessionUpdate":"available_commands_update","availableCommands":[{"name":"compact","description":"Summarize","input":{"hint":"optional"}}]}}"#;

    sink.append("agent_sess_1", "session.update", payload).await;
    sink.flush().await;

    let guard = state.lock().await;
    let events = guard
        .query_events(crate::state::LogFilter {
            limit: 10,
            kind: Some("session.update"),
            source: Some("acp"),
            ..Default::default()
        })
        .expect("query raw events");
    assert_eq!(events.len(), 1);
    let session = guard
        .get_session("sess_local")
        .expect("session lookup")
        .expect("session exists");
    let metadata: serde_json::Value =
        serde_json::from_str(&session.metadata_json).expect("metadata JSON");
    let commands = metadata["available_commands"]
        .as_array()
        .expect("commands array");
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0]["name"], "compact");
    assert_eq!(commands[0]["input_hint"], "optional");
}

#[tokio::test]
async fn writer_keeps_raw_session_info_when_projection_fails() {
    use crate::runtime::agent::session_sink::{SessionEventSink, StateStoreSessionSink};
    use std::sync::Arc;
    use tokio::sync::Mutex as TokioMutex;

    let tempdir = tempfile::tempdir().expect("tempdir");
    let store = StateStore::open(tempdir.path().join("state.sqlite")).expect("state open");
    store.migrate().expect("migrate");
    store
        .insert_session_for_target(
            "target_a",
            "agent_sess_1".to_owned(),
            NewSessionRecord {
                id: "sess_local".to_owned(),
                agent_id: "target_a".to_owned(),
                cwd: "/tmp".to_owned(),
                title: Some("original".to_owned()),
                metadata_json: "[]".to_owned(),
            },
        )
        .expect("session inserted");
    let state = Arc::new(TokioMutex::new(store));
    let sink = StateStoreSessionSink::new("target_a".to_owned(), state.clone());
    let payload = r#"{"sessionId":"agent_sess_1","update":{"sessionUpdate":"session_info_update","title":"renamed"}}"#;

    sink.append("agent_sess_1", "session.update", payload).await;
    sink.flush().await;

    let guard = state.lock().await;
    let events = guard
        .query_events(crate::state::LogFilter {
            limit: 10,
            kind: Some("session.update"),
            source: Some("acp"),
            ..Default::default()
        })
        .expect("query raw events");
    assert_eq!(events.len(), 1);
    let session = guard
        .get_session("sess_local")
        .expect("session lookup")
        .expect("session exists");
    assert_eq!(session.title.as_deref(), Some("original"));
}

#[tokio::test]
async fn session_cwd_resolves_local_session_record() {
    use crate::runtime::agent::session_sink::{SessionEventSink, StateStoreSessionSink};
    use std::sync::Arc;
    use tokio::sync::Mutex as TokioMutex;

    let tempdir = tempfile::tempdir().expect("tempdir");
    let store = StateStore::open(tempdir.path().join("state.sqlite")).expect("state open");
    store.migrate().expect("migrate");
    store
        .insert_session_for_target(
            "target_a",
            "agent_sess_1".to_owned(),
            NewSessionRecord {
                id: "sess_local".to_owned(),
                agent_id: "target_a".to_owned(),
                cwd: "/tmp/session-sub".to_owned(),
                title: None,
                metadata_json: "{}".to_owned(),
            },
        )
        .expect("session inserted");
    let state = Arc::new(TokioMutex::new(store));
    let sink = StateStoreSessionSink::new("target_a".to_owned(), state);

    assert_eq!(
        sink.session_cwd("agent_sess_1").await,
        Some("/tmp/session-sub".to_owned())
    );
    assert_eq!(sink.session_cwd("agent_sess_unknown").await, None);
}

#[tokio::test]
async fn change_capture_maps_same_agent_session_id_to_each_array_target() {
    use crate::runtime::agent::session_changes::SessionChangesHandle;
    use crate::runtime::agent::session_sink::{SessionEventSink, StateStoreSessionSink};
    use agent_client_protocol::schema::v1::{
        Diff, SessionUpdate, ToolCall, ToolCallContent, ToolKind,
    };
    use std::sync::Arc;
    use tokio::sync::Mutex as TokioMutex;

    let tempdir = tempfile::tempdir().expect("tempdir");
    let store = StateStore::open(tempdir.path().join("state.sqlite")).expect("state open");
    store.migrate().expect("migrate");
    for (target_id, local_id) in [("target_a", "sess_local_a"), ("target_b", "sess_local_b")] {
        store
            .insert_session_for_target(
                target_id,
                "shared_agent_session".to_owned(),
                NewSessionRecord {
                    id: local_id.to_owned(),
                    agent_id: target_id.to_owned(),
                    cwd: "/tmp".to_owned(),
                    title: None,
                    metadata_json: "{}".to_owned(),
                },
            )
            .expect("session inserted");
    }
    let state = Arc::new(TokioMutex::new(store));
    let changes = SessionChangesHandle::new();
    let sink_a = StateStoreSessionSink::with_session_changes(
        "target_a".to_owned(),
        state.clone(),
        changes.clone(),
    );
    let sink_b =
        StateStoreSessionSink::with_session_changes("target_b".to_owned(), state, changes.clone());
    let update_for = |new_text: &str| {
        SessionUpdate::ToolCall(
            ToolCall::new("call", "edit file")
                .kind(ToolKind::Edit)
                .content(vec![ToolCallContent::Diff(
                    Diff::new("/workspace/file", new_text).old_text("before"),
                )]),
        )
    };

    assert!(
        sink_a
            .capture_session_update("shared_agent_session", &update_for("target a"))
            .await
    );
    assert!(
        sink_b
            .capture_session_update("shared_agent_session", &update_for("target b"))
            .await
    );

    let snapshot_a =
        serde_json::to_value(changes.snapshot("sess_local_a").await).expect("snapshot a JSON");
    let snapshot_b =
        serde_json::to_value(changes.snapshot("sess_local_b").await).expect("snapshot b JSON");
    assert_eq!(
        snapshot_a["tool_calls"][0]["content"][0]["newText"],
        "target a"
    );
    assert_eq!(
        snapshot_b["tool_calls"][0]["content"][0]["newText"],
        "target b"
    );
}

#[tokio::test]
async fn change_capture_accepts_non_tool_updates_without_a_session_lookup() {
    use crate::runtime::agent::session_sink::{SessionEventSink, StateStoreSessionSink};
    use agent_client_protocol::schema::v1::{
        ContentBlock, ContentChunk, SessionUpdate, TextContent,
    };
    use std::sync::Arc;
    use tokio::sync::Mutex as TokioMutex;

    let tempdir = tempfile::tempdir().expect("tempdir");
    let store = StateStore::open(tempdir.path().join("state.sqlite")).expect("state open");
    store.migrate().expect("migrate");
    let sink = StateStoreSessionSink::new("target".to_owned(), Arc::new(TokioMutex::new(store)));

    let chunk = SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
        TextContent::new("hello"),
    )));
    assert!(
        sink.capture_session_update("agent_sess_unknown", &chunk)
            .await
    );
}

#[test]
fn touch_running_prompt_is_noop_when_no_in_flight_prompt() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state open");
    store.migrate().expect("migrate");
    store
        .insert_session(NewSessionRecord {
            id: "sess_empty".to_owned(),
            agent_id: "fake".to_owned(),
            cwd: "/tmp".to_owned(),
            title: None,
            metadata_json: "{}".to_owned(),
        })
        .expect("session inserted");

    // No prompt rows — re-touch must succeed without an error so the
    // ACP session sink never blocks on a benign no-op.
    super::touch_running_prompt(&store, "sess_empty").expect("noop succeeds");
}
