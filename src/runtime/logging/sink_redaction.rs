//! Per-table redaction allowlists for the Supabase logging sink. Default closed: an unmodeled
//! table errors, so a new source table MUST be added here before it can be mirrored outbound.

use crate::error::{Result, StackError};
use serde_json::{Map, Value, json};

pub fn redact_row(table: &str, row: &mut Map<String, Value>) -> Result<()> {
    match table {
        "events" => redact_events_row(row),
        "sessions" => redact_sessions_row(row),
        "prompts" => redact_prompts_row(row),
        "commands" => redact_commands_row(row),
        "permission_requests" => redact_permission_requests_row(row),
        "permission_decisions" => redact_permission_decisions_row(row),
        "auth_failures" => redact_auth_failures_row(row),
        "agent_lifecycle" => redact_agent_lifecycle_row(row),
        other => Err(StackError::SupabaseSinkUnknownTable {
            table: other.to_owned(),
        }),
    }
}

const EVENTS_PAYLOAD_KEEP: &[&str] = &[
    "session_id",
    "kind",
    "duration_ms",
    "status",
    "exit_code",
    "input_tokens",
    "output_tokens",
    "context_window_used",
    "context_window_max",
    "cost_amount",
    "cost_currency",
    "agent_id",
    "command_id",
    "request_id",
    "bind",
    "client_label",
    "reason_code",
];

const AGENT_LIFECYCLE_PAYLOAD_KEEP: &[&str] = &[
    "bind",
    "agent_id",
    "exit_code",
    "duration_ms",
    "capabilities_hash",
];

fn redact_events_row(row: &mut Map<String, Value>) -> Result<()> {
    redact_json_column(row, "payload_json", EVENTS_PAYLOAD_KEEP);
    // Free-form text is replaced wholesale so short inline secrets cannot pass a length cap.
    redact_string_field(row, "message");
    Ok(())
}

fn redact_sessions_row(row: &mut Map<String, Value>) -> Result<()> {
    // Keep only agent_id so an upstream regression stuffing secrets into metadata cannot leak.
    if let Some(metadata) = row.get_mut("metadata_json") {
        let mut redacted = Map::new();
        if let Some(obj) = metadata.as_object()
            && let Some(agent_id) = obj.get("agent_id")
            && let Some(value) = safe_json_scalar(agent_id)
        {
            redacted.insert("agent_id".to_owned(), value);
        }
        *metadata = Value::Object(redacted);
    }
    if row.contains_key("title") {
        row.insert("title".to_owned(), Value::Null);
    }
    // `cwd` can encode private repo or user names; the Postgres mirror column is NOT NULL, so
    // it redacts to the empty string rather than null.
    if row.contains_key("cwd") {
        row.insert("cwd".to_owned(), Value::String(String::new()));
    }
    Ok(())
}

fn redact_prompt_scalars(row: &mut Map<String, Value>) {
    // Adapter-dependent free-form text; replaced wholesale rather than length-capped.
    redact_string_field(row, "error_message");
    redact_string_field(row, "stop_reason");
}

fn redact_prompts_row(row: &mut Map<String, Value>) -> Result<()> {
    redact_prompt_scalars(row);
    // Prompt content keeps only a length stamp. The byte count comes from the hydrator's
    // out-of-band hint because the in-memory `Value` is no proxy for the on-disk size.
    let byte_len = row
        .remove("_prompt_json_bytes")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .or_else(|| {
            row.get("prompt_json").and_then(|v| match v {
                Value::String(s) => Some(s.len()),
                Value::Null => Some(0),
                _ => serde_json::to_string(v).ok().map(|s| s.len()),
            })
        })
        .unwrap_or(0);
    row.insert(
        "prompt_json".to_owned(),
        json!({"redacted": true, "byte_len": byte_len}),
    );
    Ok(())
}

