//! HTTPS download error helpers (`download.*` namespace).

use http::StatusCode;

use super::StackError;

/// Public rendering of an operator-supplied download URL. Userinfo, query, and
/// fragment can carry credentials or presigning tokens, so only scheme, host,
/// and path may reach the envelope. `None` when the caller recorded no URL or
/// the string does not parse; match arms then fall back to a static phrase
/// rather than echoing the raw string.
fn sanitized_download_url(url: &str) -> Option<String> {
    if url.trim().is_empty() {
        return None;
    }
    let mut parsed = reqwest::Url::parse(url).ok()?;
    parsed.set_username("").ok()?;
    parsed.set_password(None).ok()?;
    parsed.set_query(None);
    parsed.set_fragment(None);
    let rendered = parsed.to_string();
    // `Url` serializes an empty path as `/`; drop it so a bare origin reads as given.
    if parsed.path() == "/" {
        return Some(rendered.trim_end_matches('/').to_owned());
    }
    Some(rendered)
}

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
        SafeDownloadInsecureRedirect { url } => match sanitized_download_url(url) {
            Some(sanitized) => {
                format!("download URL `{sanitized}` is not allowed (only https:// is permitted)")
            }
            None => "download URL is not allowed (only https:// is permitted)".to_owned(),
        },
        // `body` is upstream response text; the URL and status are what the API may carry.
        SafeDownloadHttpStatus { url, status, .. } => match sanitized_download_url(url) {
            Some(sanitized) => {
                format!("download from {sanitized} failed with HTTP status {status}")
            }
            None => format!("download failed with HTTP status {status}"),
        },
        // Reasons name destination paths and I/O or transport error text at the
        // call sites; the URL is the identifier the API may carry, sanitized.
        SafeDownloadFailed { url, .. } => match sanitized_download_url(url) {
            Some(sanitized) => format!("download from {sanitized} failed"),
            None => "download failed".to_owned(),
        },
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
