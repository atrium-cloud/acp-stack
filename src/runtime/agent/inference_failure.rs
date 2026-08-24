//! Inference-failure classifier for ACP agent JSON-RPC errors.
//!
//! Sanitization invariant: [`Classified`] carries only an enum, an
//! `Option<u16>`, and a `&'static str`, so no secret text embedded in
//! `err.to_string()` can reach SQLite or events through this type.

use agent_client_protocol::Error as AcpError;

use crate::state::FailureClass;

/// Static reason categories, kept `&'static str` rather than raw upstream text
/// so the classifier is the only place that must vet text for safety.
pub mod reason {
    pub const RATE_LIMIT: &str = "rate_limit";
    pub const INTERNAL_SERVER_ERROR: &str = "internal_server_error";
    pub const BAD_GATEWAY: &str = "bad_gateway";
    pub const SERVICE_UNAVAILABLE: &str = "service_unavailable";
    pub const GATEWAY_TIMEOUT: &str = "gateway_timeout";
    pub const SERVER_OVERLOADED: &str = "server_overloaded";
    pub const CLIENT_ERROR: &str = "client_error";
    pub const UNKNOWN: &str = "unknown";
}

/// Classifier output. Only `'static`/primitive fields, so no part of the
/// original error text can be transitively persisted by callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Classified {
    /// Bucket for `prompts.failure_class` persistence.
    pub class: FailureClass,
    /// Parsed HTTP status code, when one could be located in the error string.
    pub status_code: Option<u16>,
    /// Stable, vetted reason label.
    pub reason_category: &'static str,
}

impl Classified {
    fn unknown() -> Self {
        Self {
            class: FailureClass::AgentRequest,
            status_code: None,
            reason_category: reason::UNKNOWN,
        }
    }
}

/// Classify an ACP error as an upstream-inference HTTP failure
/// (`Inference5xx`/`Inference4xx`) or `AgentRequest` for everything else.
pub fn classify(err: &AcpError) -> Classified {
    let rendered = err.to_string();
    match extract_status_code(&rendered) {
        Some(status_code) => classify_status(status_code),
        None => Classified::unknown(),
    }
}

fn classify_status(status_code: u16) -> Classified {
    if (500..600).contains(&status_code) || status_code == 529 {
        Classified {
            class: FailureClass::Inference5xx,
            status_code: Some(status_code),
            reason_category: reason_for(status_code),
        }
    } else if (400..500).contains(&status_code) {
        Classified {
            class: FailureClass::Inference4xx,
            status_code: Some(status_code),
            reason_category: reason_for(status_code),
        }
    } else {
        Classified::unknown()
    }
}

fn reason_for(status_code: u16) -> &'static str {
    match status_code {
        429 => reason::RATE_LIMIT,
        500 => reason::INTERNAL_SERVER_ERROR,
        502 => reason::BAD_GATEWAY,
        503 => reason::SERVICE_UNAVAILABLE,
        504 => reason::GATEWAY_TIMEOUT,
        529 => reason::SERVER_OVERLOADED,
        code if (400..500).contains(&code) => reason::CLIENT_ERROR,
        _ => reason::UNKNOWN,
    }
}

/// First plausible 3-digit status code (400..600) preceded by a recognizable
/// prefix word or followed by a canonical reason phrase; the required context
/// is what keeps timestamps and ids from matching.
fn extract_status_code(rendered: &str) -> Option<u16> {
    let lower = rendered.to_ascii_lowercase();

    const PREFIX_PATTERNS: &[&str] = &[
        "status code:",
        "status code ",
        "status:",
        "status ",
        "http/1.0 ",
        "http/1.1 ",
        "http/2.0 ",
        "http/2 ",
        "http/3 ",
        "http ",
    ];
    for prefix in PREFIX_PATTERNS {
        let mut search_from = 0usize;
        while let Some(found) = lower[search_from..].find(prefix) {
            let after = search_from + found + prefix.len();
            if let Some(code) = parse_status_at(&rendered[after..]) {
                return Some(code);
            }
            search_from = after;
        }
    }

    // Most providers ship these phrases verbatim in `error.message`.
    const REASON_PHRASES: &[(&str, u16)] = &[
        ("too many requests", 429),
        ("internal server error", 500),
        ("bad gateway", 502),
        ("service unavailable", 503),
        ("gateway timeout", 504),
    ];
    for (phrase, default_code) in REASON_PHRASES {
        if let Some(found) = lower.find(phrase) {
            // Prefer a 3-digit code immediately before the phrase; otherwise
            // fall back to the phrase's canonical mapping.
            let window_start = found.saturating_sub(6);
            let window = &rendered[window_start..found];
            if let Some(code) = trailing_status_code(window) {
                return Some(code);
            }
            return Some(*default_code);
        }
    }

    None
}

