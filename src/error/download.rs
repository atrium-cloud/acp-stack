//! HTTPS download error helpers (`download.*` namespace).

use http::StatusCode;

use super::StackError;

pub(super) fn error_code(err: &StackError) -> Option<&'static str> {
    use StackError::*;
    Some(match err {
        SafeDownloadTooLarge { .. } => "download.too_large",
        SafeDownloadInsecureRedirect { .. } => "download.insecure_redirect",
        SafeDownloadHttpStatus { .. } => "download.http_status",
        SafeDownloadFailed { .. } => "download.failed",
        SafeDownloadChecksumMismatch { .. } => "download.checksum_mismatch",
        _ => return None,
    })
}

pub(super) fn public_message(err: &StackError) -> Option<String> {
    use StackError::*;
    Some(match err {
        SafeDownloadTooLarge { limit } => {
            format!("download exceeded the {limit}-byte size limit")
        }
        SafeDownloadInsecureRedirect { url } => {
            format!("download URL `{url}` is not allowed (only https:// is permitted)")
        }
        SafeDownloadHttpStatus { url, status } => {
            format!("download from {url} failed with HTTP status {status}")
        }
        SafeDownloadFailed { url, reason } => format!("download from {url} failed: {reason}"),
        SafeDownloadChecksumMismatch { expected, actual } => {
            format!("downloaded content sha256 mismatch: expected {expected}, got {actual}")
        }
        _ => return None,
    })
}

pub(super) fn http_status(err: &StackError) -> Option<StatusCode> {
    use StackError::*;
    Some(match err {
        SafeDownloadTooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
        SafeDownloadInsecureRedirect { .. } => StatusCode::BAD_REQUEST,
        SafeDownloadHttpStatus { .. }
        | SafeDownloadFailed { .. }
        | SafeDownloadChecksumMismatch { .. } => StatusCode::BAD_GATEWAY,
        _ => return None,
    })
}
