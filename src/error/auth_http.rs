//! HTTP-edge auth and request-shape error helpers.
//!
//! Covers the runtime `auth.*` namespace (rate limit, IP block, origin
//! allowlist) plus the `request.invalid_param` query-validation shape.

use http::StatusCode;

use super::StackError;

pub(super) fn error_code(err: &StackError) -> Option<&'static str> {
    use StackError::*;
    Some(match err {
        RateLimited => "auth.rate_limited",
        IpBlocked { .. } => "auth.ip_blocked",
        OriginNotAllowed { .. } => "auth.origin_not_allowed",
        InvalidParam { .. } => "request.invalid_param",
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
        _ => return None,
    })
}

pub(super) fn http_status(err: &StackError) -> Option<StatusCode> {
    use StackError::*;
    Some(match err {
        RateLimited | IpBlocked { .. } => StatusCode::TOO_MANY_REQUESTS,
        OriginNotAllowed { .. } => StatusCode::FORBIDDEN,
        InvalidParam { .. } => StatusCode::BAD_REQUEST,
        _ => return None,
    })
}
