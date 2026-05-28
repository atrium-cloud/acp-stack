//! Category clustering for the durable `security.*` event kinds.
//!
//! The runtime emits a flat namespace of `security.*` event kinds from
//! `src/api/auth.rs` and `src/api/ws.rs`. The category enum here groups those
//! flat kinds into operator-facing buckets so `GET /v1/logs/security?category=`
//! and `acps logs query --category` can scope a query without forcing callers
//! to memorize the exact kind names.
//!
//! Each `SecurityCategory` resolves to a fixed slice of kinds; adding a new
//! kind that belongs in an existing bucket means extending the corresponding
//! `KINDS_*` constant below. Adding a new bucket means adding a variant plus a
//! match arm in `as_str`, `kinds`, and the `FromStr` impl.

use crate::error::StackError;
use std::str::FromStr;

// === CONSTANTS ===

/// Operator-facing category strings accepted by `?category=` and `--category`.
const CATEGORY_RATE_LIMIT: &str = "rate_limit";
const CATEGORY_ORIGIN_CORS: &str = "origin_cors";
const CATEGORY_IP_BLOCK: &str = "ip_block";
const CATEGORY_OVERSIZED_REQUEST: &str = "oversized_request";

/// Kinds emitted by the rate limiter at `src/api/auth.rs`.
const KINDS_RATE_LIMIT: &[&str] = &["security.rate_limited"];
/// Kinds emitted when an HTTP or WebSocket origin fails the configured CORS
/// allowlist (see `src/api/auth.rs` for HTTP and `src/api/ws.rs` for WS).
const KINDS_ORIGIN_CORS: &[&str] = &["security.cors_origin_denied", "security.ws_origin_denied"];
/// Kinds emitted by the IP-block ladder: the active block rejection and the
/// post-failure block-applied marker (see `src/api/auth.rs`).
const KINDS_IP_BLOCK: &[&str] = &["security.ip_block_active", "security.ip_block_applied"];
/// Kind emitted by the request-size limiter (`src/api/auth.rs`).
const KINDS_OVERSIZED_REQUEST: &[&str] = &["security.request_oversized"];

/// Operator-facing category label used by HTTP `?category=` and the CLI
/// `--category` flag. The label-to-kind mapping is the single source of truth;
/// `from_str` parses operator input and `kinds` returns the SQL `IN (...)` set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecurityCategory {
    RateLimit,
    OriginCors,
    IpBlock,
    OversizedRequest,
}

impl SecurityCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            SecurityCategory::RateLimit => CATEGORY_RATE_LIMIT,
            SecurityCategory::OriginCors => CATEGORY_ORIGIN_CORS,
            SecurityCategory::IpBlock => CATEGORY_IP_BLOCK,
            SecurityCategory::OversizedRequest => CATEGORY_OVERSIZED_REQUEST,
        }
    }

    pub fn kinds(self) -> &'static [&'static str] {
        match self {
            SecurityCategory::RateLimit => KINDS_RATE_LIMIT,
            SecurityCategory::OriginCors => KINDS_ORIGIN_CORS,
            SecurityCategory::IpBlock => KINDS_IP_BLOCK,
            SecurityCategory::OversizedRequest => KINDS_OVERSIZED_REQUEST,
        }
    }
}

/// Parse an operator-supplied category label. Returns `InvalidParam` for any
/// value outside the closed set so the API surfaces a 4xx instead of silently
/// returning the unfiltered stream.
impl FromStr for SecurityCategory {
    type Err = StackError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            CATEGORY_RATE_LIMIT => Ok(SecurityCategory::RateLimit),
            CATEGORY_ORIGIN_CORS => Ok(SecurityCategory::OriginCors),
            CATEGORY_IP_BLOCK => Ok(SecurityCategory::IpBlock),
            CATEGORY_OVERSIZED_REQUEST => Ok(SecurityCategory::OversizedRequest),
            other => Err(StackError::InvalidParam {
                field: "category",
                reason: format!(
                    "unknown security category `{other}`; expected one of `{CATEGORY_RATE_LIMIT}`, `{CATEGORY_ORIGIN_CORS}`, `{CATEGORY_IP_BLOCK}`, `{CATEGORY_OVERSIZED_REQUEST}`"
                ),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_labels() {
        for category in [
            SecurityCategory::RateLimit,
            SecurityCategory::OriginCors,
            SecurityCategory::IpBlock,
            SecurityCategory::OversizedRequest,
        ] {
            let label = category.as_str();
            let parsed = SecurityCategory::from_str(label).expect("label round-trips");
            assert_eq!(parsed, category);
            assert!(!category.kinds().is_empty());
        }
    }

    #[test]
    fn unknown_category_rejected_with_invalid_param() {
        let err = SecurityCategory::from_str("does_not_exist")
            .expect_err("unknown label must be rejected");
        match err {
            StackError::InvalidParam { field, reason } => {
                assert_eq!(field, "category");
                assert!(
                    reason.contains("does_not_exist"),
                    "reason should echo the bad input: {reason}"
                );
            }
            other => panic!("expected InvalidParam, got {other:?}"),
        }
    }
}
