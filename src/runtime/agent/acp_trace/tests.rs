use super::*;

fn tap() -> TraceTap {
    TraceTap::new(TraceLabels {
        agent_id: "placebo".to_owned(),
        target_id: Some("primary".to_owned()),
    })
}

#[test]
fn a_response_is_matched_to_its_request_with_elapsed_time() {
    let tap = tap();
    let request = tap.observe(
        Direction::ClientToAgent,
        r#"{"jsonrpc":"2.0","id":7,"method":"session/prompt","params":{"sessionId":"sess_1","prompt":[]}}"#,
    );
    assert_eq!(request.kind, "request");
    assert_eq!(request.method.as_deref(), Some("session/prompt"));
    assert_eq!(request.session_id.as_deref(), Some("sess_1"));
    assert_eq!(request.payload, None, "request params are never rendered");

    let response = tap.observe(
        Direction::AgentToClient,
        r#"{"jsonrpc":"2.0","id":7,"result":{"stopReason":"end_turn"}}"#,
    );
    assert_eq!(response.kind, "response");
    assert_eq!(response.method.as_deref(), Some("session/prompt"));
    assert_eq!(response.session_id.as_deref(), Some("sess_1"));
    assert_eq!(response.outcome, Some("ok"));
    assert!(response.elapsed_ms.is_some());
    assert_eq!(
        response.payload.as_deref(),
        Some(r#"{"stopReason":"end_turn"}"#)
    );
    assert!(tap.sent_by(Direction::ClientToAgent).is_empty());
}

#[test]
fn agent_requests_are_matched_to_client_responses() {
    let tap = tap();
    tap.observe(
        Direction::AgentToClient,
        r#"{"jsonrpc":"2.0","id":"fs-1","method":"fs/read_text_file","params":{"sessionId":"sess_2","path":"/w/a"}}"#,
    );
    let response = tap.observe(
        Direction::ClientToAgent,
        r#"{"jsonrpc":"2.0","id":"fs-1","error":{"code":-32602,"message":"outside workspace"}}"#,
    );
    assert_eq!(response.method.as_deref(), Some("fs/read_text_file"));
    assert_eq!(response.session_id.as_deref(), Some("sess_2"));
    assert_eq!(response.id.as_deref(), Some("fs-1"));
    assert_eq!(response.outcome, Some("error"));
    assert!(response.elapsed_ms.is_some());
    assert!(response.payload.is_some());
    assert!(tap.sent_by(Direction::AgentToClient).is_empty());
}

#[test]
fn client_results_to_agent_requests_are_not_rendered() {
    let tap = tap();
    tap.observe(
        Direction::AgentToClient,
        r#"{"jsonrpc":"2.0","id":4,"method":"fs/read_text_file","params":{"sessionId":"sess_2","path":"/w/.env"}}"#,
    );
    let response = tap.observe(
        Direction::ClientToAgent,
        r#"{"jsonrpc":"2.0","id":4,"result":{"content":"DATABASE_URL=postgres://u:p4ss@host"}}"#,
    );
    assert_eq!(response.method.as_deref(), Some("fs/read_text_file"));
    assert_eq!(response.outcome, Some("ok"));
    assert_eq!(response.payload, None);
}

#[test]
fn a_null_id_is_not_a_request_id() {
    let tap = tap();
    let notification = tap.observe(
        Direction::ClientToAgent,
        r#"{"jsonrpc":"2.0","id":null,"method":"session/cancel","params":{"sessionId":"sess_4"}}"#,
    );
    assert_eq!(notification.kind, "notification");
    assert_eq!(notification.id, None);

    let response = tap.observe(
        Direction::AgentToClient,
        r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error"}}"#,
    );
    assert_eq!(response.kind, "response");
    assert_eq!(response.method, None);
    assert_eq!(response.elapsed_ms, None);
    assert_eq!(response.outcome, Some("error"));
}

#[test]
fn json_outside_json_rpc_shapes_logs_no_payload() {
    for line in [
        r#"[{"jsonrpc":"2.0","method":"session/update","params":{"text":"secret plan"}}]"#,
        r#"{"jsonrpc":"2.0","params":{"text":"secret plan"}}"#,
    ] {
        let frame = tap().observe(Direction::AgentToClient, line);
        assert_eq!(frame.kind, "unrecognized", "{line}");
        assert_eq!(frame.payload, None, "{line}");
    }
}

#[test]
fn ids_and_methods_are_bounded() {
    let long = "m".repeat(1000);
    let frame = tap().observe(
        Direction::AgentToClient,
        &format!(r#"{{"jsonrpc":"2.0","id":"{long}","method":"{long}"}}"#),
    );
    for label in [frame.id.expect("id"), frame.method.expect("method")] {
        assert!(label.starts_with("mmm"), "{label}");
        assert!(label.contains("[truncated "), "{label}");
        assert!(label.len() < 300, "{label}");
    }
}

#[test]
fn an_unmatched_response_takes_its_session_from_the_result() {
    let response = tap().observe(
        Direction::AgentToClient,
        r#"{"jsonrpc":"2.0","id":3,"result":{"sessionId":"sess_new"}}"#,
    );
    assert_eq!(response.method, None);
    assert_eq!(response.elapsed_ms, None);
    assert_eq!(response.session_id.as_deref(), Some("sess_new"));
}

#[test]
fn notifications_carry_method_and_session_only() {
    let notification = tap().observe(
        Direction::AgentToClient,
        r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess_3","update":{"text":"hi"}}}"#,
    );
    assert_eq!(notification.kind, "notification");
    assert_eq!(notification.method.as_deref(), Some("session/update"));
    assert_eq!(notification.session_id.as_deref(), Some("sess_3"));
    assert_eq!(notification.id, None);
    assert_eq!(notification.payload, None);
}

#[test]
fn unparseable_lines_are_redacted_and_bounded() {
    let frame = tap().observe(
        Direction::AgentToClient,
        &format!("not json sk-AbCdEf123456 {}", "x".repeat(5000)),
    );
    assert_eq!(frame.kind, "unparsed");
    let payload = frame.payload.expect("payload");
    assert!(payload.starts_with("not json [redacted] xxx"), "{payload}");
    assert!(payload.contains("[truncated "), "{payload}");
}

#[test]
fn response_payloads_are_redacted_and_bounded() {
    crate::redaction::register_secret_values(["TraceCanary-4Rw92"]);
    let frame = tap().observe(
        Direction::AgentToClient,
        &format!(
            r#"{{"jsonrpc":"2.0","id":1,"error":{{"code":-32000,"message":"bad key TraceCanary-4Rw92","data":"{}"}}}}"#,
            "y".repeat(5000)
        ),
    );
    let payload = frame.payload.expect("payload");
    assert!(!payload.contains("TraceCanary-4Rw92"), "{payload}");
    assert!(payload.contains("bad key [redacted]"), "{payload}");
    assert!(payload.contains("[truncated "), "{payload}");
}

#[test]
fn session_ids_are_bounded() {
    let long = "s".repeat(1000);
    let tap = tap();
    let notification = tap.observe(
        Direction::AgentToClient,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"{long}"}}}}"#
        ),
    );
    let response = tap.observe(
        Direction::AgentToClient,
        &format!(r#"{{"jsonrpc":"2.0","id":9,"result":{{"sessionId":"{long}"}}}}"#),
    );
    for session_id in [notification.session_id, response.session_id] {
        let session_id = session_id.expect("session id");
        assert!(session_id.contains("[truncated "), "{session_id}");
        assert!(session_id.len() < 300, "{session_id}");
    }
}

#[test]
fn a_forgotten_request_leaves_nothing_in_flight() {
    let tap = tap();
    let request = tap.observe(
        Direction::ClientToAgent,
        r#"{"jsonrpc":"2.0","id":5,"method":"session/new","params":{}}"#,
    );
    tap.forget(&request);
    assert!(tap.sent_by(Direction::ClientToAgent).is_empty());
}
