use super::*;
use crate::state::MetricsWindow;

const SUBAGENT_UPDATE_PAYLOAD: &str = r#"{"sessionId":"agent_root","update":{"sessionUpdate":"subagent_update","sessionId":"agent_child"}}"#;
const RUNNING_STATE: &str = r#"{"state":"running"}"#;

fn store_with_root(dir: &tempfile::TempDir) -> StateStore {
    let store = StateStore::open(dir.path().join("state.sqlite")).expect("open state");
    store.migrate().expect("migrate");
    insert_target_session(&store, "sess_root", "agent_root");
    store
}

fn insert_target_session(store: &StateStore, id: &str, agent_session_id: &str) {
    store
        .insert_session_for_target(
            "target_a",
            agent_session_id.to_owned(),
            NewSessionRecord {
                id: id.to_owned(),
                agent_id: "target_a".to_owned(),
                cwd: "/work".to_owned(),
                title: None,
                metadata_json: "{}".to_owned(),
            },
        )
        .expect("session inserted");
}

fn patch(agent_session_id: &str) -> SubagentPatch {
    SubagentPatch {
        agent_session_id: agent_session_id.to_owned(),
        title: FieldPatch::Keep,
        description: FieldPatch::Keep,
        state_json: FieldPatch::Keep,
        cancel_granted: None,
    }
}

fn announce(store: &StateStore, parent_session_id: &str, agent_session_id: &str) -> String {
    let applied = store
        .record_subagent_update(
            parent_session_id,
            SUBAGENT_UPDATE_PAYLOAD,
            SubagentPatch {
                state_json: FieldPatch::Set(RUNNING_STATE.to_owned()),
                cancel_granted: Some(true),
                ..patch(agent_session_id)
            },
        )
        .expect("announcement recorded");
    match applied {
        SubagentApplied::Announced {
            child_session_id, ..
        } => child_session_id,
        other => panic!("expected an announcement, got {other:?}"),
    }
}

fn event_kinds(store: &StateStore, session_id: &str) -> Vec<String> {
    store
        .query_session_events(session_id, None, 100)
        .expect("session events")
        .into_iter()
        .map(|event| event.kind)
        .collect()
}

fn running_prompt(store: &StateStore, session_id: &str, prompt_id: &str) {
    store
        .insert_prompt(NewPromptRecord {
            id: prompt_id.to_owned(),
            session_id: session_id.to_owned(),
            prompt_json: "[]".to_owned(),
        })
        .expect("prompt inserted");
    store
        .update_prompt_status(
            prompt_id,
            PromptStatus::Running,
            None,
            None,
            None,
            None,
            None,
        )
        .expect("prompt running");
}

