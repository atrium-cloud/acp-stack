//! Permission-store error helpers (`permission.*`), plus the `config.invalid`
//! variants from the permissions / MCP / dependencies config domain.

use http::StatusCode;

use super::StackError;

pub(super) fn error_code(err: &StackError) -> Option<&'static str> {
    use StackError::*;
    Some(match err {
        PermissionNotFound { .. } => "permission.not_found",
        InvalidPermissionTransition { .. } => "permission.invalid_transition",
        InvalidTimeoutAction
        | InvalidAcpPromptAction
        | InvalidTrustedProxy { .. }
        | InvalidMcpServer { .. }
        | DuplicateMcpServer { .. }
        | DependencyMissingName { .. }
        | DuplicateDependency { .. } => "config.invalid",
        _ => return None,
    })
}

pub(super) fn public_message(err: &StackError) -> Option<String> {
    use StackError::*;
    Some(match err {
        PermissionNotFound { id } => format!("permission `{id}` was not found"),
        InvalidPermissionTransition { id, from, to } => {
            format!("permission `{id}` cannot transition from `{from}` to `{to}`")
        }
        InvalidTimeoutAction => {
            "permissions.timeout_action must be one of deny, approve".to_owned()
        }
        InvalidAcpPromptAction => {
            "permissions.acp_prompt_action must be one of ask, approve".to_owned()
        }
        InvalidTrustedProxy { value } => {
            format!("security.http.trusted_proxies entry `{value}` is not a valid IP address")
        }
        InvalidMcpServer { name, reason } => {
            format!("mcp.servers entry `{name}` is invalid: {reason}")
        }
        DuplicateMcpServer { name } => format!("mcp.servers contains duplicate name `{name}`"),
        DependencyMissingName { category } => {
            format!("dependencies.{category} entry has empty name")
        }
        DuplicateDependency { category, name } => {
            format!("dependencies.{category} contains duplicate name `{name}`")
        }
        _ => return None,
    })
}

pub(super) fn http_status(err: &StackError) -> Option<StatusCode> {
    use StackError::*;
    Some(match err {
        PermissionNotFound { .. } => StatusCode::NOT_FOUND,
        // The request is well-formed but the row is already terminal, so 409
        // reads as "decision race lost", not "malformed request".
        InvalidPermissionTransition { .. } => StatusCode::CONFLICT,
        InvalidTimeoutAction
        | InvalidAcpPromptAction
        | InvalidTrustedProxy { .. }
        | InvalidMcpServer { .. }
        | DuplicateMcpServer { .. }
        | DependencyMissingName { .. }
        | DuplicateDependency { .. } => StatusCode::BAD_REQUEST,
        _ => return None,
    })
}
