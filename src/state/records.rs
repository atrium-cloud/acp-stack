//! Shared filter DTOs for paginated reads; domain-specific filters live with their domain file.

use super::events::Event;
use super::security_category::SecurityCategory;

/// Sort direction for log queries; `Desc` (newest-first) is the default.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum LogOrder {
    #[default]
    Desc,
    Asc,
}

impl LogOrder {
    /// SQL direction keyword for `ORDER BY` and the keyset cursor comparison it implies.
    pub(super) fn sql_keyword(self) -> &'static str {
        match self {
            LogOrder::Desc => "DESC",
            LogOrder::Asc => "ASC",
        }
    }
}

/// Composable filter for `events` queries; absent fields do not constrain. `after_id` is a
/// keyset cursor comparing `(created_at, id)` so paging clears rows sharing a `created_at`.
#[derive(Debug, Clone, Copy, Default)]
pub struct LogFilter<'a> {
    pub limit: u32,
    pub after_id: Option<&'a str>,
    pub since: Option<&'a str>,
    pub until: Option<&'a str>,
    pub level: Option<&'a str>,
    pub kind: Option<&'a str>,
    pub kind_prefix: Option<&'a str>,
    pub source: Option<&'a str>,
    pub session_id: Option<&'a str>,
    pub command_id: Option<&'a str>,
    pub permission_id: Option<&'a str>,
    pub security_category: Option<SecurityCategory>,
    pub order: LogOrder,
}

impl<'a> LogFilter<'a> {
    pub fn with_limit(limit: u32) -> Self {
        Self {
            limit,
            ..Self::default()
        }
    }

    /// Rust twin of the SQL predicates in `push_event_predicates`, so live-stream consumers drop
    /// frames the durable query would not have matched. Paging fields are ignored here.
    pub fn matches(&self, event: &Event) -> bool {
        if let Some(level) = self.level
            && event.level != level
        {
            return false;
        }
        if let Some(kind) = self.kind
            && event.kind != kind
        {
            return false;
        }
        if let Some(prefix) = self.kind_prefix
            && !event.kind.starts_with(prefix)
        {
            return false;
        }
        if let Some(source) = self.source
            && event.source != source
        {
            return false;
        }
        if let Some(since) = self.since
            && event.created_at.as_str() < since
        {
            return false;
        }
        if let Some(until) = self.until
            && event.created_at.as_str() >= until
        {
            return false;
        }
        if let Some(category) = self.security_category
            && !category.kinds().iter().any(|kind| *kind == event.kind)
        {
            return false;
        }
        // Parse once, and only when a payload-probing field is set: a level/kind/source-only
        // matcher should not pay for serde_json.
        let payload = if self.session_id.is_some()
            || self.command_id.is_some()
            || self.permission_id.is_some()
        {
            serde_json::from_str::<serde_json::Value>(&event.payload_json).ok()
        } else {
            None
        };
        if let Some(session_id) = self.session_id {
            // The typed column wins; `$.session_id` covers legacy events that embedded the id.
            let column_hit = event.session_id.as_deref() == Some(session_id);
            let payload_hit = matches!(
                payload.as_ref().and_then(|value| extract_string(value, "session_id")),
                Some(value) if value == session_id
            );
            if !column_hit && !payload_hit {
                return false;
            }
        }
        if let Some(command_id) = self.command_id {
            let payload_hit = matches!(
                payload.as_ref().and_then(|value| extract_string(value, "command_id")),
                Some(value) if value == command_id
            );
            if !payload_hit {
                return false;
            }
        }
        if let Some(permission_id) = self.permission_id
            && !permission_payload_matches(event, payload.as_ref(), permission_id)
        {
            return false;
        }
        true
    }
}

/// Probe `$.permission_id` first, falling back to `$.id` only on permission-shaped rows so an
/// unrelated `$.id` cannot satisfy a permission lookup.
fn permission_payload_matches(
    event: &Event,
    payload: Option<&serde_json::Value>,
    permission_id: &str,
) -> bool {
    if let Some(value) = payload.and_then(|value| extract_string(value, "permission_id"))
        && value == permission_id
    {
        return true;
    }
    let permission_shaped = event.kind.starts_with("permission.")
        || event.kind.starts_with("permissions.")
        || event.source == "permission";
    if !permission_shaped {
        return false;
    }
    matches!(
        payload.and_then(|value| extract_string(value, "id")),
        Some(value) if value == permission_id
    )
}

fn extract_string(payload: &serde_json::Value, field: &str) -> Option<String> {
    payload
        .get(field)
        .and_then(|inner| inner.as_str())
        .map(str::to_owned)
}

/// Alias retained for the CLI's direct-SQLite log query path; new code uses `LogFilter`.
pub type EventFilter<'a> = LogFilter<'a>;

#[derive(Debug, Clone, Copy, Default)]
pub struct SessionFilter<'a> {
    pub limit: u32,
    pub after_id: Option<&'a str>,
    pub since: Option<&'a str>,
    pub until: Option<&'a str>,
    pub status: Option<&'a str>,
    pub target_id: Option<&'a str>,
    pub order: LogOrder,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CommandFilter<'a> {
    pub limit: u32,
    pub after_id: Option<&'a str>,
    pub since: Option<&'a str>,
    pub until: Option<&'a str>,
    pub status: Option<&'a str>,
    pub order: LogOrder,
}
