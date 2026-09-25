//! Managed Node.js runtime error helpers (`node_runtime.*` namespace).

use http::StatusCode;

use super::StackError;

pub(super) fn error_code(err: &StackError) -> Option<&'static str> {
    use StackError::*;
    Some(match err {
        NodeRuntimeChecksumMismatch { .. } => "node_runtime.checksum_mismatch",
        NodeRuntimeInstallFailed { .. } => "node_runtime.install_failed",
        _ => return None,
    })
}

pub(super) fn public_message(err: &StackError) -> Option<String> {
    use StackError::*;
    Some(match err {
        NodeRuntimeChecksumMismatch { archive, .. } => {
            format!("managed Node.js archive `{archive}` failed sha256 verification")
        }
        // Reasons carry local paths and transport or subprocess error text.
        NodeRuntimeInstallFailed { .. } => "managed Node.js runtime install failed".to_owned(),
        _ => return None,
    })
}

pub(super) fn http_status(err: &StackError) -> Option<StatusCode> {
    use StackError::*;
    Some(match err {
        NodeRuntimeChecksumMismatch { .. } => StatusCode::BAD_GATEWAY,
        NodeRuntimeInstallFailed { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        _ => return None,
    })
}
