//! Translates an MCP tool call into a UDS HTTP request against the daemon's
//! local listener. By reusing the existing UDS, every tool call inherits the
//! daemon's `KeyKind::Local` tag and `source = "local"` event attribution —
//! we never reimplement permission or logging logic.

use std::path::Path;

use serde_json::{Map, Value};

use crate::client::{HttpResponse, request};
use crate::helpers::{resolve_time_bound, url_encode};

/// A tool call resolved to a concrete UDS HTTP request.
#[derive(Debug)]
pub(crate) struct UdsCall {
    pub(crate) method: &'static str,
    pub(crate) path: String,
    pub(crate) headers: Vec<(&'static str, &'static str)>,
    pub(crate) body: Option<Vec<u8>>,
}

/// Map a tool name + args object into a `UdsCall`. Returns `Err` for unknown
/// tools (the rmcp layer should never call us with one, but we defend) and for
/// missing/ill-typed required arguments.
pub(crate) fn build_call(name: &str, args: &Value) -> Result<UdsCall, String> {
    let args = if args.is_null() {
        &Value::Object(Map::new())
    } else {
        args
    };
    let args = args
        .as_object()
        .ok_or_else(|| format!("tool {name} arguments must be a JSON object"))?;

    match name {
        "status" => Ok(get("/v1/status".to_owned())),
        "security_check" => Ok(get("/v1/security/check".to_owned())),
        "deps_check" => Ok(post_json("/v1/deps/check".to_owned(), b"{}".to_vec())),
        "logs_query" => Ok(get(build_logs_query(args)?)),
        "workspace_list" => {
            let path = require_string(args, "path", "workspace_list")?;
            Ok(get(format!("/v1/files?path={}", url_encode(path))))
        }
        "workspace_read" => {
            let path = require_string(args, "path", "workspace_read")?;
            Ok(get(format!("/v1/files/content?path={}", url_encode(path))))
        }
        "workspace_write" => {
            let path = require_string(args, "path", "workspace_write")?;
            let content = require_string(args, "content", "workspace_write")?;
            let (encoding, content_field) = match optional_string(args, "encoding")? {
                Some("base64") => ("base64", content.to_owned()),
                Some("utf8") | None => ("utf8", content.to_owned()),
                Some(other) => {
                    return Err(format!(
                        "workspace_write `encoding` must be 'utf8' or 'base64', got {other:?}"
                    ));
                }
            };
            // If the caller asked for utf8 but the input is somehow not valid
            // utf-8 we still hand it to the server as-is; the server enforces.
            // Round-trip non-utf8 bytes via base64 if the caller didn't pick.
            let _ = encoding; // already used in body
            let body = serde_json::json!({
                "path": path,
                "encoding": encoding,
                "content": content_field,
            })
            .to_string();
            Ok(put_json("/v1/files/content".to_owned(), body.into_bytes()))
        }
        "command_run" => {
            let command = require_string(args, "command", "command_run")?;
            let mut body = Map::new();
            body.insert("command".to_owned(), Value::String(command.to_owned()));
            if let Some(cwd) = optional_string(args, "cwd")? {
                body.insert("cwd".to_owned(), Value::String(cwd.to_owned()));
            }
            if let Some(timeout) = optional_string(args, "timeout")? {
                body.insert("timeout".to_owned(), Value::String(timeout.to_owned()));
            }
            let text = Value::Object(body).to_string();
            Ok(post_json("/v1/commands".to_owned(), text.into_bytes()))
        }
        "config_export" => Ok(get("/v1/config/export".to_owned())),
        "permissions_pending" => {
            let limit = optional_u32(args, "limit")?.unwrap_or(200);
            Ok(get(format!("/v1/permissions/pending?limit={limit}")))
        }
        "ws_connections" => Ok(get("/v1/ws/connections".to_owned())),
        "ws_sessions" => Ok(get("/v1/ws/sessions".to_owned())),
        other => Err(format!("unknown tool {other:?}")),
    }
}

/// Execute a `UdsCall` against the daemon's local socket and return either the
/// parsed JSON success body or a structured error.
pub(crate) async fn execute(socket: &Path, call: UdsCall) -> Result<DispatchResult, String> {
    let response = request(socket, call.method, &call.path, &call.headers, call.body).await?;
    Ok(map_response(response))
}

