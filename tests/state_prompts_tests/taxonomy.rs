use acp_stack::state::{FailureClass, NewPromptRecord, NewSessionRecord, PromptStatus, StateStore};
use std::str::FromStr;

#[test]
fn prompt_status_helpers_round_trip_stalled() {
    assert_eq!(PromptStatus::Stalled.as_str(), "stalled");
    assert_eq!(
        PromptStatus::from_str("stalled").expect("stalled should parse"),
        PromptStatus::Stalled,
    );
    assert!(PromptStatus::Stalled.terminal());
    assert!(PromptStatus::Completed.terminal());
    assert!(PromptStatus::Errored.terminal());
    assert!(PromptStatus::Cancelled.terminal());
    assert!(!PromptStatus::Pending.terminal());
    assert!(!PromptStatus::Running.terminal());
    assert!(PromptStatus::from_str("not_a_status").is_err());
}

#[test]
fn failure_class_round_trips_taxonomy_strings() {
    let pairs = [
        (FailureClass::AgentRequest, "agent_request"),
        (FailureClass::Inference5xx, "inference_5xx"),
        (FailureClass::Inference4xx, "inference_4xx"),
        (FailureClass::Vm, "vm"),
        (FailureClass::Sqlite, "sqlite"),
        (FailureClass::Daemon, "daemon"),
        (FailureClass::AgentProcess, "agent_process"),
        (FailureClass::Stalled, "stalled"),
    ];
    for (variant, expected) in pairs {
        assert_eq!(variant.as_str(), expected);
        assert_eq!(
            FailureClass::from_str(expected).expect("taxonomy should parse"),
            variant,
        );
    }
    assert!(FailureClass::from_str("unknown").is_err());
}

#[test]
fn prompt_update_persists_stalled_with_failure_class_and_detail() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    store
        .insert_session(NewSessionRecord {
            id: "sess_stalled".to_owned(),
            agent_id: "fake".to_owned(),
            cwd: "/tmp/stalled".to_owned(),
            title: None,
            metadata_json: "{}".to_owned(),
        })
        .expect("session inserted");
    store
        .insert_prompt(NewPromptRecord {
            id: "prm_stalled".to_owned(),
            session_id: "sess_stalled".to_owned(),
            prompt_json: "[]".to_owned(),
        })
        .expect("prompt inserted");

    let detail = r#"{"reason":"threshold_exceeded"}"#;
    store
        .update_prompt_status(
            "prm_stalled",
            PromptStatus::Stalled,
            None,
            None,
            None,
            Some(FailureClass::Stalled.as_str()),
            Some(detail),
        )
        .expect("prompt status updated to stalled");

    let prompt = store
        .get_prompt("prm_stalled")
        .expect("prompt lookup")
        .expect("prompt exists");
    assert_eq!(prompt.status, PromptStatus::Stalled.as_str());
    assert_eq!(prompt.failure_class.as_deref(), Some("stalled"));
    assert_eq!(prompt.failure_detail_json.as_deref(), Some(detail));
}

#[test]
fn prompt_update_persists_inference_5xx_failure_taxonomy() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    store
        .insert_session(NewSessionRecord {
            id: "sess_5xx".to_owned(),
            agent_id: "fake".to_owned(),
            cwd: "/tmp/5xx".to_owned(),
            title: None,
            metadata_json: "{}".to_owned(),
        })
        .expect("session inserted");
    store
        .insert_prompt(NewPromptRecord {
            id: "prm_5xx".to_owned(),
            session_id: "sess_5xx".to_owned(),
            prompt_json: "[]".to_owned(),
        })
        .expect("prompt inserted");

    let detail = r#"{"upstream_status":502,"provider":"acme"}"#;
    store
        .update_prompt_status(
            "prm_5xx",
            PromptStatus::Errored,
            None,
            Some("inference.upstream"),
            Some("upstream returned 502"),
            Some(FailureClass::Inference5xx.as_str()),
            Some(detail),
        )
        .expect("prompt status updated to errored");

    let prompt = store
        .get_prompt("prm_5xx")
        .expect("prompt lookup")
        .expect("prompt exists");
    assert_eq!(prompt.status, PromptStatus::Errored.as_str());
    assert_eq!(prompt.failure_class.as_deref(), Some("inference_5xx"));
    assert_eq!(prompt.failure_detail_json.as_deref(), Some(detail));
    assert_eq!(prompt.error_code.as_deref(), Some("inference.upstream"));
}

