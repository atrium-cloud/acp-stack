//! Runtime workspace-access error helpers (`workspace.*` for path/IO ops).

use http::StatusCode;

use super::StackError;

/// `Display` body for `WorkspaceCommandFailed`, shared with the
/// `workspace_source` domain's public message so the two renderings of a
/// failed command cannot drift.
pub(super) fn workspace_command_failed_message(
    command: &str,
    exit: Option<i32>,
    stderr_tail: &str,
) -> String {
    match exit {
        Some(code) => format!("`{command}` exited with status {code}: {stderr_tail}"),
        None => format!("`{command}` exited without a status: {stderr_tail}"),
    }
}

pub(super) fn error_code(err: &StackError) -> Option<&'static str> {
    use StackError::*;
    Some(match err {
        WorkspacePathInvalid { .. } => "workspace.path_invalid",
        WorkspaceSymlinkEscape { .. } => "workspace.symlink_escape",
        WorkspaceNotFound { .. } | WorkspaceParentNotFound { .. } => "workspace.not_found",
        WorkspaceTooLarge { .. } => "workspace.too_large",
        WorkspaceUploadInvalid { .. } => "workspace.upload_invalid",
        WorkspaceIo { .. } => "workspace.io_failed",
        WorkspaceEncodingInvalid { .. } => "workspace.encoding_invalid",
        _ => return None,
    })
}

pub(super) fn public_message(err: &StackError) -> Option<String> {
    use StackError::*;
    Some(match err {
        WorkspacePathInvalid { reason, .. } => format!("workspace path is invalid: {reason}"),
        WorkspaceSymlinkEscape { .. } => {
            "workspace path resolves outside the workspace root".to_owned()
        }
        WorkspaceNotFound { requested } => format!("workspace path `{requested}` was not found"),
        WorkspaceParentNotFound { requested } => {
            format!("workspace parent directory for `{requested}` was not found")
        }
        WorkspaceTooLarge { limit } => {
            format!("workspace file exceeds the {limit}-byte size limit")
        }
        WorkspaceUploadInvalid { reason } => format!("workspace upload is invalid: {reason}"),
        WorkspaceIo { .. } => "workspace I/O failed".to_owned(),
        WorkspaceEncodingInvalid { reason } => {
            format!("workspace file encoding is invalid: {reason}")
        }
        _ => return None,
    })
}

pub(super) fn http_status(err: &StackError) -> Option<StatusCode> {
    use StackError::*;
    Some(match err {
        WorkspacePathInvalid { .. }
        | WorkspaceSymlinkEscape { .. }
        | WorkspaceUploadInvalid { .. }
        | WorkspaceEncodingInvalid { .. } => StatusCode::BAD_REQUEST,
        WorkspaceNotFound { .. } | WorkspaceParentNotFound { .. } => StatusCode::NOT_FOUND,
        WorkspaceTooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
        WorkspaceIo { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        _ => return None,
    })
}
