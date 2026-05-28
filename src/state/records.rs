//! Shared filter DTOs for paginated reads across multiple domain tables.
//!
//! Domain-specific filters (e.g. `AuthFailureFilter`) live with their domain
//! file; what's here is the cross-cutting `LogFilter` used by the unified
//! `events` query path and the per-domain session/command filters that share
//! the same shape.

use super::events::Event;
use super::security_category::SecurityCategory;

/// Sort direction for log queries. `Desc` is the default (newest-first) and
/// matches the historical behavior; `Asc` is opt-in for follow-mode backfill
/// and for callers that want to walk forward through history.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum LogOrder {
    #[default]
    Desc,
    Asc,
}

impl LogOrder {
    /// SQL direction keyword used by both `ORDER BY` and the keyset cursor
    /// comparison operator that this direction implies.
    pub(super) fn sql_keyword(self) -> &'static str {
        match self {
            LogOrder::Desc => "DESC",
            LogOrder::Asc => "ASC",
        }
    }
}

/// Composable filter for `events` queries. Each field is optional; absent
/// fields don't constrain the query. `after_id` is a keyset cursor: the query
/// uses `(created_at, id)` row-value comparison via a subquery so a paginated
/// scan progresses past rows sharing a `created_at` (see migration 007 indexes).
///
/// The `command_id` and `permission_id` filters rely on `json_extract` against
/// the payload JSON (`$.command_id`, `$.permission_id`). The permission
/// publisher in `src/runtime/mediation/permissions.rs` writes a `permission_id` field
/// alongside the legacy `id` field so this filter keeps working.
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

    /// Re-implements the SQL predicates in `push_event_predicates` as a Rust
    /// matcher so live-stream consumers (WebSocket fanout, `acps logs query
    /// --follow`) can drop frames that wouldn't have matched the durable
    /// query. `limit`, `after_id`, and `order` are paging concerns and are
    /// intentionally ignored here; live frames are not paginated.
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
        // Parse the payload exactly once and only when one of the three
        // payload-probing fields is actually set. Avoids paying serde_json
        // for the common case of a matcher that only uses level/kind/source.
        let payload = if self.session_id.is_some()
            || self.command_id.is_some()
            || self.permission_id.is_some()
        {
            serde_json::from_str::<serde_json::Value>(&event.payload_json).ok()
        } else {
            None
        };
        if let Some(session_id) = self.session_id {
            // Prefer the typed column (modern writes use
            // `append_session_event_with_source`); fall back to `$.session_id`
            // in the payload for legacy events that embedded the id there.
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

/// Probe `$.permission_id` first (the canonical field written by the modern
/// publisher), falling back to `$.id` only on permission-shaped rows so an
/// unrelated `$.id` cannot satisfy a permission lookup. Mirrors the SQL clause
/// in `rows::push_event_predicates`.
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

/// Backward-compatible alias retained for the CLI's direct-SQLite log query
/// path that pre-dated the unified filter. New code should use `LogFilter`.
pub type EventFilter<'a> = LogFilter<'a>;

#[derive(Debug, Clone, Copy, Default)]
pub struct SessionFilter<'a> {
    pub limit: u32,
    pub after_id: Option<&'a str>,
    pub since: Option<&'a str>,
    pub until: Option<&'a str>,
    pub status: Option<&'a str>,
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
