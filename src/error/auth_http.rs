//! HTTP-edge auth (`auth.*`) and request-shape (`request.invalid_param`,
//! `request.too_large`) error helpers.

use http::StatusCode;

use super::StackError;
use crate::envelope::{REQUEST_TOO_LARGE_CODE, REQUEST_TOO_LARGE_MESSAGE};

pub(super) fn error_code(err: &StackError) -> Option<&'static str> {
    use StackError::*;
    Some(match err {
        RateLimited => "auth.rate_limited",
        IpBlocked { .. } => "auth.ip_blocked",
        OriginNotAllowed { .. } => "auth.origin_not_allowed",
        InvalidParam { .. } => "request.invalid_param",
        RequestTooLarge { .. } => REQUEST_TOO_LARGE_CODE,
        _ => return None,
    })
}

pub(super) fn public_message(err: &StackError) -> Option<String> {
    use StackError::*;
    Some(match err {
        RateLimited => "rate limit exceeded".to_owned(),
        IpBlocked { .. } => "client IP is temporarily blocked".to_owned(),
        OriginNotAllowed { .. } => "origin is not allowed".to_owned(),
        InvalidParam { field, reason } => format!("invalid parameter `{field}`: {reason}"),
        RequestTooLarge { .. } => REQUEST_TOO_LARGE_MESSAGE.to_owned(),
        _ => return None,
    })
}

pub(super) fn http_status(err: &StackError) -> Option<StatusCode> {
    use StackError::*;
    Some(match err {
        RateLimited | IpBlocked { .. } => StatusCode::TOO_MANY_REQUESTS,
        OriginNotAllowed { .. } => StatusCode::FORBIDDEN,
        InvalidParam { .. } => StatusCode::BAD_REQUEST,
        RequestTooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
        _ => return None,
    })
}