/// Parse a 3-digit code (400..600) from the head of a slice. The digit run
/// must be exactly 3 long: `5031` is not status 503.
fn parse_status_at(slice: &str) -> Option<u16> {
    let trimmed = slice.trim_start();
    let digits: String = trimmed
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .collect();
    if digits.len() != 3 {
        return None;
    }
    let head: u16 = digits.parse().ok()?;
    if (400..600).contains(&head) {
        Some(head)
    } else {
        None
    }
}

/// Parse a 3-digit code (400..600) from the tail of a short text window.
fn trailing_status_code(window: &str) -> Option<u16> {
    let trimmed = window.trim_end();
    let digits: String = trimmed
        .chars()
        .rev()
        .take_while(|character| character.is_ascii_digit())
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    if digits.len() != 3 {
        return None;
    }
    let code: u16 = digits.parse().ok()?;
    if (400..600).contains(&code) {
        Some(code)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::FailureClass;

    fn make_error(message: &str) -> AcpError {
        AcpError::new(-32000, message.to_owned())
    }

    #[test]
    fn classifies_named_5xx_statuses() {
        let cases: &[(&str, u16, &str)] = &[
            (
                "internal error: HTTP 500",
                500,
                reason::INTERNAL_SERVER_ERROR,
            ),
            ("status: 502 Bad Gateway", 502, reason::BAD_GATEWAY),
            ("status code 503", 503, reason::SERVICE_UNAVAILABLE),
            ("HTTP/1.1 504 Gateway Timeout", 504, reason::GATEWAY_TIMEOUT),
            ("status: 529", 529, reason::SERVER_OVERLOADED),
        ];
        for (message, expected_code, expected_reason) in cases {
            let result = classify(&make_error(message));
            assert_eq!(
                result.class,
                FailureClass::Inference5xx,
                "case `{message}` must classify as Inference5xx"
            );
            assert_eq!(result.status_code, Some(*expected_code), "case `{message}`");
            assert_eq!(result.reason_category, *expected_reason, "case `{message}`");
        }
    }

    #[test]
    fn classifies_429_as_rate_limit_4xx() {
        let cases = &[
            "status: 429",
            "429 Too Many Requests",
            "HTTP/1.1 429",
            "received status code 429 from upstream",
        ];
        for message in cases {
            let result = classify(&make_error(message));
            assert_eq!(
                result.class,
                FailureClass::Inference4xx,
                "case `{message}` must classify as Inference4xx"
            );
            assert_eq!(result.status_code, Some(429), "case `{message}`");
            assert_eq!(
                result.reason_category,
                reason::RATE_LIMIT,
                "case `{message}`"
            );
        }
    }

    #[test]
    fn classifies_other_4xx_as_client_error() {
        let result = classify(&make_error("status: 418 I'm a teapot"));
        assert_eq!(result.class, FailureClass::Inference4xx);
        assert_eq!(result.status_code, Some(418));
        assert_eq!(result.reason_category, reason::CLIENT_ERROR);
    }

    #[test]
    fn defaults_to_agent_request_unknown_when_no_status_present() {
        let result = classify(&make_error("connection refused"));
        assert_eq!(result.class, FailureClass::AgentRequest);
        assert_eq!(result.status_code, None);
        assert_eq!(result.reason_category, reason::UNKNOWN);
    }

    #[test]
    fn ignores_two_xx_and_three_xx_codes() {
        let result = classify(&make_error("status: 200 OK"));
        assert_eq!(result.class, FailureClass::AgentRequest);
        assert_eq!(result.status_code, None);
        assert_eq!(result.reason_category, reason::UNKNOWN);
    }

    #[test]
    fn does_not_leak_url_or_secret_into_reason_category() {
        // Sanitization invariant: even when the SDK error embeds a URL with a
        // query-string secret, only the vetted `'static` label surfaces.
        let message = "upstream call to https://api.openai.com/v1/chat?key=sk-secret returned status: 503 Service Unavailable";
        let result = classify(&make_error(message));
        assert_eq!(result.class, FailureClass::Inference5xx);
        assert_eq!(result.status_code, Some(503));
        let allowed: &[&str] = &[
            reason::RATE_LIMIT,
            reason::INTERNAL_SERVER_ERROR,
            reason::BAD_GATEWAY,
            reason::SERVICE_UNAVAILABLE,
            reason::GATEWAY_TIMEOUT,
            reason::SERVER_OVERLOADED,
            reason::CLIENT_ERROR,
            reason::UNKNOWN,
        ];
        assert!(
            allowed.contains(&result.reason_category),
            "reason_category must be one of the allowed static labels"
        );
        assert!(!result.reason_category.contains("openai"));
        assert!(!result.reason_category.contains("sk-secret"));
    }

    #[test]
    fn handles_status_with_long_digit_runs_safely() {
        let result = classify(&make_error("status: 5031"));
        assert_eq!(result.class, FailureClass::AgentRequest);
        assert_eq!(result.status_code, None);
    }
}