#[test]
fn an_announcement_creates_the_child_and_later_patches_merge_onto_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = store_with_root(&dir);

    let applied = store
        .record_subagent_update(
            "sess_root",
            SUBAGENT_UPDATE_PAYLOAD,
            SubagentPatch {
                title: FieldPatch::Set("Explore".to_owned()),
                description: FieldPatch::Set("Map the repository".to_owned()),
                state_json: FieldPatch::Set(RUNNING_STATE.to_owned()),
                cancel_granted: Some(true),
                ..patch("agent_child")
            },
        )
        .expect("announcement recorded");
    let SubagentApplied::Announced {
        child_session_id,
        root_session_id,
    } = applied
    else {
        panic!("expected an announcement, got {applied:?}");
    };
    assert_eq!(root_session_id, "sess_root");
    let child = store
        .get_session(&child_session_id)
        .expect("child lookup")
        .expect("child row");
    assert_eq!(
        (
            child.target_id.as_str(),
            child.agent_session_id.as_str(),
            child.agent_id.as_str(),
            child.cwd.as_str(),
            child.status.as_str(),
        ),
        (
            "target_a",
            "agent_child",
            "target_a",
            "/work",
            SESSION_STATUS_AVAILABLE
        )
    );
    assert_eq!(
        event_kinds(&store, "sess_root"),
        [
            EVENT_KIND_SESSION_UPDATE,
            EVENT_KIND_SESSION_SUBAGENT_ANNOUNCED
        ]
    );

    let idle = r#"{"state":"idle","stopReason":"end_turn"}"#;
    let applied = store
        .record_subagent_update(
            "sess_root",
            SUBAGENT_UPDATE_PAYLOAD,
            SubagentPatch {
                description: FieldPatch::Clear,
                state_json: FieldPatch::Set(idle.to_owned()),
                cancel_granted: Some(false),
                ..patch("agent_child")
            },
        )
        .expect("patch recorded");
    assert_eq!(
        applied,
        SubagentApplied::Updated {
            child_session_id: child_session_id.clone()
        }
    );
    let record = store
        .get_subagent(&child_session_id)
        .expect("subagent lookup")
        .expect("subagent row");
    assert_eq!(record.parent_session_id, "sess_root");
    assert_eq!(record.title.as_deref(), Some("Explore"));
    assert_eq!(record.description, None);
    assert_eq!(record.state_json.as_deref(), Some(idle));
    assert!(record.state_current);
    assert!(!record.cancel_granted);
    let patched = store
        .get_session(&child_session_id)
        .expect("child lookup")
        .expect("child row");
    assert!(patched.change_seq > child.change_seq);
    assert_eq!(
        event_kinds(&store, "sess_root"),
        [
            EVENT_KIND_SESSION_UPDATE,
            EVENT_KIND_SESSION_SUBAGENT_ANNOUNCED,
            EVENT_KIND_SESSION_UPDATE
        ]
    );
}

#[test]
fn a_nested_child_belongs_to_the_root_of_its_parents_tree() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = store_with_root(&dir);
    let child = announce(&store, "sess_root", "agent_child");

    let applied = store
        .record_subagent_update(&child, SUBAGENT_UPDATE_PAYLOAD, patch("agent_grandchild"))
        .expect("nested announcement recorded");

    let SubagentApplied::Announced {
        child_session_id,
        root_session_id,
    } = applied
    else {
        panic!("expected an announcement, got {applied:?}");
    };
    assert_eq!(root_session_id, "sess_root");
    let record = store
        .get_subagent(&child_session_id)
        .expect("subagent lookup")
        .expect("subagent row");
    assert_eq!(record.parent_session_id, child);
    assert_eq!(record.root_session_id, "sess_root");
}

#[test]
fn patches_that_contradict_stored_sessions_are_rejected_and_still_logged() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = store_with_root(&dir);
    let child = announce(&store, "sess_root", "agent_child");
    announce(&store, &child, "agent_grandchild");
    insert_target_session(&store, "sess_prompted", "agent_prompted");
    running_prompt(&store, "sess_prompted", "prm_prompted");
    insert_target_session(&store, "sess_loaded", "agent_loaded");
    announce(&store, "sess_loaded", "agent_loaded_child");
    let logged_before = event_kinds(&store, "sess_root").len() + event_kinds(&store, &child).len();

    for (parent, agent_session_id, rejection) in [
        ("sess_root", "agent_root", SubagentRejection::SelfParent),
        (child.as_str(), "agent_root", SubagentRejection::Ancestor),
        ("sess_root", "agent_grandchild", SubagentRejection::Reparent),
        ("sess_root", "agent_prompted", SubagentRejection::Prompted),
        ("sess_root", "agent_loaded", SubagentRejection::HeadsTree),
    ] {
        assert_eq!(
            store
                .record_subagent_update(parent, SUBAGENT_UPDATE_PAYLOAD, patch(agent_session_id))
                .expect("update recorded"),
            SubagentApplied::Rejected(rejection),
        );
    }

    assert_eq!(
        event_kinds(&store, "sess_root").len() + event_kinds(&store, &child).len(),
        logged_before + 5
    );
    assert_eq!(store.get_subagent("sess_loaded").expect("lookup"), None);
    assert_eq!(store.get_subagent("sess_root").expect("lookup"), None);
    assert_eq!(store.get_subagent("sess_prompted").expect("lookup"), None);

    insert_target_session(&store, "sess_listed", "agent_listed");
    assert_eq!(announce(&store, "sess_root", "agent_listed"), "sess_listed");
}

