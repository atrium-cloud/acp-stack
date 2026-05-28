//! Security self-check history error helpers (`security.*` namespace).

use http::StatusCode;

use super::StackError;

pub(super) fn error_code(err: &StackError) -> Option<&'static str> {
    use StackError::*;
    Some(match err {
        SecurityRunNotFound { .. } => "security.run_not_found",
        SecurityFindingDetailsCorrupt { .. } => "security.finding_details_corrupt",
        _ => return None,
    })
}

pub(super) fn public_message(err: &StackError) -> Option<String> {
    use StackError::*;
    Some(match err {
        SecurityRunNotFound { id } => format!("security run `{id}` was not found"),
        SecurityFindingDetailsCorrupt {
            run_id, ordinal, ..
        } => {
            format!(
                "security run `{run_id}` finding {ordinal} has unreadable `details_json` in the state database"
            )
        }
        _ => return None,
    })
}

pub(super) fn http_status(err: &StackError) -> Option<StatusCode> {
    use StackError::*;
    Some(match err {
        SecurityRunNotFound { .. } => StatusCode::NOT_FOUND,
        SecurityFindingDetailsCorrupt { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        _ => return None,
    })
}