#[test]
fn prompt_update_preserves_taxonomy_when_called_with_none() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    store
        .insert_session(NewSessionRecord {
            id: "sess_preserve".to_owned(),
            agent_id: "fake".to_owned(),
            cwd: "/tmp/preserve".to_owned(),
            title: None,
            metadata_json: "{}".to_owned(),
        })
        .expect("session inserted");
    store
        .insert_prompt(NewPromptRecord {
            id: "prm_preserve".to_owned(),
            session_id: "sess_preserve".to_owned(),
            prompt_json: "[]".to_owned(),
        })
        .expect("prompt inserted");

    // First write (non-terminal) sets the taxonomy; second write transitions
    // to a terminal status with None on both taxonomy params and must NOT
    // clobber the existing failure_class / failure_detail_json. The terminal
    // write is the only one that lands on already-set rows in production —
    // the supervisor sets a running-state taxonomy and then settles once.
    store
        .update_prompt_status(
            "prm_preserve",
            PromptStatus::Running,
            None,
            Some("vm.boom"),
            Some("vm crashed"),
            Some(FailureClass::Vm.as_str()),
            Some(r#"{"node":"vm-1"}"#),
        )
        .expect("first update");
    store
        .update_prompt_status(
            "prm_preserve",
            PromptStatus::Errored,
            None,
            Some("vm.boom"),
            Some("vm crashed (settle pass)"),
            None,
            None,
        )
        .expect("second update");

    let prompt = store
        .get_prompt("prm_preserve")
        .expect("prompt lookup")
        .expect("prompt exists");
    assert_eq!(prompt.status, PromptStatus::Errored.as_str());
    assert_eq!(prompt.failure_class.as_deref(), Some("vm"));
    assert_eq!(
        prompt.failure_detail_json.as_deref(),
        Some(r#"{"node":"vm-1"}"#)
    );
    assert_eq!(
        prompt.error_message.as_deref(),
        Some("vm crashed (settle pass)")
    );
}

#[test]
fn prompt_update_clears_taxonomy_with_empty_string_sentinel() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("state should open");
    store.migrate().expect("migration should pass");

    store
        .insert_session(NewSessionRecord {
            id: "sess_clear".to_owned(),
            agent_id: "fake".to_owned(),
            cwd: "/tmp/clear".to_owned(),
            title: None,
            metadata_json: "{}".to_owned(),
        })
        .expect("session inserted");
    store
        .insert_prompt(NewPromptRecord {
            id: "prm_clear".to_owned(),
            session_id: "sess_clear".to_owned(),
            prompt_json: "[]".to_owned(),
        })
        .expect("prompt inserted");

    // First write (non-terminal) sets the taxonomy; second write transitions
    // to terminal with Some("") for both taxonomy params and must clear them.
    store
        .update_prompt_status(
            "prm_clear",
            PromptStatus::Running,
            None,
            None,
            None,
            Some(FailureClass::Daemon.as_str()),
            Some(r#"{"k":"v"}"#),
        )
        .expect("first update sets taxonomy");
    store
        .update_prompt_status(
            "prm_clear",
            PromptStatus::Errored,
            None,
            None,
            None,
            Some(""),
            Some(""),
        )
        .expect("second update clears taxonomy");

    let prompt = store
        .get_prompt("prm_clear")
        .expect("prompt lookup")
        .expect("prompt exists");
    assert_eq!(prompt.status, PromptStatus::Errored.as_str());
    assert!(prompt.failure_class.is_none());
    assert!(prompt.failure_detail_json.is_none());
}
