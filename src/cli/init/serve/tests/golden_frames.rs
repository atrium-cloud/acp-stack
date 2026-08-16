//! Golden byte pins for the server→client frame surface. These assert the
//! exact serialized bytes, not just the fields, because the platform proxy
//! and its recorded fixtures read them. Two different key orders are in
//! play and both are load-bearing: seq-bearing events are assembled through
//! a `BTreeMap` and come out alphabetically sorted, while every seq-less
//! frame comes out in declaration order. `agent-client-protocol` turns on
//! `serde_json/preserve_order` for the whole build, so `Map` is insertion
//! ordered and neither order can be assumed to be the other.
//!
//! Frames are read back from the recorded history rather than the broadcast
//! channel: history is written while the session lock is held, so it is
//! race-free against the wizard thread, and the WebSocket sends exactly
//! `frame.to_string()` of the same `Value`.

use super::super::*;
use super::support::*;

use serde_json::json;

#[test]
fn golden_progress_event_bytes() {
    let session = test_session("init_golden_progress");
    let driver = SessionPromptDriver {
        session: session.clone(),
    };
    driver.progress("materializing workspace".to_owned());
    // The session constructor records the first progress event itself.
    assert_eq!(
        recorded_frame(&session, 1),
        r#"{"message":"init session started","seq":1,"session_id":"init_golden_progress","type":"progress"}"#
    );
    assert_eq!(
        recorded_frame(&session, 2),
        r#"{"message":"materializing workspace","seq":2,"session_id":"init_golden_progress","type":"progress"}"#
    );
}

#[test]
fn golden_input_required_and_input_accepted_event_bytes() {
    let session = test_session("init_golden_input");
    let driver = SessionPromptDriver {
        session: session.clone(),
    };
    let request = hosted_test_request(
        HostedPromptKind::Model,
        HostedPromptStyle::SearchableSelect,
        "select a model",
        &["alpha", "beta"],
    );
    let handle = std::thread::spawn(move || driver.select(request));
    let pending = wait_for_pending_input(&session);
    session
        .submit_input(&pending.request_id, json!(1))
        .expect("submit input");
    let outcome = handle
        .join()
        .expect("driver thread")
        .expect("driver result");
    assert!(matches!(outcome, HostedPromptOutcome::Handled(Some(1))));

    // The nested `input` object is the only frame body whose key order
    // comes from a `Serialize` impl rendered into a `Map`, so it is the
    // only one that would silently reorder if `preserve_order` ever went
    // away. Pinning it as bytes (with the per-request `request_id`
    // spliced in) is what makes that visible. `kind` sits after
    // `request_id` and each option's `value` after its `index`.
    assert_eq!(
        recorded_frame(&session, 2),
        format!(
            r#"{{"input":{{"request_id":"{}","kind":"model","style":"searchable_select","prompt":"select a model","required":false,"default":null,"options":[{{"index":0,"value":"id_alpha","label":"alpha","hint":""}},{{"index":1,"value":"id_beta","label":"beta","hint":""}}]}},"seq":2,"session_id":"init_golden_input","type":"input_required"}}"#,
            pending.request_id
        )
    );
    // seq 3 is the state frame the prompt raised (pinned in
    // `golden_state_event_bytes`); the acceptance follows it.
    assert_eq!(
        recorded_frame(&session, 4),
        format!(
            r#"{{"request_id":"{}","seq":4,"session_id":"init_golden_input","type":"input_accepted"}}"#,
            pending.request_id
        )
    );
}

