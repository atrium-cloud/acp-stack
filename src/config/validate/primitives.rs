//! Small reusable validators and helpers used across the per-domain validators.
//!
//! These intentionally avoid pulling in domain knowledge: they validate types
//! (durations, sockets, paths, sha256, env names, secret refs) and surface
//! generic `StackError` variants keyed on a field name supplied by the caller.

use std::net::SocketAddr;
use std::path::Path;

use crate::error::{Result, StackError};

/// Parse a duration string like "10m", "5s", "2h", "1d", "4w", "750ms". Returns `None` on
/// any invalid input. Empty string and pure-numeric inputs (no suffix) are
/// rejected so config typos surface at load time rather than meaning seconds.
pub fn parse_duration_string(input: &str) -> Option<std::time::Duration> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let (number_part, unit_part) = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .map(|idx| trimmed.split_at(idx))?;
    if number_part.is_empty() {
        return None;
    }
    let value: u64 = number_part.parse().ok()?;
    match unit_part {
        "ms" => Some(std::time::Duration::from_millis(value)),
        "s" => Some(std::time::Duration::from_secs(value)),
        "m" => Some(std::time::Duration::from_secs(value.checked_mul(60)?)),
        "h" => Some(std::time::Duration::from_secs(value.checked_mul(3_600)?)),
        "d" => Some(std::time::Duration::from_secs(value.checked_mul(86_400)?)),
        "w" => Some(std::time::Duration::from_secs(value.checked_mul(604_800)?)),
        _ => None,
    }
}

/// The largest duration any config field may express: the time elapsed since
/// the Unix epoch. Durations are used as `now - duration` windows (staleness,
/// auto-update skip), so a span longer than this would place the computed
/// cutoff before 1970-01-01 — meaningless for the timestamps this runtime
/// records. The bound grows with wall-clock time, so a config that validates
/// once stays valid. A system clock set before 1970 (degenerate) yields
/// `Duration::MAX`, which disables the cap rather than failing every load.
fn max_duration_since_epoch() -> std::time::Duration {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or(std::time::Duration::MAX)
}

/// Validate a duration-valued config field: it must parse via
/// [`parse_duration_string`] and must not exceed [`max_duration_since_epoch`].
/// Returns the parsed `Duration` so callers can apply their own extra checks
/// (e.g. non-zero). Every duration field routes through this or
/// [`normalize_duration`], so the 1970 hardstop lives in exactly one helper.
pub(crate) fn validate_duration_field(
    field: &'static str,
    raw: &str,
) -> Result<std::time::Duration> {
    let duration = parse_duration_string(raw).ok_or(StackError::InvalidDurationField { field })?;
    validate_duration_epoch_hardstop(field, raw, duration)?;
    Ok(duration)
}

/// The 1970 hardstop described at [`max_duration_since_epoch`], shared by
/// [`validate_duration_field`] and [`normalize_duration`].
fn validate_duration_epoch_hardstop(
    field: &'static str,
    raw: &str,
    duration: std::time::Duration,
) -> Result<()> {
    if duration > max_duration_since_epoch() {
        return Err(StackError::InvalidParam {
            field,
            reason: format!(
                "`{raw}` exceeds the maximum interval (the time since 1970-01-01); a longer span would place a `now - {raw}` cutoff before the Unix epoch"
            ),
        });
    }
    Ok(())
}

/// Duration units understood by [`parse_duration_string`], plus `Month`.
/// Ordered finest to coarsest.
///
/// `Month` is deliberately *not* in the shared parser: it is spelled `mo` (a
/// fixed 30-day month, mirroring `time_util::parse_coarse_duration_suffix`) so
/// it never collides with `m` (minute). [`normalize_duration`] recognizes it so
/// consumers can accept it via [`DurationLimits`]; a consumer that does must
/// also teach its runtime re-parser the `mo` suffix, since stored values are
/// re-parsed with [`parse_duration_string`] when scheduling.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum DurationUnit {
    Millisecond,
    Second,
    Minute,
    Hour,
    Day,
    Week,
    Month,
}

impl DurationUnit {
    fn from_suffix(suffix: &str) -> Option<Self> {
        Some(match suffix {
            "ms" => Self::Millisecond,
            "s" => Self::Second,
            "m" => Self::Minute,
            "h" => Self::Hour,
            "d" => Self::Day,
            "w" => Self::Week,
            "mo" => Self::Month,
            _ => return None,
        })
    }