fn redact_commands_row(row: &mut Map<String, Value>) -> Result<()> {
    // env vars routinely carry secrets, so the whole column becomes a count.
    let env_var_count = row
        .get("env_json")
        .and_then(|v| match v {
            Value::Object(m) => Some(m.len()),
            Value::Null => Some(0),
            _ => None,
        })
        .unwrap_or(0);
    row.insert(
        "env_json".to_owned(),
        json!({"env_var_count": env_var_count}),
    );
    // Command lines and cwd routinely carry inline credentials, so the external mirror gets a
    // length stamp only; the local SQLite store still keeps the full value for audit.
    let command_byte_len = row
        .get("command")
        .and_then(|v| v.as_str())
        .map(|s| s.len())
        .unwrap_or(0);
    row.insert(
        "command".to_owned(),
        Value::String(format!("[redacted; {command_byte_len} bytes]")),
    );
    if row.contains_key("cwd") {
        row.insert("cwd".to_owned(), Value::Null);
    }
    Ok(())
}

fn redact_permission_requests_row(row: &mut Map<String, Value>) -> Result<()> {
    redact_json_column(row, "detail_json", EVENTS_PAYLOAD_KEEP);
    Ok(())
}

fn redact_permission_decisions_row(row: &mut Map<String, Value>) -> Result<()> {
    // The operator's free-form note; replaced wholesale so pasted tokens cannot pass a cap.
    redact_string_field(row, "reason");
    Ok(())
}

fn redact_auth_failures_row(row: &mut Map<String, Value>) -> Result<()> {
    // payload_json here is internal-debugging freeform and never ships outbound; `route` is an
    // unauthenticated request path that can carry path-embedded tokens.
    row.insert("payload_json".to_owned(), Value::Object(Map::new()));
    if let Some(reason) = row.get_mut("reason")
        && let Some(text) = reason.as_str()
    {
        *reason = redact_auth_failure_reason(text);
    }
    if row.contains_key("route") {
        row.insert("route".to_owned(), Value::Null);
    }
    Ok(())
}

fn redact_agent_lifecycle_row(row: &mut Map<String, Value>) -> Result<()> {
    redact_json_column(row, "payload_json", AGENT_LIFECYCLE_PAYLOAD_KEEP);
    redact_string_field(row, "message");
    Ok(())
}

/// Keep only allowlisted top-level keys with SCALAR values; nested arrays and objects are
/// dropped even under an allowed name, since they can smuggle structure through a safe key.
fn redact_json_column(row: &mut Map<String, Value>, column: &str, allow: &[&str]) {
    let mut filtered = Map::new();
    if let Some(Value::Object(obj)) = row.get(column) {
        for key in allow {
            if let Some(v) = obj.get(*key)
                && let Some(value) = safe_json_scalar(v)
            {
                filtered.insert((*key).to_owned(), value);
            }
        }
    }
    row.insert(column.to_owned(), Value::Object(filtered));
}

fn safe_json_scalar(value: &Value) -> Option<Value> {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => Some(value.clone()),
        Value::Array(_) | Value::Object(_) => None,
    }
}

/// Replace a top-level string column with a length stamp; no-op when null or absent.
fn redact_string_field(row: &mut Map<String, Value>, key: &str) {
    if let Some(v) = row.get_mut(key)
        && let Some(text) = v.as_str()
    {
        *v = Value::String(format!("[redacted; {} bytes]", text.len()));
    }
}

