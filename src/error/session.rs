//! Session and prompt error helpers (`session.*`, `prompt.*` namespaces).

use http::StatusCode;
use serde_json::{Map, Value};

use super::StackError;
use crate::redaction::{bounded, redact_text, strip_serde_value_literals};

// CONSTANTS

/// Cap on `details.reason`; serde's text for a rejected content block can quote the caller's input.
const PROMPT_BODY_REASON_MAX_BYTES: usize = 1024;

pub(super) fn public_details(err: &StackError) -> Option<Map<String, Value>> {
    let reason = match err {
        StackError::PromptBodyInvalid(reason) => {
            let reason = strip_serde_value_literals(reason);
            bounded(&redact_text(&reason), PROMPT_BODY_REASON_MAX_BYTES).into_owned()
        }
        StackError::SessionCwdInvalid { reason, .. } => (*reason).to_owned(),
        _ => return None,
    };
    let mut details = Map::new();
    details.insert("reason".to_owned(), Value::String(reason));
    Some(details)
}

pub(super) fn error_code(err: &StackError) -> Option<&'static str> {
    use StackError::*;
    Some(match err {
        SessionNotFound { .. } => "session.not_found",
        SessionClosed { .. } => "session.closed",
        SessionDeleted { .. } => "session.deleted",
        SessionReattachUnsupported { .. } => "session.reattach_unsupported",
        SessionEventCursorUnknown { .. } => "session.event_cursor_unknown",
        PromptInFlight { .. } => "session.prompt_in_flight",
        PromptNotFound { .. } => "prompt.not_found",
        PromptSessionMismatch { .. } => "prompt.session_mismatch",
        PromptBodyEmpty => "prompt.body_empty",
        // The cwd rides in the same request bodies as the prompt and session payloads, so it
        // shares their invalid-body code; `details.reason` names the check that failed.
        PromptBodyInvalid(_) | SessionCwdInvalid { .. } => "prompt.body_invalid",
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
        SessionDeleted { id } => format!("session `{id}` was deleted"),
        SessionReattachUnsupported { id } => format!(
            "session `{id}` cannot be re-attached: the agent advertises neither `session/resume` nor `session/load`"
        ),
        SessionEventCursorUnknown {
            session_id,
            cursor_id,
        } => format!("session `{session_id}` has no event `{cursor_id}` to page after"),
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
        SessionCwdInvalid { .. } => "session cwd is invalid".to_owned(),
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
        SessionNotFound { .. } | PromptNotFound { .. } | SessionEventCursorUnknown { .. } => {
            StatusCode::NOT_FOUND
        }
        SessionClosed { .. } | PromptInFlight { .. } | PromptSessionMismatch { .. } => {
            StatusCode::CONFLICT
        }
        SessionTargetRenameConflict { .. } => StatusCode::CONFLICT,
        SessionDeleted { .. } => StatusCode::GONE,
        SessionReattachUnsupported { .. } => StatusCode::NOT_IMPLEMENTED,
        PromptBodyEmpty
        | PromptBodyInvalid(_)
        | SessionCwdInvalid { .. }
        | PromptUnsupportedModality { .. } => StatusCode::BAD_REQUEST,
        _ => return None,
    })
}