/// What a tool dispatch produced. `Ok` carries the parsed JSON body; `Err`
/// carries the status code + a descriptive message extracted from the daemon's
/// envelope when possible.
pub(crate) enum DispatchResult {
    Ok(Value),
    Err { status: u16, message: String },
}

fn map_response(response: HttpResponse) -> DispatchResult {
    let body = match parse_body(&response.body) {
        Ok(value) => value,
        Err(err) => {
            return DispatchResult::Err {
                status: response.status,
                message: format!("daemon returned a non-JSON body ({err})"),
            };
        }
    };
    if (200..300).contains(&response.status) {
        DispatchResult::Ok(body)
    } else {
        let message = extract_error_message(&body)
            .unwrap_or_else(|| format!("daemon returned HTTP {}", response.status));
        DispatchResult::Err {
            status: response.status,
            message,
        }
    }
}

fn parse_body(raw: &[u8]) -> Result<Value, String> {
    if raw.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_slice(raw).map_err(|err| err.to_string())
}

fn extract_error_message(body: &Value) -> Option<String> {
    let envelope = body.get("error")?;
    let message = envelope.get("message").and_then(Value::as_str)?;
    let code = envelope.get("code").and_then(Value::as_str);
    Some(match code {
        Some(code) => format!("{code}: {message}"),
        None => message.to_owned(),
    })
}

fn get(path: String) -> UdsCall {
    UdsCall {
        method: "GET",
        path,
        headers: Vec::new(),
        body: None,
    }
}

fn post_json(path: String, body: Vec<u8>) -> UdsCall {
    UdsCall {
        method: "POST",
        path,
        headers: vec![("content-type", "application/json")],
        body: Some(body),
    }
}

fn put_json(path: String, body: Vec<u8>) -> UdsCall {
    UdsCall {
        method: "PUT",
        path,
        headers: vec![("content-type", "application/json")],
        body: Some(body),
    }
}

fn build_logs_query(args: &Map<String, Value>) -> Result<String, String> {
    let mut path = String::from("/v1/logs/events?");
    let mut sep = "";
    let mut push = |key: &str, value: &str| {
        path.push_str(sep);
        path.push_str(key);
        path.push('=');
        path.push_str(&url_encode(value));
        sep = "&";
    };
    let limit = optional_u32(args, "limit")?.unwrap_or(200);
    push("limit", &limit.to_string());
    // The server's `/v1/logs/events` compares timestamps lexically and does
    // not parse duration suffixes itself. Resolve `30m` / `1h` / `2d`
    // client-side so the tool's documented suffix support is honest.
    if let Some(value) = optional_string(args, "since")? {
        let resolved = resolve_time_bound(value, "since")?;
        push("since", &resolved);
    }
    if let Some(value) = optional_string(args, "until")? {
        let resolved = resolve_time_bound(value, "until")?;
        push("until", &resolved);
    }
    if let Some(value) = optional_string(args, "kind")? {
        push("kind", value);
    }
    if let Some(value) = optional_string(args, "level")? {
        push("level", value);
    }
    if let Some(value) = optional_string(args, "session_id")? {
        push("session_id", value);
    }
    if let Some(value) = optional_string(args, "after")? {
        push("after", value);
    }
    Ok(path)
}

fn require_string<'a>(
    args: &'a Map<String, Value>,
    field: &str,
    tool: &str,
) -> Result<&'a str, String> {
    args.get(field)
        .ok_or_else(|| format!("tool {tool}: missing required field {field:?}"))?
        .as_str()
        .ok_or_else(|| format!("tool {tool}: field {field:?} must be a string"))
}

fn optional_string<'a>(
    args: &'a Map<String, Value>,
    field: &str,
) -> Result<Option<&'a str>, String> {
    match args.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_str()
            .map(Some)
            .ok_or_else(|| format!("field {field:?} must be a string")),
    }
}

