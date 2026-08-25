//! Session and prompt error helpers (`session.*`, `prompt.*` namespaces).

use http::StatusCode;

use super::StackError;

pub(super) fn error_code(err: &StackError) -> Option<&'static str> {
    use StackError::*;
    Some(match err {
        SessionNotFound { .. } => "session.not_found",
        SessionClosed { .. } => "session.closed",
        SessionNotActive { .. } => "session.not_active",
        PromptInFlight { .. } => "session.prompt_in_flight",
        PromptNotFound { .. } => "prompt.not_found",
        PromptSessionMismatch { .. } => "prompt.session_mismatch",
        PromptBodyEmpty => "prompt.body_empty",
        PromptBodyInvalid(_) => "prompt.body_invalid",
        PromptUnsupportedModality { .. } => "prompt.unsupported_modality",
        SessionTargetRenameConflict { .. } => "session.target_rename_conflict",
        _ => return None,
    })
}

pub(super) fn public_message(err: &StackError) -> Option<String> {
    use StackError::*;
    Some(match err {
        SessionNotFound { id } => format!("session `{id}` was not found"),
        SessionClosed { id } => format!("session `{id}` is closed"),
        SessionNotActive { id, status } => {
            format!("session `{id}` is {status}; load or resume it before prompting")
        }
        PromptInFlight { session_id } => {
            format!("session `{session_id}` already has a prompt in flight")
        }
        PromptNotFound { id } => format!("prompt `{id}` was not found"),
        PromptSessionMismatch {
            session_id,
            prompt_id,
        } => format!("session `{session_id}` does not own prompt `{prompt_id}`"),
        PromptBodyEmpty => "prompt body must include at least one content block".to_owned(),
        PromptBodyInvalid(_) => "prompt body is not valid ACP content".to_owned(),
        PromptUnsupportedModality { model, modality } => {
            format!("model `{model}` does not support `{modality}` prompt input")
        }
        SessionTargetRenameConflict {
            old_target_id,
            new_target_id,
            count,
        } => format!(
            "cannot move {count} session(s) from `{old_target_id}` to `{new_target_id}`: the new target already has session(s) with the same agent session id"
        ),
        _ => return None,
    })
}

pub(super) fn http_status(err: &StackError) -> Option<StatusCode> {
    use StackError::*;
    Some(match err {
        SessionNotFound { .. } | PromptNotFound { .. } => StatusCode::NOT_FOUND,
        SessionClosed { .. }
        | SessionNotActive { .. }
        | PromptInFlight { .. }
        | PromptSessionMismatch { .. } => StatusCode::CONFLICT,
        SessionTargetRenameConflict { .. } => StatusCode::CONFLICT,
        PromptBodyEmpty | PromptBodyInvalid(_) | PromptUnsupportedModality { .. } => {
            StatusCode::BAD_REQUEST
        }
        _ => return None,
    })
}
