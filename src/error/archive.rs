//! Archive-extraction error helpers (`archive.*` namespace).

use http::StatusCode;

use super::StackError;

pub(super) fn error_code(err: &StackError) -> Option<&'static str> {
    use StackError::*;
    Some(match err {
        ArchiveUnsafeEntry { .. } => "archive.unsafe_entry",
        ArchiveUnsupportedFormat => "archive.unsupported_format",
        ArchiveTooLarge { .. } => "archive.too_large",
        ArchiveReadFailed { .. } => "archive.read_failed",
        _ => return None,
    })
}

pub(super) fn public_message(err: &StackError) -> Option<String> {
    use StackError::*;
    Some(match err {
        ArchiveUnsafeEntry { kind, name } => {
            format!("archive contained an unsafe {kind}: `{name}`")
        }
        ArchiveUnsupportedFormat => "archive format is not supported".to_owned(),
        ArchiveTooLarge { limit } => {
            format!("archive extracted output exceeded the {limit}-byte size limit")
        }
        ArchiveReadFailed { reason } => format!("archive read failed: {reason}"),
        _ => return None,
    })
}

pub(super) fn http_status(err: &StackError) -> Option<StatusCode> {
    use StackError::*;
    Some(match err {
        ArchiveUnsafeEntry { .. } | ArchiveUnsupportedFormat | ArchiveTooLarge { .. } => {
            StatusCode::BAD_REQUEST
        }
        ArchiveReadFailed { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        _ => return None,
    })
}
