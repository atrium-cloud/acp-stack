//! Command-gateway error helpers (`command.*` namespace).

use http::StatusCode;

use super::StackError;

pub(super) fn error_code(err: &StackError) -> Option<&'static str> {
    use StackError::*;
    Some(match err {
        CommandNotFound { .. } => "command.not_found",
        CommandDenied { .. } => "command.denied",
        CommandCwdOutsideWorkspace { .. } => "command.cwd_outside_workspace",
        CommandEnvNotAllowed { .. } => "command.env_not_allowed",
        CommandSpawnFailed { .. } => "command.spawn_failed",
        CommandTimeout => "command.timeout",
        _ => return None,
    })
}

pub(super) fn public_message(err: &StackError) -> Option<String> {
    use StackError::*;
    Some(match err {
        CommandNotFound { id } => format!("command `{id}` was not found"),
        CommandDenied { reason } => format!("command rejected by policy: {reason}"),
        CommandCwdOutsideWorkspace { requested } => {
            format!("command cwd `{requested}` resolves outside the workspace root")
        }
        CommandEnvNotAllowed { name } => {
            format!("command env variable `{name}` is not on commands.env_allowlist")
        }
        CommandSpawnFailed { .. } => "failed to spawn command subprocess".to_owned(),
        CommandTimeout => {
            "command timed out before the subprocess produced an exit status".to_owned()
        }
        _ => return None,
    })
}

pub(super) fn http_status(err: &StackError) -> Option<StatusCode> {
    use StackError::*;
    Some(match err {
        CommandDenied { .. } | CommandCwdOutsideWorkspace { .. } | CommandEnvNotAllowed { .. } => {
            StatusCode::BAD_REQUEST
        }
        CommandNotFound { .. } => StatusCode::NOT_FOUND,
        CommandSpawnFailed { .. } | CommandTimeout => StatusCode::INTERNAL_SERVER_ERROR,
        _ => return None,
    })
}