#[test]
fn subagent_traffic_keeps_the_roots_turn_alive() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = store_with_root(&dir);
    running_prompt(&store, "sess_root", "prm_root");
    let child = announce(&store, "sess_root", "agent_child");
    // Aged so the touch is visible against the string comparison.
    store
        .connection()
        .execute(
            "UPDATE prompts SET updated_at = '2020-01-01T00:00:00.000000000Z' WHERE id = 'prm_root'",
            [],
        )
        .expect("age prompt");

    let event = store
        .append_session_update(&child, EVENT_KIND_SESSION_UPDATE, SUBAGENT_UPDATE_PAYLOAD)
        .expect("child update appended");

    let prompt = store
        .get_prompt("prm_root")
        .expect("prompt lookup")
        .expect("prompt row");
    assert_eq!(prompt.updated_at, event.created_at);
}

#[test]
fn a_state_patch_keeps_its_child_in_the_status_window() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = store_with_root(&dir);
    let child = announce(&store, "sess_root", "agent_child");
    // Aged so the window opens after the announcement and before the patch.
    store
        .connection()
        .execute(
            "UPDATE sessions SET updated_at = '2020-01-01T00:00:00.000000000Z' WHERE id = ?1",
            params![child],
        )
        .expect("age child");
    let since = "2021-01-01T00:00:00.000000000Z";

    store
        .record_subagent_update(
            "sess_root",
            SUBAGENT_UPDATE_PAYLOAD,
            SubagentPatch {
                state_json: FieldPatch::Set(r#"{"state":"requires_action"}"#.to_owned()),
                ..patch("agent_child")
            },
        )
        .expect("patch recorded");

    let window = store
        .query_session_status_window(since, None, Some("sess_root"), 10)
        .expect("status window");
    assert_eq!(
        window
            .iter()
            .map(|record| record.id.as_str())
            .collect::<Vec<_>>(),
        [child.as_str()]
    );
}

#[test]
fn reconcile_withdraws_the_live_state_and_cancel_grant_of_its_targets_subagents() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = store_with_root(&dir);
    let child = announce(&store, "sess_root", "agent_child");

    assert!(
        store
            .reconcile_orphaned_sessions(Some("target_b"))
            .expect("reconcile another target")
            .is_empty()
    );
    let untouched = store
        .get_subagent(&child)
        .expect("lookup")
        .expect("subagent row");
    assert!(untouched.state_current && untouched.cancel_granted);

    assert_eq!(
        store
            .reconcile_orphaned_sessions(Some("target_a"))
            .expect("reconcile"),
        ["sess_root"]
    );
    let reset = store
        .get_subagent(&child)
        .expect("lookup")
        .expect("subagent row");
    assert!(!reset.state_current && !reset.cancel_granted);
    assert_eq!(reset.state_json.as_deref(), Some(RUNNING_STATE));
}

#[test]
fn metrics_count_root_sessions_and_root_usage_only() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = store_with_root(&dir);
    let child = announce(&store, "sess_root", "agent_child");
    for (session_id, input_tokens) in [("sess_root", 10), (child.as_str(), 4)] {
        store
            .update_session_status(session_id, SESSION_STATUS_CLOSED)
            .expect("close");
        store
            .append_session_event_with_source(
                session_id,
                "info",
                "usage.reported",
                EVENT_SOURCE_ACP,
                "agent usage reported",
                &serde_json::json!({ "input_tokens": input_tokens }).to_string(),
            )
            .expect("usage event");
    }

    let summary = store
        .metrics_summary(MetricsWindow {
            since: "2000-01-01T00:00:00.000000000Z".to_owned(),
            until: "2100-01-01T00:00:00.000000000Z".to_owned(),
        })
        .expect("metrics");

    assert_eq!(summary.sessions.closed, 1);
    assert_eq!(summary.usage.tokens_input, Some(10));
}