    fn suffix(self) -> &'static str {
        match self {
            Self::Millisecond => "ms",
            Self::Second => "s",
            Self::Minute => "m",
            Self::Hour => "h",
            Self::Day => "d",
            Self::Week => "w",
            Self::Month => "mo",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Millisecond => "millisecond",
            Self::Second => "second",
            Self::Minute => "minute",
            Self::Hour => "hour",
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
        }
    }

    /// "a day", "an hour" — for prose in error messages and prompts.
    fn indefinite_name(self) -> String {
        let article = if matches!(self, Self::Hour) {
            "an"
        } else {
            "a"
        };
        format!("{article} {}", self.name())
    }

    fn length(self) -> std::time::Duration {
        const DAY_SECS: u64 = 86_400;
        match self {
            Self::Millisecond => std::time::Duration::from_millis(1),
            Self::Second => std::time::Duration::from_secs(1),
            Self::Minute => std::time::Duration::from_secs(60),
            Self::Hour => std::time::Duration::from_secs(3_600),
            Self::Day => std::time::Duration::from_secs(DAY_SECS),
            Self::Week => std::time::Duration::from_secs(7 * DAY_SECS),
            Self::Month => std::time::Duration::from_secs(30 * DAY_SECS),
        }
    }

    fn checked_duration(self, count: u64) -> Option<std::time::Duration> {
        match self {
            Self::Millisecond => Some(std::time::Duration::from_millis(count)),
            _ => self
                .length()
                .as_secs()
                .checked_mul(count)
                .map(std::time::Duration::from_secs),
        }
    }
}

/// Per-consumer acceptance rules for a duration-valued field: which unit
/// suffixes are accepted and the minimum total duration. Declared at each
/// consumer (e.g. stack vs managed-agent update frequencies) so every surface
/// states its own granularity policy instead of sharing a hardcoded one.
pub(crate) struct DurationLimits {
    /// Accepted units, declared finest-first.
    pub accepted_units: &'static [DurationUnit],
    pub minimum: std::time::Duration,
}

impl DurationLimits {
    /// Minute and month must never appear in the same accepted set: both read
    /// as "m" to users (month is spelled `mo`), so one field accepting both
    /// would invite silent misconfiguration. Enforced here — at compile time
    /// for the `const` declarations consumers use.
    pub(crate) const fn new(
        accepted_units: &'static [DurationUnit],
        minimum: std::time::Duration,
    ) -> Self {
        assert!(
            !accepted_units.is_empty(),
            "duration limits must accept at least one unit"
        );
        let mut has_minute = false;
        let mut has_month = false;
        let mut index = 0;
        while index < accepted_units.len() {
            if index > 0 {
                assert!(
                    accepted_units[index - 1] as u8 <= accepted_units[index] as u8,
                    "duration limit units must be declared finest-first"
                );
            }
            match accepted_units[index] {
                DurationUnit::Minute => has_minute = true,
                DurationUnit::Month => has_month = true,
                _ => {}
            }
            index += 1;
        }
        assert!(
            !(has_minute && has_month),
            "duration limits must not accept both minute and month units"
        );
        Self {
            accepted_units,
            minimum,
        }
    }

    fn smallest_unit(&self) -> DurationUnit {
        self.accepted_units[0]
    }

    /// A fine and a coarse example value, e.g. `1h, 3w` — for prompts and
    /// error messages.
    pub(crate) fn examples(&self) -> String {
        format!(
            "1{}, 3{}",
            self.smallest_unit().suffix(),
            self.accepted_units[self.accepted_units.len() - 1].suffix()
        )
    }

    /// The minimum rendered as prose, e.g. `1 hour` — for prompts and error
    /// messages.
    pub(crate) fn render_minimum(&self) -> String {
        render_duration_prose(self.minimum)
    }

