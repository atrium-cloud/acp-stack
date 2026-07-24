//! Extension-seam error helpers (`extensions.*` namespace).

use http::StatusCode;

use super::StackError;

pub(super) fn error_code(err: &StackError) -> Option<&'static str> {
    use StackError::*;
    Some(match err {
        ExtensionNamespaceUnknown { .. } => "extensions.not_found",
        ExtensionRevisionConflict { .. } => "extensions.revision_conflict",
        ExtensionStateOwnership { .. } => "extensions.state_ownership",
        _ => return None,
    })
}

pub(super) fn public_message(err: &StackError) -> Option<String> {
    use StackError::*;
    Some(match err {
        ExtensionNamespaceUnknown { name } => {
            format!("no managed-state extension named `{name}` is declared")
        }
        ExtensionRevisionConflict { namespace, reason } => {
            format!("managed-state apply for `{namespace}` conflicts: {reason}")
        }
        ExtensionStateOwnership {
            namespace,
            provider_id,
            reason,
        } => format!(
            "managed-state namespace `{namespace}` cannot take ownership of provider `{provider_id}`: {reason}"
        ),
        _ => return None,
    })
}

pub(super) fn http_status(err: &StackError) -> Option<StatusCode> {
    use StackError::*;
    Some(match err {
        ExtensionNamespaceUnknown { .. } => StatusCode::NOT_FOUND,
        // A revision-ordering conflict is a concurrency condition the
        // orchestrator resolves by retrying with fresh state, not a payload
        // defect; keep it distinct from 400.
        ExtensionRevisionConflict { .. } => StatusCode::CONFLICT,
        ExtensionStateOwnership { .. } => StatusCode::BAD_REQUEST,
        _ => return None,
    })
}
