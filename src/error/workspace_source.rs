//! Workspace-source materialization error helpers.
//!
//! Covers the `workspace.code_sources` / `workspace.data_sources` config-time
//! validators and the materialization pipeline that runs during `acps init`.
//! Variants here predominantly use `workspace.*` error codes plus the catch-all
//! `config.invalid` for the code/data-source validators.

use http::StatusCode;

use super::{StackError, workspace_command_failed_message};

pub(super) fn error_code(err: &StackError) -> Option<&'static str> {
    use StackError::*;
    Some(match err {
        WorkspaceCodeSourceInvalid { .. } | WorkspaceDataSourceInvalid { .. } => "config.invalid",
        WorkspaceUploadsNotUnderRoot => "config.invalid",
        WorkspaceDestinationNotEmpty { .. } => "workspace.destination_not_empty",
        WorkspaceDestinationOutsideRoot { .. } => "workspace.destination_outside_root",
        WorkspaceMaterializeFailed { .. } => "workspace.materialize_failed",
        WorkspaceCommandFailed { .. } => "workspace.command_failed",
        _ => return None,
    })
}

pub(super) fn public_message(err: &StackError) -> Option<String> {
    use StackError::*;
    Some(match err {
        WorkspaceCodeSourceInvalid { index, reason } => {
            format!("workspace.code_sources[{index}]: {reason}")
        }
        WorkspaceDataSourceInvalid { index, reason } => {
            format!("workspace.data_sources[{index}]: {reason}")
        }
        WorkspaceUploadsNotUnderRoot => {
            "workspace.uploads must be inside workspace.root".to_owned()
        }
        WorkspaceDestinationNotEmpty { dest } => {
            format!("workspace destination `{dest}` is not empty")
        }
        WorkspaceDestinationOutsideRoot { dest, root } => {
            format!("workspace destination `{dest}` is outside workspace.root `{root}`")
        }
        WorkspaceMaterializeFailed { reason } => {
            format!("workspace materialization failed: {reason}")
        }
        WorkspaceCommandFailed {
            command,
            exit,
            stderr_tail,
        } => workspace_command_failed_message(command, *exit, stderr_tail),
        _ => return None,
    })
}

pub(super) fn http_status(err: &StackError) -> Option<StatusCode> {
    use StackError::*;
    Some(match err {
        WorkspaceCodeSourceInvalid { .. } | WorkspaceDataSourceInvalid { .. } => {
            StatusCode::BAD_REQUEST
        }
        WorkspaceUploadsNotUnderRoot => StatusCode::BAD_REQUEST,
        WorkspaceDestinationNotEmpty { .. } | WorkspaceDestinationOutsideRoot { .. } => {
            StatusCode::CONFLICT
        }
        WorkspaceMaterializeFailed { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        WorkspaceCommandFailed { .. } => StatusCode::BAD_GATEWAY,
        _ => return None,
    })
}