    /// The accepted units rendered for error messages, e.g. `an hour (h), day
    /// (d), or week (w) unit`.
    fn render_accepted_units(&self) -> String {
        let parts: Vec<String> = self
            .accepted_units
            .iter()
            .map(|unit| format!("{} ({})", unit.name(), unit.suffix()))
            .collect();
        let article = if matches!(self.smallest_unit(), DurationUnit::Hour) {
            "an"
        } else {
            "a"
        };
        match parts.len() {
            1 => format!("{article} {} unit", parts[0]),
            2 => format!("{article} {} or {} unit", parts[0], parts[1]),
            last => format!(
                "{article} {}, or {} unit",
                parts[..last - 1].join(", "),
                parts[last - 1]
            ),
        }
    }
}

/// Validate a duration-valued field against a consumer's [`DurationLimits`]:
/// the value must carry one of the accepted unit suffixes and total at least
/// the minimum duration. Returns the trimmed value for storage. Applies the
/// same 1970 hardstop as [`validate_duration_field`] so config load stays in
/// agreement with the runtime, which re-parses the stored string when it
/// schedules work.
pub(crate) fn normalize_duration(
    field: &'static str,
    raw: &str,
    limits: &DurationLimits,
) -> Result<String> {
    let value = raw.trim();
    let unit_index = value
        .find(|character: char| !character.is_ascii_digit())
        .ok_or(StackError::InvalidParam {
            field,
            reason: format!(
                "expected a count and a unit (e.g. {}), got `{value}`",
                limits.examples()
            ),
        })?;
    let (digits, suffix) = value.split_at(unit_index);
    let accepted =
        DurationUnit::from_suffix(suffix).filter(|unit| limits.accepted_units.contains(unit));
    let Some(unit) = accepted else {
        return Err(StackError::InvalidParam {
            field,
            reason: format!(
                "use {}; the minimum granularity is {}, got `{value}`",
                limits.render_accepted_units(),
                limits.smallest_unit().indefinite_name()
            ),
        });
    };
    let count: u64 = digits.parse().map_err(|_| StackError::InvalidParam {
        field,
        reason: format!("`{value}` is not a valid count + unit"),
    })?;
    let duration = unit
        .checked_duration(count)
        .ok_or(StackError::InvalidDurationField { field })?;
    validate_duration_epoch_hardstop(field, value, duration)?;
    if duration < limits.minimum {
        return Err(StackError::InvalidParam {
            field,
            reason: format!("must be at least {}", limits.render_minimum()),
        });
    }
    Ok(value.to_owned())
}

/// Render a duration as `{count} {unit}` with the coarsest whole unit, for
/// prose in prompts and error messages ("1 hour", "3 weeks").
fn render_duration_prose(duration: std::time::Duration) -> String {
    for unit in [
        DurationUnit::Month,
        DurationUnit::Week,
        DurationUnit::Day,
        DurationUnit::Hour,
        DurationUnit::Minute,
        DurationUnit::Second,
    ] {
        let unit_secs = unit.length().as_secs();
        if duration.as_secs() >= unit_secs && duration.as_secs().is_multiple_of(unit_secs) {
            let count = duration.as_secs() / unit_secs;
            let plural = if count == 1 { "" } else { "s" };
            return format!("{count} {}{plural}", unit.name());
        }
    }
    format!("{} milliseconds", duration.as_millis())
}

/// Hosts for which plaintext http is accepted: a request to a loopback address
/// never leaves the machine, so the no-plaintext-off-host rule that motivates
/// https-only is not violated.
pub(crate) const LOOPBACK_HOSTS: [&str; 3] = ["127.0.0.1", "::1", "localhost"];

/// Upper bound on any externally supplied endpoint URL.
pub(crate) const MAX_ENDPOINT_URL_BYTES: usize = 2048;

/// What is wrong with an endpoint URL. The caller owns the wording, because the
/// same rules guard config-declared MCP servers and orchestrator-supplied
/// provider endpoints, whose operators read very different messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EndpointUrlProblem {
    Unparseable,
    NotHttpsOrLoopback,
    ContainsCredentials,
    ContainsQueryOrFragment,
    TooLong,
}