#[test]
fn golden_state_event_bytes() {
    // Two key orders in one frame, both deliberate: the envelope sorts
    // `categories`/`current_step`/`seq`/`session_id`/`type`
    // alphabetically like every other seq-bearing event, while each
    // category object keeps its declared `id`/`status`/`blocked_on`/
    // `value`/`code`/`reason` order.
    let session = test_session("init_golden_state");
    session.apply_state_signal(InitStateSignal::StepStarted {
        kind: step_kind::PROVIDER_CONFIGURE,
    });
    session.apply_state_signal(InitStateSignal::CategorySettled {
        category: InitCategory::Agent,
        value: Some("opencode".to_owned()),
    });
    session.apply_state_signal(InitStateSignal::CategoryApplicability {
        category: InitCategory::Mode,
        applicable: false,
        source: ApplicabilitySource::Registry,
        reason: Some("agent does not take a mode".to_owned()),
    });
    session.apply_state_signal(InitStateSignal::CategoryFailed {
        category: InitCategory::Skills,
        code: "init.skills_install_failed".to_owned(),
    });
    assert_eq!(
        recorded_frame(&session, 5),
        r#"{"categories":[{"id":"agent","status":"settled","value":"opencode"},{"id":"provider","status":"ready"},{"id":"model","status":"blocked","blocked_on":"provider"},{"id":"mode","status":"not_applicable","reason":"agent does not take a mode"},{"id":"workspace","status":"ready"},{"id":"native_config","status":"ready"},{"id":"mcp","status":"ready"},{"id":"skills","status":"failed","code":"init.skills_install_failed"},{"id":"deps","status":"ready"}],"current_step":"provider_configure","seq":5,"session_id":"init_golden_state","type":"state"}"#
    );
}

#[test]
fn golden_result_ready_result_frame_and_result_acked_bytes() {
    let session = test_session("init_golden_result");
    // Nested objects and non-ASCII text pin the `format!` splice: the
    // stored result is forwarded verbatim, never re-encoded or re-ordered.
    session.set_result(json!({
        "note": "héllo ✅",
        "handoff": {"token": "t", "nested": {"a": [1, 2]}}
    }));
    assert_eq!(
        recorded_frame(&session, 2),
        r#"{"seq":2,"session_id":"init_golden_result","status":"completed_awaiting_ack","type":"result_ready"}"#
    );
    assert_eq!(
        session.result_frame().expect("result frame"),
        r#"{"type":"result","session_id":"init_golden_result","payload":{"note":"héllo ✅","handoff":{"token":"t","nested":{"a":[1,2]}}}}"#
    );
    session.ack_result().expect("ack result");
    assert_eq!(
        recorded_frame(&session, 3),
        r#"{"seq":3,"session_id":"init_golden_result","status":"closed","type":"result_acked"}"#
    );
}

#[test]
fn golden_canceled_event_bytes() {
    let session = test_session("init_golden_cancel");
    session.cancel("backend_cancel");
    assert_eq!(
        recorded_frame(&session, 2),
        r#"{"reason":"backend_cancel","seq":2,"session_id":"init_golden_cancel","type":"canceled"}"#
    );
}

#[test]
fn golden_error_replay_and_error_acked_bytes() {
    let session = test_session("init_golden_error");
    session.set_error("init.boom", "it broke".to_owned());
    assert_eq!(
        recorded_frame(&session, 2),
        r#"{"code":"init.boom","message":"it broke","seq":2,"session_id":"init_golden_error","type":"error"}"#
    );
    assert_eq!(
        session.error_replay_frame().expect("error replay frame"),
        r#"{"type":"error","session_id":"init_golden_error","code":"init.boom","message":"it broke"}"#
    );
    session.ack_error().expect("ack error");
    assert_eq!(
        recorded_frame(&session, 3),
        r#"{"seq":3,"session_id":"init_golden_error","status":"errored","type":"error_acked"}"#
    );
}

#[test]
fn golden_error_expired_event_bytes() {
    let session = test_session("init_golden_expired");
    session.set_error("init.boom", "it broke".to_owned());
    session.expire("error_ack_timeout");
    assert_eq!(
        recorded_frame(&session, 3),
        r#"{"reason":"error_ack_timeout","seq":3,"session_id":"init_golden_expired","type":"error_expired"}"#
    );
}