fn redact_auth_failure_reason(text: &str) -> Value {
    if matches!(
        text,
        "missing" | "invalid" | "wrong_kind" | "malformed_header"
    ) {
        Value::String(text.to_owned())
    } else {
        Value::String(format!("[redacted; {} bytes]", text.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(m) => m,
            _ => panic!("expected object"),
        }
    }

    fn assert_redacted_length_stamp(value: &Value, expected_bytes: usize) {
        let text = value.as_str().expect("value should be string");
        assert_eq!(text, format!("[redacted; {expected_bytes} bytes]"));
    }

    #[test]
    fn events_payload_drops_unknown_keys() {
        let mut row = obj(json!({
            "id": "evt_1",
            "kind": "api.request",
            "payload_json": {
                "session_id": "sess_1",
                "duration_ms": 12,
                "request_id": {"token": "sk-nested"},
                "api_key": "sk-leak",
                "Authorization": "Bearer leak"
            }
        }));
        redact_row("events", &mut row).expect("redact events");
        let payload = row.get("payload_json").and_then(|v| v.as_object()).unwrap();
        assert_eq!(
            payload.get("session_id").and_then(|v| v.as_str()),
            Some("sess_1")
        );
        assert_eq!(
            payload.get("duration_ms").and_then(|v| v.as_i64()),
            Some(12)
        );
        assert!(
            payload.get("api_key").is_none(),
            "secret-looking key must be dropped"
        );
        assert!(
            payload.get("request_id").is_none(),
            "nested allowed key must be dropped"
        );
        assert!(payload.get("Authorization").is_none());
        let serialized = serde_json::to_string(&row).expect("serialize");
        assert!(!serialized.contains("sk-nested"));
    }

    #[test]
    fn events_payload_preserves_normalized_acp_usage_fields() {
        let mut row = obj(json!({
            "id": "evt_usage",
            "kind": "usage.reported",
            "payload_json": {
                "context_window_used": 4096,
                "context_window_max": 32768,
                "cost_amount": 1.25,
                "cost_currency": "USD",
                "untrusted_detail": "drop me"
            }
        }));
        redact_row("events", &mut row).expect("redact events");
        let payload = row.get("payload_json").and_then(Value::as_object).unwrap();
        assert_eq!(payload.get("context_window_used"), Some(&json!(4096)));
        assert_eq!(payload.get("context_window_max"), Some(&json!(32768)));
        assert_eq!(payload.get("cost_amount"), Some(&json!(1.25)));
        assert_eq!(payload.get("cost_currency"), Some(&json!("USD")));
        assert!(payload.get("untrusted_detail").is_none());
    }

    #[test]
    fn events_message_is_redacted() {
        let secret = "sk-event-secret";
        let message = format!("prefix-{secret}");
        let mut row = obj(json!({
            "id": "evt_1",
            "message": message,
            "payload_json": {}
        }));
        redact_row("events", &mut row).expect("redact events");
        let serialized = serde_json::to_string(&row).expect("serialize");
        assert!(
            !serialized.contains(secret),
            "event message secret should be fully redacted"
        );
        assert_redacted_length_stamp(
            row.get("message").expect("message present"),
            "prefix-".len() + secret.len(),
        );
    }

    #[test]
    fn events_payload_replaced_with_empty_object_when_missing() {
        let mut row = obj(json!({"id": "evt_1"}));
        redact_row("events", &mut row).expect("redact events");
        assert_eq!(
            row.get("payload_json").and_then(|v| v.as_object()),
            Some(&Map::new())
        );
    }

    #[test]
    fn prompt_json_replaced_with_byte_len() {
        let mut row = obj(json!({
            "id": "prm_1",
            "prompt_json": {"content": "very long secret-bearing prompt body"}
        }));
        redact_row("prompts", &mut row).expect("redact prompts");
        let pj = row.get("prompt_json").and_then(|v| v.as_object()).unwrap();
        assert_eq!(pj.get("redacted").and_then(|v| v.as_bool()), Some(true));
        let byte_len = pj.get("byte_len").and_then(|v| v.as_u64()).unwrap();
        assert!(byte_len > 0);
        let serialized = serde_json::to_string(&row).expect("serialize");
        assert!(
            !serialized.contains("very long secret-bearing prompt body"),
            "prompt body must not survive redaction"
        );
    }

    #[test]
    fn prompt_scalar_error_fields_are_redacted() {
        let stop_secret = "stop-secret";
        let error_secret = "error-secret";
        let stop_reason = format!("cancelled-{stop_secret}");
        let error_message = format!("failed with {error_secret}");
        let mut row = obj(json!({
            "id": "prm_1",
            "stop_reason": stop_reason,
            "error_message": error_message,
            "prompt_json": {"content": "prompt body"}
        }));
        redact_row("prompts", &mut row).expect("redact prompts");
        let serialized = serde_json::to_string(&row).expect("serialize");
        assert!(
            !serialized.contains(stop_secret),
            "stop_reason secret leaked"
        );
        assert!(
            !serialized.contains(error_secret),
            "error_message secret leaked"
        );
        assert_redacted_length_stamp(
            row.get("stop_reason").expect("stop_reason present"),
            "cancelled-".len() + stop_secret.len(),
        );
        assert_redacted_length_stamp(
            row.get("error_message").expect("error_message present"),
            "failed with ".len() + error_secret.len(),
        );
    }

    #[test]
    fn env_json_replaced_with_count() {
        let mut row = obj(json!({
            "id": "cmd_1",
            "command": "echo hi",
            "env_json": {"OPENAI_API_KEY": "sk-leak", "FOO": "bar"}
        }));
        redact_row("commands", &mut row).expect("redact commands");
        let env = row.get("env_json").and_then(|v| v.as_object()).unwrap();
        assert_eq!(env.get("env_var_count").and_then(|v| v.as_u64()), Some(2));
        let serialized = serde_json::to_string(&row).expect("serialize");
        assert!(!serialized.contains("sk-leak"));
        assert!(!serialized.contains("OPENAI_API_KEY"));
    }

    #[test]
    fn auth_failures_drops_payload_and_keeps_structural_reason() {
        let mut row = obj(json!({
            "id": "af_1",
            "reason": "invalid",
            "payload_json": {"raw_token": "secret"},
        }));
        redact_row("auth_failures", &mut row).expect("redact auth_failures");
        assert_eq!(
            row.get("payload_json").and_then(|v| v.as_object()),
            Some(&Map::new())
        );
        assert_eq!(row.get("reason").and_then(|v| v.as_str()), Some("invalid"));
        let serialized = serde_json::to_string(&row).expect("serialize");
        assert!(!serialized.contains("secret"));
    }

    #[test]
    fn auth_failures_redacts_unknown_reason() {
        let secret_reason = "sk-auth-secret";
        let secret_route = "/v1/secrets/sk-route-secret";
        let mut row = obj(json!({
            "id": "af_1",
            "reason": secret_reason,
            "route": secret_route,
            "payload_json": {"raw_token": "secret"},
        }));
        redact_row("auth_failures", &mut row).expect("redact auth_failures");
        assert_redacted_length_stamp(
            row.get("reason").expect("reason present"),
            secret_reason.len(),
        );
        assert!(row.get("route").map(|v| v.is_null()).unwrap_or(false));
        let serialized = serde_json::to_string(&row).expect("serialize");
        assert!(!serialized.contains(secret_reason));
        assert!(!serialized.contains(secret_route));
        assert!(!serialized.contains("secret"));
    }

    #[test]
    fn sessions_metadata_keeps_scalar_agent_id_and_redacts_free_text() {
        let mut row = obj(json!({
            "id": "sess_1",
            "cwd": "/var/secrets/repo",
            "title": "secret meeting notes",
            "metadata_json": {
                "agent_id": "claude",
                "previous_agent_id": {"token": "sk-nested"},
                "internal_state": "do-not-export"
            }
        }));
        redact_row("sessions", &mut row).expect("redact sessions");
        assert!(row.get("title").map(|v| v.is_null()).unwrap_or(false));
        assert_eq!(row.get("cwd").and_then(|v| v.as_str()), Some(""));
        let serialized = serde_json::to_string(&row).expect("serialize");
        assert!(
            !serialized.contains("secret meeting notes"),
            "title text must not survive redaction"
        );
        assert!(
            !serialized.contains("/var/secrets/repo"),
            "cwd must not survive redaction"
        );
        assert!(!serialized.contains("sk-nested"));
        let metadata = row
            .get("metadata_json")
            .and_then(|v| v.as_object())
            .unwrap();
        assert_eq!(
            metadata.get("agent_id").and_then(|v| v.as_str()),
            Some("claude")
        );
        assert!(metadata.get("internal_state").is_none());
        assert!(metadata.get("previous_agent_id").is_none());
    }

    #[test]
    fn commands_command_and_cwd_are_redacted() {
        let mut row = obj(json!({
            "id": "cmd_1",
            "command": "curl -u admin:secret-token https://api.example/x",
            "cwd": "/var/secrets/repo",
            "env_json": {}
        }));
        redact_row("commands", &mut row).expect("redact commands");
        let serialized = serde_json::to_string(&row).expect("serialize");
        assert!(
            !serialized.contains("secret-token"),
            "inline credential leaked: {serialized}"
        );
        assert!(!serialized.contains("/var/secrets/repo"), "cwd leaked");
        let command = row.get("command").and_then(|v| v.as_str()).unwrap();
        assert!(command.starts_with("[redacted;"), "got {command}");
    }

    #[test]
    fn agent_lifecycle_payload_uses_dedicated_allowlist() {
        let secret_message = "started with sk-agent-secret";
        let mut row = obj(json!({
            "id": "agl_1",
            "message": secret_message,
            "payload_json": {
                "bind": "0.0.0.0:8080",
                "agent_id": {"token": "sk-nested"},
                "secret_token": "leak"
            }
        }));
        redact_row("agent_lifecycle", &mut row).expect("redact agent_lifecycle");
        let payload = row.get("payload_json").and_then(|v| v.as_object()).unwrap();
        assert_eq!(
            payload.get("bind").and_then(|v| v.as_str()),
            Some("0.0.0.0:8080")
        );
        assert!(payload.get("agent_id").is_none());
        assert!(payload.get("secret_token").is_none());
        assert_redacted_length_stamp(
            row.get("message").expect("message present"),
            secret_message.len(),
        );
        let serialized = serde_json::to_string(&row).expect("serialize");
        assert!(!serialized.contains("sk-agent-secret"));
        assert!(!serialized.contains("sk-nested"));
        assert!(!serialized.contains("leak"));
    }

    #[test]
    fn permission_requests_uses_events_allowlist() {
        let mut row = obj(json!({
            "id": "perm_1",
            "detail_json": {
                "session_id": "sess_1",
                "request_id": {"token": "sk-nested"},
                "private": "x"
            }
        }));
        redact_row("permission_requests", &mut row).expect("redact permission_requests");
        let detail = row.get("detail_json").and_then(|v| v.as_object()).unwrap();
        assert_eq!(
            detail.get("session_id").and_then(|v| v.as_str()),
            Some("sess_1")
        );
        assert!(detail.get("request_id").is_none());
        assert!(detail.get("private").is_none());
        let serialized = serde_json::to_string(&row).expect("serialize");
        assert!(!serialized.contains("sk-nested"));
    }

    #[test]
    fn permission_decisions_reason_is_redacted() {
        let secret = "permission-secret";
        let reason = format!("denied because {secret}");
        let mut row = obj(json!({
            "id": "pdec_1",
            "decision": "approved",
            "reason": reason,
        }));
        redact_row("permission_decisions", &mut row).expect("redact permission_decisions");
        assert_eq!(
            row.get("decision").and_then(|v| v.as_str()),
            Some("approved")
        );
        let serialized = serde_json::to_string(&row).expect("serialize");
        assert!(
            !serialized.contains(secret),
            "permission decision reason secret leaked"
        );
        assert_redacted_length_stamp(
            row.get("reason").expect("reason present"),
            "denied because ".len() + secret.len(),
        );
    }

    #[test]
    fn unknown_table_errors_with_dedicated_variant() {
        let mut row = Map::new();
        let err = redact_row("unknown_secrets_table", &mut row).expect_err("should error");
        assert!(
            matches!(err, StackError::SupabaseSinkUnknownTable { ref table } if table == "unknown_secrets_table"),
            "unexpected error: {err:?}"
        );
    }
}