/// The shared endpoint-URL rule: https, or http toward a loopback host; no
/// embedded credentials; no query or fragment (both are meaningless on an
/// endpoint base and are a smuggling surface); bounded length. The value is
/// never normalized — callers store it verbatim and append per their own
/// convention.
pub(crate) fn check_endpoint_url(
    url: &str,
    allow_query_or_fragment: bool,
) -> std::result::Result<(), EndpointUrlProblem> {
    if url.len() > MAX_ENDPOINT_URL_BYTES {
        return Err(EndpointUrlProblem::TooLong);
    }
    let parsed = reqwest::Url::parse(url).map_err(|_| EndpointUrlProblem::Unparseable)?;
    // `host_str()` keeps the brackets around IPv6 literals (`[::1]`).
    let http_loopback = parsed.scheme() == "http"
        && parsed.host_str().is_some_and(|host| {
            LOOPBACK_HOSTS.contains(&host.trim_start_matches('[').trim_end_matches(']'))
        });
    if (parsed.scheme() != "https" || parsed.host_str().is_none()) && !http_loopback {
        return Err(EndpointUrlProblem::NotHttpsOrLoopback);
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(EndpointUrlProblem::ContainsCredentials);
    }
    if !allow_query_or_fragment && (parsed.query().is_some() || parsed.fragment().is_some()) {
        return Err(EndpointUrlProblem::ContainsQueryOrFragment);
    }
    Ok(())
}

pub(crate) fn validate_socket_address(field: &'static str, value: &str) -> Result<()> {
    value
        .parse::<SocketAddr>()
        .map(|_| ())
        .map_err(|_| StackError::InvalidSocketAddress { field })
}

pub(crate) fn validate_nonzero(field: &'static str, value: u64) -> Result<()> {
    if value == 0 {
        return Err(StackError::NonZeroRequired { field });
    }

    Ok(())
}

pub(crate) fn validate_absolute_path(field: &'static str, value: &str) -> Result<()> {
    if !Path::new(value).is_absolute() {
        return Err(StackError::PathMustBeAbsolute { field });
    }

    Ok(())
}

/// `Path::starts_with` is purely lexical — `/workspace/../etc/uploads`
/// "starts with" `/workspace` even though it resolves outside. Reject `..`
/// segments in the configured paths up front so the workspace-root/uploads
/// containment check below cannot be tricked, and so request-time path
/// resolution does not have to canonicalize the config paths repeatedly.
pub(crate) fn validate_no_parent_dir_segments(field: &'static str, value: &str) -> Result<()> {
    for component in Path::new(value).components() {
        if matches!(component, std::path::Component::ParentDir) {
            return Err(StackError::PathContainsParentDir { field });
        }
    }
    Ok(())
}

pub(crate) fn require_present<'a>(field: &'static str, value: Option<&'a str>) -> Result<&'a str> {
    match value {
        Some(value) if !value.trim().is_empty() => Ok(value),
        _ => Err(StackError::MissingField { field }),
    }
}

pub(crate) fn validate_expected_sha256(value: &str) -> Result<()> {
    if value.len() == 64 && value.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')) {
        Ok(())
    } else {
        Err(StackError::InvalidExpectedSha256)
    }
}

pub(crate) fn validate_nonempty(field: &'static str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(StackError::MissingField { field });
    }
    Ok(())
}

pub(crate) fn validate_non_empty_trimmed(field: &'static str, value: &str) -> Result<()> {
    if value.trim().is_empty() || value.trim().len() != value.len() {
        return Err(StackError::MissingField { field });
    }
    Ok(())
}

pub(crate) fn validate_optional_config_path(field: &'static str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(StackError::MissingField { field });
    }
    if !Path::new(value).is_absolute() {
        return Err(StackError::PathMustBeAbsolute { field });
    }
    for component in Path::new(value).components() {
        if matches!(component, std::path::Component::ParentDir) {
            return Err(StackError::PathContainsParentDir { field });
        }
    }
    Ok(())
}

pub(crate) fn validate_secret_ref_name_value(name: &str) -> Result<()> {
    if !is_valid_secret_ref_name(name) {
        return Err(StackError::InvalidSecretRefName {
            name: name.to_owned(),
        });
    }
    Ok(())
}