#[test]
fn golden_hello_frame_bytes() {
    // `state` sits beside `status`: the two answer the same question at
    // different resolutions, and the snapshot is declaration-ordered
    // (`current_step` then `categories`) unlike the sorted state event.
    let session = test_session("init_golden_hello");
    assert_eq!(
        session.hello_frame(),
        format!(
            r#"{{"type":"hello","session_id":"init_golden_hello","status":"running","state":{FRESH_STATE_JSON},"last_seq":1,"pending_input":null,"result_available":false,"error":null}}"#
        )
    );
    // The errored hello pins the nested `PublicError` object, the one
    // part of the frame that goes through a `Serialize` impl.
    session.set_error("init.boom", "it broke".to_owned());
    assert_eq!(
        session.hello_frame(),
        format!(
            r#"{{"type":"hello","session_id":"init_golden_hello","status":"errored","state":{FRESH_STATE_JSON},"last_seq":2,"pending_input":null,"result_available":false,"error":{{"code":"init.boom","message":"it broke"}}}}"#
        )
    );
}

/// The snapshot of a session no signal has reached yet: nothing is settled,
/// the four root categories are ready, and everything else is blocked on
/// the dependency table.
const FRESH_STATE_JSON: &str = r#"{"current_step":null,"categories":[{"id":"agent","status":"ready"},{"id":"provider","status":"blocked","blocked_on":"agent"},{"id":"model","status":"blocked","blocked_on":"provider"},{"id":"mode","status":"blocked","blocked_on":"model"},{"id":"workspace","status":"ready"},{"id":"native_config","status":"ready"},{"id":"mcp","status":"blocked","blocked_on":"agent"},{"id":"skills","status":"blocked","blocked_on":"agent"},{"id":"deps","status":"ready"}]}"#;

#[test]
fn golden_hello_frame_with_pending_input_and_result_bytes() {
    // The reconnect cases: a hello sent while the wizard is blocked on a
    // prompt, and one sent after the result is waiting to be acked. Both
    // populate fields the fresh-session hello leaves null.
    let session = test_session("init_golden_hello_pending");
    let driver = SessionPromptDriver {
        session: session.clone(),
    };
    let request = hosted_test_request(
        HostedPromptKind::Model,
        HostedPromptStyle::Text,
        "model",
        &[],
    );
    let handle = std::thread::spawn(move || driver.text(request));
    let pending = wait_for_pending_input(&session);
    // The pending prompt is what makes `model` derive as awaiting_input,
    // and it is the only category that may: there is one prompt slot.
    assert_eq!(
        session.hello_frame(),
        format!(
            r#"{{"type":"hello","session_id":"init_golden_hello_pending","status":"waiting_for_input","state":{{"current_step":null,"categories":[{{"id":"agent","status":"ready"}},{{"id":"provider","status":"blocked","blocked_on":"agent"}},{{"id":"model","status":"awaiting_input"}},{{"id":"mode","status":"blocked","blocked_on":"model"}},{{"id":"workspace","status":"ready"}},{{"id":"native_config","status":"ready"}},{{"id":"mcp","status":"blocked","blocked_on":"agent"}},{{"id":"skills","status":"blocked","blocked_on":"agent"}},{{"id":"deps","status":"ready"}}]}},"last_seq":3,"pending_input":{{"request_id":"{}","kind":"model","style":"text","prompt":"model","required":false,"default":null,"options":[]}},"result_available":false,"error":null}}"#,
            pending.request_id
        )
    );
    session
        .submit_input(&pending.request_id, json!("gpt-5"))
        .expect("submit input");
    handle
        .join()
        .expect("driver thread")
        .expect("driver result");

    session.set_result(json!({"status": "initialized"}));
    // The answered prompt released the frontier, so the completed hello is
    // back to the fresh snapshot: an answer settles nothing on its own.
    assert_eq!(
        session.hello_frame(),
        format!(
            r#"{{"type":"hello","session_id":"init_golden_hello_pending","status":"completed_awaiting_ack","state":{FRESH_STATE_JSON},"last_seq":6,"pending_input":null,"result_available":true,"error":null}}"#
        )
    );
}

