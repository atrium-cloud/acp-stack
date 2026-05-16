//! Shared filter DTOs for paginated reads across multiple domain tables.
//!
//! Domain-specific filters (e.g. `AuthFailureFilter`) live with their domain
//! file; what's here is the cross-cutting `LogFilter` used by the unified
//! `events` query path and the per-domain session/command filters that share
//! the same shape.

/// Composable filter for `events` queries. Each field is optional; absent
/// fields don't constrain the query. `after_id` is a keyset cursor: the query
/// uses `(created_at, id)` row-value comparison via a subquery so a paginated
/// scan progresses past rows sharing a `created_at` (see migration 007 indexes).
///
/// The `command_id` and `permission_id` filters rely on `json_extract` against
/// the payload JSON (`$.command_id`, `$.permission_id`). The permission
/// publisher in `src/runtime/permissions.rs` writes a `permission_id` field
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
}

impl<'a> LogFilter<'a> {
    pub fn with_limit(limit: u32) -> Self {
        Self {
            limit,
            ..Self::default()
        }
    }
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
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CommandFilter<'a> {
    pub limit: u32,
    pub after_id: Option<&'a str>,
    pub since: Option<&'a str>,
    pub until: Option<&'a str>,
    pub status: Option<&'a str>,
}