/// Accept identifier-like names: ASCII letters, digits, and underscores; must
/// not be empty and must not start with a digit. Matches the spirit of POSIX
/// env-var names and the auth-key naming used elsewhere in the project.
pub fn is_valid_secret_ref_name(name: &str) -> bool {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.len() != name.len() {
        return false;
    }
    let mut chars = trimmed.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

pub(crate) fn secret_ref_looks_like_value(name: &str) -> bool {
    if name.len() > 128 {
        return true;
    }
    secret_value_shape(name)
}

/// The shape heuristics of [`secret_ref_looks_like_value`] without the
/// length ceiling, which only makes sense for ref names. Concatenated
/// template literals are screened with this so long-but-legitimate static
/// text does not trip the name-length rule.
pub(crate) fn secret_value_shape(text: &str) -> bool {
    if text.len() > 40 && text.chars().all(|c| c.is_ascii_hexdigit()) {
        return true;
    }
    if text.starts_with("acps_")
        || text.starts_with("sk-")
        || text.starts_with("ghp_")
        || text.starts_with("github_pat_")
        || text.starts_with("xoxb-")
        || text.starts_with("xoxp-")
        || text.starts_with("xoxa-")
    {
        return true;
    }
    let jwt_parts = text.split('.').collect::<Vec<_>>();
    if jwt_parts.len() == 3
        && jwt_parts
            .iter()
            .all(|part| part.len() >= 10 && part.chars().all(is_base64url_char))
    {
        return true;
    }
    false
}

fn is_base64url_char(value: char) -> bool {
    value.is_ascii_alphanumeric() || value == '_' || value == '-'
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOURLY_OR_SLOWER: DurationLimits = DurationLimits::new(
        &[DurationUnit::Hour, DurationUnit::Day, DurationUnit::Week],
        std::time::Duration::from_secs(3_600),
    );

    #[test]
    fn normalize_duration_accepts_declared_units_at_or_above_the_minimum() {
        assert_eq!(
            normalize_duration("field", "12h", &HOURLY_OR_SLOWER).expect("12h is accepted"),
            "12h"
        );
        assert_eq!(
            normalize_duration("field", " 3w ", &HOURLY_OR_SLOWER).expect("3w is accepted"),
            "3w"
        );
        assert_eq!(
            normalize_duration("field", "1h", &HOURLY_OR_SLOWER)
                .expect("the minimum itself is accepted"),
            "1h"
        );
    }

    #[test]
    fn normalize_duration_rejects_units_outside_the_declared_set() {
        let error = normalize_duration("field", "30m", &HOURLY_OR_SLOWER)
            .expect_err("minutes are finer than the declared smallest unit");
        let message = error.to_string();
        assert!(
            message.contains("an hour (h), day (d), or week (w) unit"),
            "got: {message}"
        );
        assert!(
            message.contains("the minimum granularity is an hour"),
            "got: {message}"
        );
    }

    #[test]
    fn normalize_duration_rejects_month_when_not_declared() {
        // Month is a known unit (spelled `mo`), but a consumer that did not
        // declare it gets the same accepted-units message as any other unit.
        let error = normalize_duration("field", "1mo", &HOURLY_OR_SLOWER)
            .expect_err("month is not in the declared set");
        assert!(error.to_string().contains("hour (h)"), "got: {error}");
    }

    #[test]
    fn normalize_duration_enforces_the_minimum_total_duration() {
        let error = normalize_duration("field", "0d", &HOURLY_OR_SLOWER)
            .expect_err("zero is below the minimum");
        assert!(
            error.to_string().contains("must be at least 1 hour"),
            "got: {error}"
        );
    }

    #[test]
    fn normalize_duration_rejects_missing_and_garbled_units() {
        for raw in ["10", "d", "1.5h", ""] {
            assert!(
                normalize_duration("field", raw, &HOURLY_OR_SLOWER).is_err(),
                "{raw} must be rejected"
            );
        }
    }

    #[test]
    fn normalize_duration_applies_the_epoch_hardstop() {
        let error = normalize_duration("field", "9999w", &HOURLY_OR_SLOWER)
            .expect_err("longer than the epoch span");
        assert!(
            error.to_string().contains("exceeds the maximum interval"),
            "got: {error}"
        );
    }

    #[test]
    #[should_panic(expected = "must not accept both minute and month")]
    fn duration_limits_reject_minute_and_month_together() {
        let _ = DurationLimits::new(
            &[DurationUnit::Minute, DurationUnit::Month],
            std::time::Duration::ZERO,
        );
    }

    #[test]
    #[should_panic(expected = "finest-first")]
    fn duration_limits_require_finest_first_declaration() {
        let _ = DurationLimits::new(
            &[DurationUnit::Week, DurationUnit::Day],
            std::time::Duration::ZERO,
        );
    }
}
