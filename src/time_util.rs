//! Helpers for parsing human-friendly time spans used by log queries and the metrics summary.

use chrono::Duration;

/// Parse a duration suffix in the shapes `Ns` / `Nm` / `Nh` / `Nd` / `Nw`; `None` when unparseable.
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

/// Parse history-window suffixes for session range views; `mo` is a fixed 30-day month and `y`
/// a fixed 365-day year, so filtering stays independent of calendar boundaries.
pub fn parse_coarse_duration_suffix(input: &str) -> Option<Duration> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(number_part) = trimmed.strip_suffix("mo") {
        let value: i64 = number_part.parse().ok()?;
        if value < 0 {
            return None;
        }
        return Some(Duration::days(value.checked_mul(30)?));
    }
    let (number_part, unit) = trimmed.split_at(trimmed.len() - 1);
    let value: i64 = number_part.parse().ok()?;
    if value < 0 {
        return None;
    }
    match unit {
        "m" => Some(Duration::minutes(value)),
        "h" => Some(Duration::hours(value)),
        "d" => Some(Duration::days(value)),
        "w" => Some(Duration::weeks(value)),
        "y" => Some(Duration::days(value.checked_mul(365)?)),
        _ => None,
    }
}

/// Resolve a duration range relative to `now`, rejecting anything preceding the Unix epoch.
pub fn resolve_since_after_unix_epoch(
    duration: Duration,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<chrono::DateTime<chrono::Utc>> {
    let epoch = chrono::DateTime::from_timestamp(0, 0)?;
    let resolved = now.checked_sub_signed(duration)?;
    if resolved < epoch {
        None
    } else {
        Some(resolved)
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

    #[test]
    fn parses_coarse_suffixes() {
        assert_eq!(
            parse_coarse_duration_suffix("30m"),
            Some(Duration::minutes(30))
        );
        assert_eq!(
            parse_coarse_duration_suffix("24h"),
            Some(Duration::hours(24))
        );
        assert_eq!(
            parse_coarse_duration_suffix("60d"),
            Some(Duration::days(60))
        );
        assert_eq!(parse_coarse_duration_suffix("2w"), Some(Duration::weeks(2)));
        assert_eq!(
            parse_coarse_duration_suffix("6mo"),
            Some(Duration::days(180))
        );
        assert_eq!(
            parse_coarse_duration_suffix("1y"),
            Some(Duration::days(365))
        );
    }

    #[test]
    fn coarse_suffixes_reject_seconds_or_unknown_units() {
        assert_eq!(parse_coarse_duration_suffix("30s"), None);
        assert_eq!(parse_coarse_duration_suffix("15x"), None);
        assert_eq!(parse_coarse_duration_suffix("-1d"), None);
    }

    #[test]
    fn resolves_since_after_unix_epoch() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-05-26T00:00:00Z")
            .expect("timestamp")
            .with_timezone(&chrono::Utc);
        assert_eq!(
            resolve_since_after_unix_epoch(Duration::days(1), now)
                .expect("in range")
                .to_rfc3339(),
            "2026-05-25T00:00:00+00:00"
        );
        assert_eq!(
            resolve_since_after_unix_epoch(Duration::days(30_000), now),
            None
        );
    }
}
