//! Declares the MCP tools exposed by `acpctl mcp serve`. Each tool maps
//! one-to-one onto an allowlisted UDS route mounted by
//! `src/local_listener/router.rs`. The exact set is the deny-list contract
//! described in `docs/specs/acpctl/acpctl.md` — any deviation here would
//! breach Phase 3 acceptance.

use std::sync::Arc;

use rmcp::model::{JsonObject, Tool};
use serde_json::{Value, json};

/// Stable list of tool names. Sorted into the order the `acpctl` CLI uses so
/// `tools/list` is easy to scan against the spec.
pub(crate) const TOOL_NAMES: &[&str] = &[
    "status",
    "security_check",
    "deps_check",
    "logs_query",
    "workspace_list",
    "workspace_read",
    "workspace_write",
    "command_run",
    "config_export",
    "permissions_pending",
    "ws_connections",
    "ws_sessions",
];

/// Build every tool definition. Cheap enough to call on each `tools/list`
/// since rmcp passes them by value.
pub(crate) fn all() -> Vec<Tool> {
    TOOL_NAMES
        .iter()
        .copied()
        .map(|name| build(name).expect("TOOL_NAMES must match build()"))
        .collect()
}

/// Build a single tool definition by name. Returns `None` for names not in
/// `TOOL_NAMES` so callers can implement `ServerHandler::get_tool`.
pub(crate) fn build(name: &str) -> Option<Tool> {
    let (description, schema) = match name {
        "status" => (
            "Return the runtime status envelope (schema version, latest event).".to_owned(),
            schema_no_args(),
        ),
        "security_check" => (
            "Run the daemon's security self-check and return its findings.".to_owned(),
            schema_no_args(),
        ),
        "deps_check" => (
            "Re-run the agent dependency check and return the latest report.".to_owned(),
            schema_no_args(),
        ),
        "logs_query" => (
            "Query the local event log with optional time, kind, level and session filters."
                .to_owned(),
            schema_logs_query(),
        ),
        "workspace_list" => (
            "List entries under a workspace directory.".to_owned(),
            schema_path_arg(),
        ),
        "workspace_read" => (
            "Read a workspace file. Returns text content for utf-8 files or base64 otherwise."
                .to_owned(),
            schema_path_arg(),
        ),
        "workspace_write" => (
            "Atomically write to a workspace file. `encoding` is 'utf8' (default) or 'base64'."
                .to_owned(),
            schema_workspace_write(),
        ),
        "command_run" => (
            "Submit a shell command to the mediated command gateway. Returns the submission \
             envelope (including a command id) before execution completes — the gateway runs \
             asynchronously. Subsequent output lands as `command.*` events in the local log; \
             call `logs_query` with `kind = \"command.\"` to fetch them (prefix match)."
                .to_owned(),
            schema_command_run(),
        ),
        "config_export" => (
            "Export the current config as TOML with secret references only.".to_owned(),
            schema_no_args(),
        ),
        "permissions_pending" => (
            "List pending permission requests awaiting operator decision.".to_owned(),
            schema_permissions_pending(),
        ),
        "ws_connections" => (
            "Return sanitized live WebSocket connection state.".to_owned(),
            schema_no_args(),
        ),
        "ws_sessions" => (
            "Return unique session IDs with live WebSocket subscriber counts.".to_owned(),
            schema_no_args(),
        ),
        _ => return None,
    };

    Some(Tool::new(name.to_owned(), description, Arc::new(schema)))
}

fn schema_no_args() -> JsonObject {
    object_schema(json!({}), &[])
}

fn schema_path_arg() -> JsonObject {
    object_schema(
        json!({
            "path": {
                "type": "string",
                "description": "Workspace-relative path. Must stay inside the workspace root.",
            }
        }),
        &["path"],
    )
}

fn schema_workspace_write() -> JsonObject {
    object_schema(
        json!({
            "path": {
                "type": "string",
                "description": "Workspace-relative path. Must stay inside the workspace root.",
            },
            "content": {
                "type": "string",
                "description": "File body. If `encoding` is 'base64', this must be base64-encoded bytes.",
            },
            "encoding": {
                "type": "string",
                "enum": ["utf8", "base64"],
                "description": "How `content` is encoded. Default is 'utf8'.",
            }
        }),
        &["path", "content"],
    )
}