#[test]
fn golden_ack_accepted_and_error_acked_close_frame_bytes() {
    let session = test_session("init_golden_close");
    session.set_result(json!({"status": "initialized"}));
    let ClientFrameOutcome::Close(frame) =
        handle_client_frame(&session, r#"{"type":"ack_result"}"#)
    else {
        panic!("ack_result must close the connection");
    };
    assert_eq!(
        frame,
        r#"{"type":"ack_accepted","session_id":"init_golden_close"}"#
    );

    let errored = test_session("init_golden_close_error");
    errored.set_error("init.boom", "it broke".to_owned());
    let ClientFrameOutcome::Close(frame) = handle_client_frame(&errored, r#"{"type":"ack_error"}"#)
    else {
        panic!("ack_error must close the connection");
    };
    assert_eq!(
        frame,
        r#"{"type":"error_acked","session_id":"init_golden_close_error"}"#
    );
}

#[test]
fn golden_protocol_error_frame_bytes() {
    let session = test_session("init_golden_protocol");
    let sent = |text: &str| match handle_client_frame(&session, text) {
        ClientFrameOutcome::Send(frame) => frame,
        _ => panic!("frame `{text}` must produce a Send outcome"),
    };
    assert_eq!(
        sent("not json"),
        r#"{"type":"error","code":"init.bad_frame","message":"invalid client frame: expected ident at line 1 column 2"}"#
    );
    assert_eq!(
        sent(r#"{"type":"teleport"}"#),
        r#"{"type":"error","code":"init.unsupported_frame","message":"unsupported client frame `teleport`"}"#
    );
    assert_eq!(
        sent(r#"{"type":"input"}"#),
        r#"{"type":"error","code":"init.missing_request_id","message":"input frame requires request_id"}"#
    );
    assert_eq!(
        sent(r#"{"type":"input","request_id":"ireq_stale"}"#),
        r#"{"type":"error","code":"init.input_rejected","message":"no input request is pending"}"#
    );
    assert_eq!(
        sent(r#"{"type":"replay_result"}"#),
        r#"{"type":"error","code":"init.result_unavailable","message":"init result is not available"}"#
    );
    assert_eq!(
        sent(r#"{"type":"replay_error"}"#),
        r#"{"type":"error","code":"init.error_unavailable","message":"no init error is recorded for this session"}"#
    );
    assert_eq!(
        sent(r#"{"type":"ack_result"}"#),
        r#"{"type":"error","code":"init.ack_rejected","message":"no init result is awaiting acknowledgement"}"#
    );
    assert_eq!(
        ws_lagged_frame(),
        r#"{"type":"error","code":"init.ws_lagged","message":"websocket client lagged behind init event stream"}"#
    );
}

#[test]
fn golden_encode_failure_frame_is_valid_json() {
    // This frame is spliced from constants instead of serialized, which is
    // what makes it the one frame that cannot itself fail to encode. That
    // only holds while neither constant contains a JSON metacharacter, so
    // the round-trip is asserted rather than assumed.
    let frame = encode_failure_frame();
    assert_eq!(
        frame,
        r#"{"type":"error","code":"init.frame_encode_failed","message":"init frame payload could not be encoded"}"#
    );
    let parsed: Value = serde_json::from_str(&frame).expect("encode-failure frame must parse");
    assert_eq!(parsed["code"], json!(FRAME_ENCODE_FAILED_CODE));
    assert_eq!(parsed["message"], json!(FRAME_ENCODE_FAILED_MESSAGE));
}

#[test]
fn serde_json_map_is_insertion_ordered() {
    // A canary, not a preference: `agent-client-protocol` turns on
    // `serde_json/preserve_order` for the whole build, which is why
    // seq-bearing events are assembled through a `BTreeMap` and seq-less
    // frames through derived structs. If a dependency change ever drops
    // the feature, this fails loudly instead of quietly re-sorting keys
    // the golden pins above cannot all observe.
    assert_eq!(json!({"b": 1, "a": 2}).to_string(), r#"{"b":1,"a":2}"#);
}
