//! Lightweight helpers for parsing human-friendly time spans used by the log
//! query UX and metrics summary. The accepted suffixes intentionally cover
//! the cases operators tend to type: seconds, minutes, hours, days, weeks.
//! Anything more exotic should pass an RFC3339 timestamp instead.

use chrono::Duration;

/// Parse a duration suffix in the shapes `Ns` / `Nm` / `Nh` / `Nd` / `Nw`
/// (e.g. `30s`, `15m`, `1h`, `2d`, `1w`). Returns `None` for any unparseable
/// input — the caller is expected to fall back to a parse-as-RFC3339 path or
/// surface an error.
pub fn parse_duration_suffix(input: &str) -> Option<Duration> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let (number_part, unit) = trimmed.split_at(trimmed.len() - 1);
    let value: i64 = number_part.parse().ok()?;
    if value < 0 {
        return None;
    }
    match unit {
        "s" => Some(Duration::seconds(value)),
        "m" => Some(Duration::minutes(value)),
        "h" => Some(Duration::hours(value)),
        "d" => Some(Duration::days(value)),
        "w" => Some(Duration::weeks(value)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_suffixes() {
        assert_eq!(parse_duration_suffix("30s"), Some(Duration::seconds(30)));
        assert_eq!(parse_duration_suffix("15m"), Some(Duration::minutes(15)));
        assert_eq!(parse_duration_suffix("1h"), Some(Duration::hours(1)));
        assert_eq!(parse_duration_suffix("2d"), Some(Duration::days(2)));
        assert_eq!(parse_duration_suffix("1w"), Some(Duration::weeks(1)));
    }

    #[test]
    fn rejects_unknown_units_or_negative() {
        assert_eq!(parse_duration_suffix("5x"), None);
        assert_eq!(parse_duration_suffix("-1h"), None);
        assert_eq!(parse_duration_suffix(""), None);
        assert_eq!(parse_duration_suffix("h"), None);
        assert_eq!(parse_duration_suffix("foo"), None);
    }
}