fn schema_command_run() -> JsonObject {
    object_schema(
        json!({
            "command": {
                "type": "string",
                "description": "Shell command to run through the mediated gateway.",
            },
            "cwd": {
                "type": "string",
                "description": "Optional working directory; must remain inside the workspace root.",
            },
            "timeout": {
                "type": "string",
                "description": "Optional duration (e.g. `30s`, `5m`).",
            }
        }),
        &["command"],
    )
}

fn schema_logs_query() -> JsonObject {
    object_schema(
        json!({
            "since": {
                "type": "string",
                "description": "Lower bound. Accepts RFC3339 or a duration suffix (`30m`, `1h`, `2d`); duration suffixes are resolved to an absolute RFC3339 timestamp by this tool before being sent to the daemon.",
            },
            "until": {
                "type": "string",
                "description": "Upper bound. Same accepted formats as `since`.",
            },
            "kind": {
                "type": "string",
                "description": "Event kind. A trailing dot matches as a prefix (e.g. `api.`).",
            },
            "level": { "type": "string", "description": "Filter by log level." },
            "session_id": { "type": "string", "description": "Filter by session id." },
            "after": {
                "type": "string",
                "description": "Pagination cursor; the last seen event id.",
            },
            "limit": {
                "type": "integer",
                "minimum": 1,
                "maximum": 1000,
                "description": "Maximum rows to return. Default 200. The daemon clamps to 1000 — `MAX_LOGS_LIMIT` in `src/api/routes/logs.rs`.",
            }
        }),
        &[],
    )
}

fn schema_permissions_pending() -> JsonObject {
    object_schema(
        json!({
            "limit": {
                "type": "integer",
                "minimum": 1,
                "maximum": 1000,
                "description": "Maximum rows to return. Default 200. The daemon clamps to 1000 — `MAX_LOGS_LIMIT` in `src/api/routes/logs.rs`.",
            }
        }),
        &[],
    )
}

/// Assemble a JSON Schema object with `type: object`, the given properties,
/// and a `required` array. Centralized so every schema has consistent shape.
fn object_schema(properties: Value, required: &[&str]) -> JsonObject {
    let mut schema = JsonObject::new();
    schema.insert("type".to_owned(), Value::String("object".to_owned()));
    schema.insert("properties".to_owned(), properties);
    schema.insert(
        "required".to_owned(),
        Value::Array(
            required
                .iter()
                .map(|s| Value::String((*s).to_owned()))
                .collect(),
        ),
    );
    schema.insert("additionalProperties".to_owned(), Value::Bool(false));
    schema
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_tools_have_unique_names() {
        let tools = all();
        let mut names: Vec<_> = tools.iter().map(|t| t.name.to_string()).collect();
        names.sort();
        let original_len = names.len();
        names.dedup();
        assert_eq!(original_len, names.len(), "duplicate tool names");
    }

    #[test]
    fn tool_set_is_exactly_the_spec_list() {
        // The spec deny list is enforced by what is NOT here. If you add a tool,
        // ensure it does not breach the deny rules in
        // `docs/specs/acpctl/acpctl.md` and `docs/todos/phase_3.md:64-69`.
        let tools = all();
        let mut got: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
        let mut expected: Vec<String> = TOOL_NAMES.iter().map(|s| (*s).to_owned()).collect();
        got.sort();
        expected.sort();
        assert_eq!(got, expected);
    }

    #[test]
    fn denied_tools_are_absent() {
        // Sanity check: secret/key/admin operations must never surface as MCP
        // tools. If a future commit adds one of these, the test fails loudly.
        let names: Vec<String> = all().iter().map(|t| t.name.to_string()).collect();
        for forbidden in [
            "secrets_list",
            "secrets_get",
            "secrets_set",
            "secrets_delete",
            "auth_rotate",
            "api_key_rotate",
            "permissions_approve",
            "permissions_deny",
            "config_import",
            "agent_install",
            "agent_start",
            "agent_stop",
        ] {
            assert!(
                !names.iter().any(|n| n == forbidden),
                "tool {forbidden} must not be exposed"
            );
        }
    }
}
