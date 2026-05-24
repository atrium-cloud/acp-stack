use base64::Engine;
use serde_json::Value;

use crate::client::HttpResponse;

/// Returns `Err` when the server responded with `{ok:false}` (envelope error)
/// or when the HTTP status is not 2xx. The caller maps that into a non-zero
/// process exit code so failing acpctl operations are observable from
/// scripts and agents.
pub(crate) fn print_response<F>(
    resp: &HttpResponse,
    json_mode: bool,
    formatter: F,
) -> Result<(), String>
where
    F: FnOnce(&Value),
{
    let body_text = std::str::from_utf8(&resp.body).unwrap_or("");
    let parsed: Option<Value> = serde_json::from_str(body_text).ok();
    if json_mode {
        match &parsed {
            Some(value) => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(value).unwrap_or_default()
                );
            }
            None => println!("{body_text}"),
        }
    }
    let envelope_ok = parsed
        .as_ref()
        .and_then(|v| v.get("ok"))
        .and_then(Value::as_bool);
    let server_ok = (200..300).contains(&resp.status) && envelope_ok != Some(false);
    if !server_ok {
        let (code, message) = match parsed.as_ref() {
            Some(value) => {
                let code = value
                    .get("error")
                    .and_then(|e| e.get("code"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let message = value
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("(no message)");
                (code.to_owned(), message.to_owned())
            }
            None => (
                "http_error".to_owned(),
                format!("non-2xx response: {body_text}"),
            ),
        };
        return Err(format!("HTTP {} {code}: {message}", resp.status));
    }
    if json_mode {
        return Ok(());
    }
    let Some(value) = parsed else {
        println!("{body_text}");
        return Ok(());
    };
    let data = value.get("data").unwrap_or(&value);
    formatter(data);
    Ok(())
}

pub(crate) fn format_status(data: &Value) {
    print_kv(data, &["schema_version", "latest_event"]);
    if let Some(version) = data
        .get("server")
        .and_then(|s| s.get("version"))
        .and_then(Value::as_str)
    {
        println!("version: {version}");
    }
}

pub(crate) fn format_security(data: &Value) {
    let ok = data.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let count = data
        .get("auth_failure_count")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    println!("ok: {ok}");
    println!("auth_failures_total: {count}");
    if let Some(findings) = data.get("findings").and_then(Value::as_array) {
        if findings.is_empty() {
            println!("findings: (none)");
        } else {
            println!("findings:");
            for finding in findings {
                let severity = finding
                    .get("severity")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let code = finding.get("code").and_then(Value::as_str).unwrap_or("");
                let message = finding.get("message").and_then(Value::as_str).unwrap_or("");
                println!("  - [{severity}] {code}: {message}");
                if let Some(remediation) = finding.get("remediation").and_then(Value::as_str)
                    && !remediation.is_empty()
                {
                    println!("      hint: {remediation}");
                }
            }
        }
    }
}

pub(crate) fn format_deps(data: &Value) {
    println!("{}", serde_json::to_string_pretty(data).unwrap_or_default());
}

pub(crate) fn format_logs(data: &Value) {
    let Some(events) = data.get("events").and_then(Value::as_array) else {
        println!("(no events)");
        return;
    };
    if events.is_empty() {
        println!("(no events)");
        return;
    }
    for event in events {
        let created = event
            .get("created_at")
            .and_then(Value::as_str)
            .unwrap_or("");
        let level = event.get("level").and_then(Value::as_str).unwrap_or("");
        let source = event.get("source").and_then(Value::as_str).unwrap_or("");
        let kind = event.get("kind").and_then(Value::as_str).unwrap_or("");
        let message = event.get("message").and_then(Value::as_str).unwrap_or("");
        println!("{created} {level} {source} {kind} {message}");
    }
}

pub(crate) fn format_files_list(data: &Value) {
    let path = data.get("path").and_then(Value::as_str).unwrap_or("");
    println!("path: {path}");
    let Some(entries) = data.get("entries").and_then(Value::as_array) else {
        return;
    };
    for entry in entries {
        let name = entry.get("name").and_then(Value::as_str).unwrap_or("");
        let kind = entry.get("kind").and_then(Value::as_str).unwrap_or("");
        let size = entry
            .get("size")
            .and_then(Value::as_u64)
            .map(|s| s.to_string())
            .unwrap_or_else(|| "-".to_owned());
        println!("{kind:9} {size:>10} {name}");
    }
}

pub(crate) fn write_workspace_read(resp: &HttpResponse, json_mode: bool) -> Result<(), String> {
    let body_text = std::str::from_utf8(&resp.body).unwrap_or("");
    let value: Value =
        serde_json::from_str(body_text).map_err(|e| format!("response is not JSON: {e}"))?;
    if !(200..300).contains(&resp.status) || value.get("ok").and_then(Value::as_bool) == Some(false)
    {
        let code = value
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let message = value
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("(no message)");
        return Err(format!("HTTP {} {code}: {message}", resp.status));
    }
    if json_mode {
        println!(
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_default()
        );
        return Ok(());
    }
    let data = value.get("data").unwrap_or(&value);
    let encoding = data.get("encoding").and_then(Value::as_str).unwrap_or("");
    let content = data.get("content").and_then(Value::as_str).unwrap_or("");
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    if encoding == "base64" {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(content)
            .map_err(|e| format!("decode base64 content: {e}"))?;
        handle
            .write_all(&bytes)
            .map_err(|e| format!("write stdout: {e}"))?;
    } else {
        handle
            .write_all(content.as_bytes())
            .map_err(|e| format!("write stdout: {e}"))?;
    }
    handle.flush().map_err(|e| format!("flush stdout: {e}"))?;
    Ok(())
}

pub(crate) fn format_file_mutation(data: &Value) {
    print_kv(data, &["path", "size", "modified"]);
}

pub(crate) fn format_command(data: &Value) {
    // `/v1/commands` returns the row at submission time only (status is
    // typically `pending` or `running`); stdout is streamed via WebSocket on
    // the public API, not via this REST submit. Poll `/v1/commands/{id}` from
    // a follow-up call to observe completion.
    print_kv(data, &["id", "status", "command", "exit_status"]);
}

pub(crate) fn format_config_export(data: &Value) {
    if let Some(toml) = data.get("toml").and_then(Value::as_str) {
        print!("{toml}");
    } else {
        println!("{}", serde_json::to_string_pretty(data).unwrap_or_default());
    }
}

pub(crate) fn format_permissions(data: &Value) {
    let Some(perms) = data.get("permissions").and_then(Value::as_array) else {
        println!("(none)");
        return;
    };
    if perms.is_empty() {
        println!("(none)");
        return;
    }
    for perm in perms {
        let id = perm.get("id").and_then(Value::as_str).unwrap_or("");
        let source = perm.get("source").and_then(Value::as_str).unwrap_or("");
        let requester = perm.get("requester").and_then(Value::as_str).unwrap_or("");
        let created = perm.get("created_at").and_then(Value::as_str).unwrap_or("");
        println!("{created} {id} src={source} requester={requester}");
    }
}

pub(crate) fn format_ws_connections(data: &Value) {
    let Some(connections) = data.get("connections").and_then(Value::as_array) else {
        println!("(none)");
        return;
    };
    if connections.is_empty() {
        println!("(none)");
        return;
    }
    for connection in connections {
        let id = connection
            .get("connection_id")
            .and_then(Value::as_str)
            .unwrap_or("");
        let origin = connection
            .get("origin")
            .and_then(|origin| origin.get("origin_kind"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let topic_count = connection
            .get("topics")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0);
        println!("{id} origin={origin} topics={topic_count}");
    }
}

pub(crate) fn format_ws_sessions(data: &Value) {
    let Some(sessions) = data.get("sessions").and_then(Value::as_array) else {
        println!("(none)");
        return;
    };
    if sessions.is_empty() {
        println!("(none)");
        return;
    }
    for session in sessions {
        let id = session
            .get("session_id")
            .and_then(Value::as_str)
            .unwrap_or("");
        let count = session
            .get("connection_count")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        println!("{id} connections={count}");
    }
}

fn print_kv(data: &Value, keys: &[&str]) {
    for key in keys {
        if let Some(value) = data.get(*key) {
            let rendered = match value {
                Value::String(s) => s.clone(),
                _ => value.to_string(),
            };
            println!("{key}: {rendered}");
        }
    }
}