fn optional_u32(args: &Map<String, Value>, field: &str) -> Result<Option<u32>, String> {
    match args.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let n = value
                .as_u64()
                .ok_or_else(|| format!("field {field:?} must be a non-negative integer"))?;
            if n > u32::MAX as u64 {
                return Err(format!("field {field:?} exceeds u32::MAX"));
            }
            Ok(Some(n as u32))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn build_call_status_is_get() {
        let call = build_call("status", &Value::Null).unwrap();
        assert_eq!(call.method, "GET");
        assert_eq!(call.path, "/v1/status");
        assert!(call.body.is_none());
    }

    #[test]
    fn build_call_deps_check_is_empty_post() {
        let call = build_call("deps_check", &Value::Null).unwrap();
        assert_eq!(call.method, "POST");
        assert_eq!(call.body.as_deref(), Some(b"{}".as_slice()));
    }

    #[test]
    fn build_call_logs_query_encodes_filters() {
        let args = json!({"since": "2026-01-01T00:00:00Z", "kind": "api.", "limit": 50});
        let call = build_call("logs_query", &args).unwrap();
        // The since value is resolved to an RFC3339 normalized form before
        // being URL-encoded, so only check the year prefix + that the
        // request line contains a since= clause at all.
        assert!(call.path.contains("since=2026-01-01"));
        assert!(call.path.contains("kind=api."));
        assert!(call.path.contains("limit=50"));
    }

    #[test]
    fn build_call_logs_query_resolves_duration_suffix() {
        let args = json!({"since": "30m", "limit": 5});
        let call = build_call("logs_query", &args).unwrap();
        // Must NOT pass `30m` verbatim — that would compare lexically to
        // real timestamps and silently misfilter. After resolution the path
        // should contain a real year-prefixed RFC3339 timestamp.
        assert!(
            !call.path.contains("since=30m"),
            "since=30m was not resolved to RFC3339: {}",
            call.path
        );
        assert!(
            call.path.contains("since=20"),
            "expected year prefix in {}",
            call.path
        );
    }

    #[test]
    fn build_call_logs_query_rejects_invalid_since() {
        let args = json!({"since": "yesterday"});
        let err = build_call("logs_query", &args).unwrap_err();
        assert!(
            err.contains("since"),
            "expected error about since field, got {err}"
        );
    }

    #[test]
    fn build_call_workspace_write_assembles_body() {
        let args = json!({"path": "notes.md", "content": "hi", "encoding": "utf8"});
        let call = build_call("workspace_write", &args).unwrap();
        assert_eq!(call.method, "PUT");
        let body: Value = serde_json::from_slice(call.body.as_ref().unwrap()).unwrap();
        assert_eq!(body["path"], "notes.md");
        assert_eq!(body["content"], "hi");
        assert_eq!(body["encoding"], "utf8");
    }

    #[test]
    fn build_call_command_run_includes_optional_fields() {
        let args = json!({"command": "ls", "cwd": "/tmp", "timeout": "5s"});
        let call = build_call("command_run", &args).unwrap();
        let body: Value = serde_json::from_slice(call.body.as_ref().unwrap()).unwrap();
        assert_eq!(body["command"], "ls");
        assert_eq!(body["cwd"], "/tmp");
        assert_eq!(body["timeout"], "5s");
    }

    #[test]
    fn build_call_unknown_tool_errors() {
        let err = build_call("secrets_get", &Value::Null).unwrap_err();
        assert!(err.contains("unknown tool"));
    }

    #[test]
    fn build_call_workspace_read_requires_path() {
        let err = build_call("workspace_read", &json!({})).unwrap_err();
        assert!(err.contains("path"));
    }

    #[test]
    fn map_response_extracts_envelope_error() {
        let raw = b"{\"error\":{\"code\":\"forbidden\",\"message\":\"nope\"}}";
        let resp = HttpResponse {
            status: 403,
            body: raw.to_vec(),
        };
        match map_response(resp) {
            DispatchResult::Err { status, message } => {
                assert_eq!(status, 403);
                assert!(message.contains("forbidden"));
                assert!(message.contains("nope"));
            }
            DispatchResult::Ok(_) => panic!("expected error"),
        }
    }

    #[test]
    fn map_response_passes_through_success() {
        let raw = b"{\"hello\":1}";
        let resp = HttpResponse {
            status: 200,
            body: raw.to_vec(),
        };
        match map_response(resp) {
            DispatchResult::Ok(value) => assert_eq!(value["hello"], 1),
            DispatchResult::Err { .. } => panic!("expected success"),
        }
    }
}
