use crate::error::{Result, StackError};
use crate::events::EventHub;
use chrono::{SecondsFormat, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

// RFC3339 with always-9-digit subseconds so durable timestamps sort lexicographically.
// chrono's SecondsFormat::Nanos always emits 9 fractional digits, which keeps the
// ORDER BY consistent with chronological order.

/// Tables managed by migrations, paired with the migration id that introduces
/// them. Used by the pre-flight check to verify a non-fresh DB looks intact:
/// only tables introduced by migrations <= the DB's current schema version
/// are expected to exist before we run `migrate()`. Without the
/// `introduced_in` pairing, a DB at schema_version 2 would incorrectly fail
/// because `agent_capabilities` (introduced in 3) doesn't exist yet.
const MIGRATED_TABLES: &[(&str, i64)] = &[
    ("events", 1),
    ("sessions", 1),
    ("commands", 1),
    ("agent_lifecycle", 1),
    ("auth_failures", 1),
    ("installer_runs", 1),
    ("agent_capabilities", 3),
    ("prompts", 4),
    ("permission_requests", 6),
    ("permission_decisions", 6),
];

const MANIFEST_TOML: &str = include_str!("../migrations/manifest.toml");
const SQL_001_INIT: &str = include_str!("../migrations/001_init.sqlite.sql");
const SQL_002_AUTH_FAILURES_SCHEMA: &str =
    include_str!("../migrations/002_auth_failures_schema.sqlite.sql");
const SQL_003_AGENT_CAPABILITIES: &str =
    include_str!("../migrations/003_agent_capabilities.sqlite.sql");
const SQL_004_SESSIONS: &str = include_str!("../migrations/004_sessions.sqlite.sql");
const SQL_005_COMMANDS_SCHEMA: &str = include_str!("../migrations/005_commands_schema.sqlite.sql");
const SQL_006_PERMISSIONS: &str = include_str!("../migrations/006_permissions.sqlite.sql");
const SQL_007_EVENTS_SOURCE: &str = include_str!("../migrations/007_events_source.sqlite.sql");

/// Stable event-source labels. The `events.source` column (migration 007) lets
/// log queries filter writes by their origin. Default for `append_event` is
/// `system`; explicit callers should choose the closest label.
pub const EVENT_SOURCE_SYSTEM: &str = "system";
pub const EVENT_SOURCE_API: &str = "api";
pub const EVENT_SOURCE_ACP: &str = "acp";
pub const EVENT_SOURCE_COMMAND: &str = "command";
pub const EVENT_SOURCE_PERMISSION: &str = "permission";
pub const EVENT_SOURCE_CLI: &str = "cli";
/// Reserved for the `acpctl` local-agent CLI (Phase 3 batch D); no in-tree
/// writers should emit with this source today.
pub const EVENT_SOURCE_LOCAL: &str = "local";

static EVENT_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static AUTH_FAILURE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static AGENT_LIFECYCLE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static INSTALLER_RUN_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static PROMPT_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static SESSION_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static COMMAND_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static PERMISSION_REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static PERMISSION_DECISION_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub id: String,
    pub created_at: String,
    pub level: String,
    pub kind: String,
    pub message: String,
    pub payload_json: String,
    /// Origin label (`system`, `api`, `acp`, `command`, `permission`, `cli`,
    /// `local`). Added in migration 007; pre-007 rows default to `system`.
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    pub id: String,
    pub created_at: String,
    pub updated_at: String,
    pub status: String,
    pub agent_id: String,
    pub cwd: String,
    pub title: Option<String>,
    pub metadata_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSessionRecord {
    pub id: String,
    pub agent_id: String,
    pub cwd: String,
    pub title: Option<String>,
    pub metadata_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptRecord {
    pub id: String,
    pub session_id: String,
    pub created_at: String,
    pub updated_at: String,
    pub status: String,
    pub stop_reason: Option<String>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub prompt_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPromptRecord {
    pub id: String,
    pub session_id: String,
    pub prompt_json: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptStatus {
    Pending,
    Running,
    Completed,
    Errored,
    Cancelled,
}

impl PromptStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            PromptStatus::Pending => "pending",
            PromptStatus::Running => "running",
            PromptStatus::Completed => "completed",
            PromptStatus::Errored => "errored",
            PromptStatus::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRecord {
    pub id: String,
    pub created_at: String,
    pub updated_at: String,
    pub status: String,
    pub command: String,
    pub exit_status: Option<i64>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub cwd: Option<String>,
    pub env_json: Option<String>,
    pub duration_ms: Option<i64>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewCommandRecord<'a> {
    pub command: &'a str,
    pub cwd: Option<&'a str>,
    pub env_json: Option<&'a str>,
}

/// Lifecycle status of a `commands` row. The string form goes to SQLite and
/// out over the API; `CommandStatus::as_str` is the single source of truth so
/// the gateway and tests do not drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandStatus {
    Pending,
    Running,
    Exited,
    Failed,
    Canceled,
}

impl CommandStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            CommandStatus::Pending => "pending",
            CommandStatus::Running => "running",
            CommandStatus::Exited => "exited",
            CommandStatus::Failed => "failed",
            CommandStatus::Canceled => "canceled",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRequestRecord {
    pub id: String,
    pub created_at: String,
    pub updated_at: String,
    pub status: String,
    pub source: String,
    pub requester: Option<String>,
    pub subject_id: Option<String>,
    pub detail_json: String,
    pub expires_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPermissionRequest<'a> {
    pub source: &'a str,
    pub requester: Option<&'a str>,
    pub subject_id: Option<&'a str>,
    pub detail_json: &'a str,
    pub expires_at: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionDecisionRecord {
    pub id: String,
    pub request_id: String,
    pub created_at: String,
    pub decision: String,
    pub deciding_principal: Option<String>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionStatus {
    Pending,
    Approved,
    Denied,
    Expired,
    Canceled,
}

impl PermissionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            PermissionStatus::Pending => "pending",
            PermissionStatus::Approved => "approved",
            PermissionStatus::Denied => "denied",
            PermissionStatus::Expired => "expired",
            PermissionStatus::Canceled => "canceled",
        }
    }

    pub fn is_terminal(self) -> bool {
        !matches!(self, PermissionStatus::Pending)
    }
}

/// Composable filter for `events` queries. Each field is optional; absent
/// fields don't constrain the query. `after_id` is a keyset cursor: the query
/// uses `(created_at, id)` row-value comparison via a subquery so a paginated
/// scan progresses past rows sharing a `created_at` (see migration 007 indexes).
///
/// The `command_id` and `permission_id` filters rely on `json_extract` against
/// the payload JSON (`$.command_id`, `$.permission_id`). The permission
/// publisher in `src/permissions.rs` writes a `permission_id` field alongside
/// the legacy `id` field so this filter keeps working.
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthFailure {
    pub id: String,
    pub created_at: String,
    pub key_kind: String,
    pub reason: String,
    pub client_ip: Option<String>,
    pub route: Option<String>,
    pub payload_json: String,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct AuthFailureFilter<'a> {
    pub limit: u32,
    pub after_id: Option<&'a str>,
    pub since: Option<&'a str>,
    pub until: Option<&'a str>,
}

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentLifecycleEvent {
    pub id: String,
    pub created_at: String,
    pub event_kind: String,
    pub message: String,
    pub payload_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentCapabilitiesRecord {
    pub agent_id: String,
    pub captured_at: String,
    pub capabilities_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallerRun {
    pub id: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub status: String,
    pub stdout: String,
    pub stderr: String,
    pub exit_status: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallerRunInput<'a> {
    pub started_at: &'a str,
    pub finished_at: Option<&'a str>,
    pub status: &'a str,
    pub stdout: &'a str,
    pub stderr: &'a str,
    pub exit_status: Option<i32>,
}

/// Per-stream byte cap applied before INSERT to keep installer_runs rows bounded.
/// A runaway installer that streams MB to stdout would otherwise bloat SQLite.
pub const INSTALLER_OUTPUT_CAP_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StateCounts {
    pub events: i64,
    pub sessions: i64,
    pub commands: i64,
    pub auth_failures: i64,
    pub agent_lifecycle: i64,
    pub installer_runs: i64,
    pub agent_capabilities: i64,
    pub prompts: i64,
    pub permission_requests: i64,
    pub permission_decisions: i64,
}

/// Time window for `metrics_summary`. Both bounds are inclusive on `since`,
/// exclusive on `until`, RFC3339 with 9-digit subseconds (matches the
/// `current_timestamp` format used throughout the schema).
#[derive(Debug, Clone)]
pub struct MetricsWindow {
    pub since: String,
    pub until: String,
}

/// Derived per-window metrics. All counts/aggregates are scoped to the same
/// `MetricsWindow`; percentiles are `None` when the underlying sample is
/// empty. The JSON-facing shape lives in `api.rs`; this struct is the wire
/// boundary between `state` (raw SQLite aggregation) and consumers (HTTP
/// handler, CLI pretty-printer, future Supabase mirror).
#[derive(Debug, Clone)]
pub struct MetricsSummary {
    pub window: MetricsWindow,
    pub counts: StateCounts,
    pub sessions: SessionMetrics,
    pub turns: TurnMetrics,
    pub commands: CommandMetrics,
    pub permissions: PermissionMetrics,
    pub security: SecurityMetrics,
    pub api_connections: ApiConnectionMetrics,
    pub ws_connections: WsConnectionMetrics,
    pub usage: UsageMetrics,
}

#[derive(Debug, Clone, Default)]
pub struct SessionMetrics {
    pub active: i64,
    pub closed: i64,
    pub average_duration_ms: Option<i64>,
    pub p50_duration_ms: Option<i64>,
    pub p95_duration_ms: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct TurnMetrics {
    pub total: i64,
    pub by_status: std::collections::BTreeMap<String, i64>,
    pub average_per_session: Option<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct CommandMetrics {
    pub total: i64,
    pub by_status: std::collections::BTreeMap<String, i64>,
    pub average_duration_ms: Option<i64>,
    pub p50_duration_ms: Option<i64>,
    pub p95_duration_ms: Option<i64>,
    pub truncated_count: i64,
}

#[derive(Debug, Clone, Default)]
pub struct PermissionMetrics {
    pub total: i64,
    pub by_outcome: std::collections::BTreeMap<String, i64>,
    pub average_response_ms: Option<i64>,
    pub p50_response_ms: Option<i64>,
    pub p95_response_ms: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct SecurityMetrics {
    pub auth_failures: i64,
    pub by_reason: std::collections::BTreeMap<String, i64>,
    pub events_by_kind: std::collections::BTreeMap<String, i64>,
}

#[derive(Debug, Clone, Default)]
pub struct ApiConnectionMetrics {
    /// `None` when no `api.request` events were emitted in the window. The
    /// middleware that emits these events landed in 0.0.3; on a runtime that
    /// pre-dates it, callers can still tell the difference between "instrument
    /// not installed" (None) and "instrument installed, no requests" (Some(0)).
    pub request_count: Option<i64>,
    pub by_status: std::collections::BTreeMap<String, i64>,
    pub average_duration_ms: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct WsConnectionMetrics {
    pub connections_opened: Option<i64>,
    pub connections_closed: Option<i64>,
    pub average_duration_ms: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct UsageMetrics {
    pub tokens_input: Option<i64>,
    pub tokens_output: Option<i64>,
    pub context_window_max: Option<i64>,
}

pub struct StateStore {
    connection: Connection,
    /// Optional fan-out for every `append_event` write. Set via
    /// `attach_event_hub` from `acps serve`; CLI tools that open the store
    /// read-only leave it `None`.
    event_hub: Option<EventHub>,
}

#[derive(Debug, Deserialize)]
struct MigrationManifest {
    migration: Vec<ManifestEntry>,
}

#[derive(Debug, Deserialize)]
struct ManifestEntry {
    id: i64,
    name: String,
    sqlite_file: String,
}

pub fn default_state_path(home: &Path) -> PathBuf {
    home.join(".local")
        .join("share")
        .join("acp-stack")
        .join("state.sqlite")
}

impl StateStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let connection = Connection::open(path)?;
        connection.execute_batch("PRAGMA foreign_keys = ON;")?;
        Ok(Self {
            connection,
            event_hub: None,
        })
    }

    /// Attach a live `EventHub` so every `append_event` write also fans out on
    /// the `logs` topic. The daemon (`acps serve`) calls this once at startup;
    /// CLI tools that open the store for ad-hoc queries leave it unset.
    pub fn attach_event_hub(&mut self, hub: EventHub) {
        self.event_hub = Some(hub);
    }

    pub fn migrate(&self) -> Result<()> {
        self.reject_newer_schema_version()?;
        self.reject_unversioned_managed_tables()?;
        self.ensure_migrations_table()?;
        let starting_version = self.schema_version()?;
        if starting_version > 0 {
            self.assert_tables_for_version(starting_version)?;
        }

        let manifest = parse_manifest()?;
        for entry in &manifest.migration {
            // Migration DDL and the schema_migrations bookkeeping must commit together
            // so a crash between them cannot leave managed tables without a version row,
            // which would later trip reject_unversioned_managed_tables permanently.
            //
            // BEGIN IMMEDIATE acquires the write lock before checking applicability.
            // Re-checking inside that lock prevents a second process from running
            // destructive migration SQL after another process already committed it.
            let transaction =
                Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
            if migration_is_applied(&transaction, entry.id)? {
                transaction.commit()?;
                continue;
            }
            let sql = sqlite_sql_for(entry)?;
            let applied_at = current_timestamp();
            transaction.execute_batch(sql)?;
            transaction.execute(
                r#"
                INSERT INTO schema_migrations (version, name, applied_at)
                VALUES (?1, ?2, ?3)
                "#,
                params![entry.id, entry.name, applied_at],
            )?;
            transaction.commit()?;
        }

        // Defense-in-depth: even if schema_migrations claims a version is applied, the
        // underlying tables may have been dropped or never created. Surface that as a
        // clear corruption error instead of a downstream "no such table" failure.
        self.assert_required_tables_present()?;

        Ok(())
    }

    fn assert_required_tables_present(&self) -> Result<()> {
        for &(table, _) in MIGRATED_TABLES {
            if !self.table_exists(table)? {
                return Err(StackError::MissingMigratedTable { table });
            }
        }
        Ok(())
    }

    /// Like `assert_required_tables_present`, but only checks tables
    /// introduced by migrations whose id is <= `version`. Lets the pre-flight
    /// check succeed on partially-migrated databases without lowering the
    /// post-flight bar.
    fn assert_tables_for_version(&self, version: i64) -> Result<()> {
        for &(table, introduced_in) in MIGRATED_TABLES {
            if introduced_in <= version && !self.table_exists(table)? {
                return Err(StackError::MissingMigratedTable { table });
            }
        }
        Ok(())
    }

    pub fn schema_version(&self) -> Result<i64> {
        Ok(self.connection.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
            [],
            |row| row.get(0),
        )?)
    }

    /// Append an unscoped runtime event with the default source `"system"`.
    /// Use `append_event_with_source` when the caller has a more specific
    /// origin (`api`, `acp`, `command`, etc.) — log queries can filter by it.
    pub fn append_event(
        &self,
        level: &str,
        kind: &str,
        message: &str,
        payload_json: &str,
    ) -> Result<Event> {
        self.append_event_with_source(level, kind, EVENT_SOURCE_SYSTEM, message, payload_json)
    }

    pub fn append_event_with_source(
        &self,
        level: &str,
        kind: &str,
        source: &str,
        message: &str,
        payload_json: &str,
    ) -> Result<Event> {
        validate_json_payload(&self.connection, payload_json)?;
        let event = Event {
            id: next_event_id(),
            created_at: current_timestamp(),
            level: level.to_owned(),
            kind: kind.to_owned(),
            message: message.to_owned(),
            payload_json: payload_json.to_owned(),
            source: source.to_owned(),
        };

        self.connection.execute(
            r#"
            INSERT INTO events (id, created_at, level, kind, message, payload_json, source)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            "#,
            params![
                event.id,
                event.created_at,
                event.level,
                event.kind,
                event.message,
                event.payload_json,
                event.source,
            ],
        )?;

        if let Some(hub) = &self.event_hub {
            hub.publish_log_event(&event);
        }

        Ok(event)
    }

    /// Unified `events`-table query. The filter is built dynamically so each
    /// optional field translates to at most one WHERE clause. `after_id` uses
    /// a row-value comparison against `(created_at, id)` so two events that
    /// share a `created_at` still progress past the cursor in a single
    /// direction.
    pub fn query_events(&self, filter: LogFilter<'_>) -> Result<Vec<Event>> {
        let mut sql = String::from(
            "SELECT id, created_at, level, kind, message, payload_json, source FROM events WHERE 1=1",
        );
        let mut bindings: Vec<rusqlite::types::Value> = Vec::new();
        push_event_predicates(&mut sql, &mut bindings, &filter);
        sql.push_str(" ORDER BY created_at DESC, id DESC LIMIT ?");
        bindings.push(rusqlite::types::Value::Integer(i64::from(filter.limit)));
        let mut statement = self.connection.prepare(&sql)?;
        let rows =
            statement.query_map(rusqlite::params_from_iter(bindings.iter()), row_to_event)?;
        Ok(collect_events(rows)?)
    }

    /// Convenience wrapper that scopes a `LogFilter` to permission events
    /// (`kind LIKE 'permission.%'` / `'permissions.%'`). Used by
    /// `GET /v1/logs/permissions`.
    pub fn query_permission_events(&self, mut filter: LogFilter<'_>) -> Result<Vec<Event>> {
        let mut sql = String::from(
            "SELECT id, created_at, level, kind, message, payload_json, source FROM events \
             WHERE (kind LIKE 'permission.%' OR kind LIKE 'permissions.%')",
        );
        let mut bindings: Vec<rusqlite::types::Value> = Vec::new();
        // The caller may pass an explicit kind filter; honor it on top of the
        // built-in permission prefix.
        filter.kind_prefix = filter.kind_prefix.or(Some("permission."));
        // Don't double-apply the prefix below.
        let kind_prefix_was_added = filter.kind_prefix == Some("permission.");
        let filter_for_pushers = if kind_prefix_was_added {
            LogFilter {
                kind_prefix: None,
                ..filter
            }
        } else {
            filter
        };
        push_event_predicates(&mut sql, &mut bindings, &filter_for_pushers);
        sql.push_str(" ORDER BY created_at DESC, id DESC LIMIT ?");
        bindings.push(rusqlite::types::Value::Integer(i64::from(filter.limit)));
        let mut statement = self.connection.prepare(&sql)?;
        let rows =
            statement.query_map(rusqlite::params_from_iter(bindings.iter()), row_to_event)?;
        Ok(collect_events(rows)?)
    }

    /// Convenience wrapper that scopes a `LogFilter` to security events
    /// (`kind LIKE 'security.%'`). Used by `GET /v1/logs/security` alongside
    /// the dedicated `auth_failures` table.
    pub fn query_security_events(&self, filter: LogFilter<'_>) -> Result<Vec<Event>> {
        let mut sql = String::from(
            "SELECT id, created_at, level, kind, message, payload_json, source FROM events \
             WHERE kind LIKE 'security.%'",
        );
        let mut bindings: Vec<rusqlite::types::Value> = Vec::new();
        push_event_predicates(&mut sql, &mut bindings, &filter);
        sql.push_str(" ORDER BY created_at DESC, id DESC LIMIT ?");
        bindings.push(rusqlite::types::Value::Integer(i64::from(filter.limit)));
        let mut statement = self.connection.prepare(&sql)?;
        let rows =
            statement.query_map(rusqlite::params_from_iter(bindings.iter()), row_to_event)?;
        Ok(collect_events(rows)?)
    }

    pub fn query_sessions(&self, filter: SessionFilter<'_>) -> Result<Vec<SessionRecord>> {
        let mut sql = String::from(
            "SELECT id, created_at, updated_at, status, agent_id, cwd, title, metadata_json \
             FROM sessions WHERE 1=1",
        );
        let mut bindings: Vec<rusqlite::types::Value> = Vec::new();
        if let Some(since) = filter.since {
            sql.push_str(" AND updated_at >= ?");
            bindings.push(rusqlite::types::Value::Text(since.to_owned()));
        }
        if let Some(until) = filter.until {
            sql.push_str(" AND updated_at < ?");
            bindings.push(rusqlite::types::Value::Text(until.to_owned()));
        }
        if let Some(status) = filter.status {
            sql.push_str(" AND status = ?");
            bindings.push(rusqlite::types::Value::Text(status.to_owned()));
        }
        if let Some(after) = filter.after_id {
            sql.push_str(
                " AND (updated_at, id) < (SELECT updated_at, id FROM sessions WHERE id = ?)",
            );
            bindings.push(rusqlite::types::Value::Text(after.to_owned()));
        }
        sql.push_str(" ORDER BY updated_at DESC, id DESC LIMIT ?");
        bindings.push(rusqlite::types::Value::Integer(i64::from(filter.limit)));
        let mut statement = self.connection.prepare(&sql)?;
        let rows =
            statement.query_map(rusqlite::params_from_iter(bindings.iter()), row_to_session)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn get_session(&self, id: &str) -> Result<Option<SessionRecord>> {
        Ok(self
            .connection
            .query_row(
                r#"
                SELECT id, created_at, updated_at, status, agent_id, cwd, title, metadata_json
                FROM sessions
                WHERE id = ?1
                "#,
                params![id],
                row_to_session,
            )
            .optional()?)
    }

    pub fn insert_session(&self, record: NewSessionRecord) -> Result<SessionRecord> {
        validate_json_payload(&self.connection, &record.metadata_json)?;
        let now = current_timestamp();
        let row = SessionRecord {
            id: record.id,
            created_at: now.clone(),
            updated_at: now,
            status: "active".to_owned(),
            agent_id: record.agent_id,
            cwd: record.cwd,
            title: record.title,
            metadata_json: record.metadata_json,
        };
        self.connection.execute(
            r#"
            INSERT INTO sessions
                (id, created_at, updated_at, status, agent_id, cwd, title, metadata_json)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            "#,
            params![
                row.id,
                row.created_at,
                row.updated_at,
                row.status,
                row.agent_id,
                row.cwd,
                row.title,
                row.metadata_json,
            ],
        )?;
        Ok(row)
    }

    pub fn update_session_status(&self, id: &str, status: &str) -> Result<()> {
        let now = current_timestamp();
        let affected = self.connection.execute(
            r#"
            UPDATE sessions
            SET status = ?1, updated_at = ?2
            WHERE id = ?3
            "#,
            params![status, now, id],
        )?;
        if affected == 0 {
            return Err(StackError::SessionNotFound { id: id.to_owned() });
        }
        Ok(())
    }

    /// Append an event scoped to a session. Used by the ACP bridge to persist
    /// `session/update` notifications. `kind` is the dotted event kind (e.g.
    /// `session.update`); `payload_json` is the verbatim notification body.
    /// Wrapper around `append_session_event_with_source` that records the
    /// default `system` source for callers that have no better label.
    pub fn append_session_event(
        &self,
        session_id: &str,
        level: &str,
        kind: &str,
        message: &str,
        payload_json: &str,
    ) -> Result<Event> {
        self.append_session_event_with_source(
            session_id,
            level,
            kind,
            EVENT_SOURCE_SYSTEM,
            message,
            payload_json,
        )
    }

    pub fn append_session_event_with_source(
        &self,
        session_id: &str,
        level: &str,
        kind: &str,
        source: &str,
        message: &str,
        payload_json: &str,
    ) -> Result<Event> {
        validate_json_payload(&self.connection, payload_json)?;
        let event = Event {
            id: next_event_id(),
            created_at: current_timestamp(),
            level: level.to_owned(),
            kind: kind.to_owned(),
            message: message.to_owned(),
            payload_json: payload_json.to_owned(),
            source: source.to_owned(),
        };

        self.connection.execute(
            r#"
            INSERT INTO events (id, created_at, level, kind, message, payload_json, source, session_id)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            "#,
            params![
                event.id,
                event.created_at,
                event.level,
                event.kind,
                event.message,
                event.payload_json,
                event.source,
                session_id,
            ],
        )?;

        if let Some(hub) = &self.event_hub {
            hub.publish_log_event(&event);
        }

        Ok(event)
    }

    pub fn query_session_events(
        &self,
        session_id: &str,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<Event>> {
        let limit = i64::from(limit);
        match after {
            Some(after_id) => {
                // Stable ordering pairs `(created_at, id)` so two events sharing
                // a created_at still progress past the cursor. Compare on the
                // tuple instead of just id so a slow inserter cannot reorder
                // pagination across a clock tick.
                let mut statement = self.connection.prepare(
                    r#"
                    SELECT e.id, e.created_at, e.level, e.kind, e.message, e.payload_json, e.source
                    FROM events e
                    JOIN events cursor ON cursor.id = ?2
                    WHERE e.session_id = ?1
                      AND (e.created_at, e.id) > (cursor.created_at, cursor.id)
                    ORDER BY e.created_at ASC, e.id ASC
                    LIMIT ?3
                    "#,
                )?;
                let rows =
                    statement.query_map(params![session_id, after_id, limit], row_to_event)?;
                Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
            }
            None => {
                let mut statement = self.connection.prepare(
                    r#"
                    SELECT id, created_at, level, kind, message, payload_json, source
                    FROM events
                    WHERE session_id = ?1
                    ORDER BY created_at ASC, id ASC
                    LIMIT ?2
                    "#,
                )?;
                let rows = statement.query_map(params![session_id, limit], row_to_event)?;
                Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
            }
        }
    }

    pub fn insert_prompt(&self, record: NewPromptRecord) -> Result<PromptRecord> {
        validate_json_payload(&self.connection, &record.prompt_json)?;
        let now = current_timestamp();
        let row = PromptRecord {
            id: record.id,
            session_id: record.session_id,
            created_at: now.clone(),
            updated_at: now,
            status: PromptStatus::Pending.as_str().to_owned(),
            stop_reason: None,
            error_code: None,
            error_message: None,
            prompt_json: record.prompt_json,
        };
        self.connection.execute(
            r#"
            INSERT INTO prompts
                (id, session_id, created_at, updated_at, status, prompt_json)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            "#,
            params![
                row.id,
                row.session_id,
                row.created_at,
                row.updated_at,
                row.status,
                row.prompt_json,
            ],
        )?;
        Ok(row)
    }

    pub fn get_prompt(&self, id: &str) -> Result<Option<PromptRecord>> {
        Ok(self
            .connection
            .query_row(
                r#"
                SELECT id, session_id, created_at, updated_at, status,
                       stop_reason, error_code, error_message, prompt_json
                FROM prompts
                WHERE id = ?1
                "#,
                params![id],
                row_to_prompt,
            )
            .optional()?)
    }

    pub fn update_prompt_status(
        &self,
        id: &str,
        status: PromptStatus,
        stop_reason: Option<&str>,
        error_code: Option<&str>,
        error_message: Option<&str>,
    ) -> Result<()> {
        let now = current_timestamp();
        let affected = self.connection.execute(
            r#"
            UPDATE prompts
            SET status = ?1,
                updated_at = ?2,
                stop_reason = ?3,
                error_code = ?4,
                error_message = ?5
            WHERE id = ?6
            "#,
            params![
                status.as_str(),
                now,
                stop_reason,
                error_code,
                error_message,
                id
            ],
        )?;
        if affected == 0 {
            return Err(StackError::PromptNotFound { id: id.to_owned() });
        }
        Ok(())
    }

    /// Mark every `pending`/`running` prompt row as `errored` with the given
    /// reason. Called on daemon startup so prompts orphaned by a crash get a
    /// terminal status — otherwise clients polling those prompts would never
    /// see them settle. Returns the number of rows transitioned.
    pub fn reconcile_orphaned_prompts(&self, reason: &str) -> Result<usize> {
        let now = current_timestamp();
        let affected = self.connection.execute(
            r#"
            UPDATE prompts
            SET status = 'errored',
                updated_at = ?1,
                error_code = 'agent.daemon_restart',
                error_message = ?2
            WHERE status IN ('pending', 'running')
            "#,
            params![now, reason],
        )?;
        Ok(affected)
    }

    /// Same idea for `commands`: a daemon restart kills any subprocesses
    /// (`kill_on_drop` plus tokio runtime teardown), but the SQLite rows are
    /// not finalized in that path. Without this sweep, every `running` /
    /// `pending` row from the previous run is permanently stuck and a CLI/HTTP
    /// poll would never see them settle. Returns the number of rows
    /// transitioned to `failed`.
    pub fn reconcile_orphaned_commands(&self, reason: &str) -> Result<usize> {
        let now = current_timestamp();
        let _ = reason; // recorded via finished_at + a synthetic event below
        let affected = self.connection.execute(
            r#"
            UPDATE commands
            SET status = 'failed',
                updated_at = ?1,
                finished_at = COALESCE(finished_at, ?1)
            WHERE status IN ('pending', 'running')
            "#,
            params![now],
        )?;
        Ok(affected)
    }

    pub fn in_flight_prompts_for_session(&self, session_id: &str) -> Result<Vec<PromptRecord>> {
        let mut statement = self.connection.prepare(
            r#"
            SELECT id, session_id, created_at, updated_at, status,
                   stop_reason, error_code, error_message, prompt_json
            FROM prompts
            WHERE session_id = ?1 AND status IN ('pending', 'running')
            ORDER BY created_at ASC, id ASC
            "#,
        )?;
        let rows = statement.query_map(params![session_id], row_to_prompt)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn query_commands(&self, filter: CommandFilter<'_>) -> Result<Vec<CommandRecord>> {
        let mut sql = String::from(
            "SELECT id, created_at, updated_at, status, command, exit_status, \
                    started_at, finished_at, cwd, env_json, duration_ms, truncated \
             FROM commands WHERE 1=1",
        );
        let mut bindings: Vec<rusqlite::types::Value> = Vec::new();
        if let Some(since) = filter.since {
            sql.push_str(" AND updated_at >= ?");
            bindings.push(rusqlite::types::Value::Text(since.to_owned()));
        }
        if let Some(until) = filter.until {
            sql.push_str(" AND updated_at < ?");
            bindings.push(rusqlite::types::Value::Text(until.to_owned()));
        }
        if let Some(status) = filter.status {
            sql.push_str(" AND status = ?");
            bindings.push(rusqlite::types::Value::Text(status.to_owned()));
        }
        if let Some(after) = filter.after_id {
            sql.push_str(
                " AND (updated_at, id) < (SELECT updated_at, id FROM commands WHERE id = ?)",
            );
            bindings.push(rusqlite::types::Value::Text(after.to_owned()));
        }
        sql.push_str(" ORDER BY updated_at DESC, id DESC LIMIT ?");
        bindings.push(rusqlite::types::Value::Integer(i64::from(filter.limit)));
        let mut statement = self.connection.prepare(&sql)?;
        let rows =
            statement.query_map(rusqlite::params_from_iter(bindings.iter()), row_to_command)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn get_command(&self, id: &str) -> Result<Option<CommandRecord>> {
        Ok(self
            .connection
            .query_row(
                r#"
                SELECT id, created_at, updated_at, status, command, exit_status,
                       started_at, finished_at, cwd, env_json, duration_ms, truncated
                FROM commands
                WHERE id = ?1
                "#,
                params![id],
                row_to_command,
            )
            .optional()?)
    }

    /// Insert a `commands` row in the `pending` state. The gateway transitions
    /// it to `running` via `start_command` once the subprocess has been
    /// spawned, so an inserted row that never starts (e.g. a crash between
    /// INSERT and spawn) is recoverable from history.
    pub fn append_command(&self, input: NewCommandRecord<'_>) -> Result<CommandRecord> {
        if let Some(payload) = input.env_json {
            validate_json_payload(&self.connection, payload)?;
        }
        let now = current_timestamp();
        let record = CommandRecord {
            id: next_command_id(),
            created_at: now.clone(),
            updated_at: now,
            status: CommandStatus::Pending.as_str().to_owned(),
            command: input.command.to_owned(),
            exit_status: None,
            started_at: None,
            finished_at: None,
            cwd: input.cwd.map(str::to_owned),
            env_json: input.env_json.map(str::to_owned),
            duration_ms: None,
            truncated: false,
        };

        self.connection.execute(
            r#"
            INSERT INTO commands
                (id, created_at, updated_at, status, command, exit_status,
                 started_at, finished_at, cwd, env_json, duration_ms, truncated)
            VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL, NULL, ?6, ?7, NULL, 0)
            "#,
            params![
                record.id,
                record.created_at,
                record.updated_at,
                record.status,
                record.command,
                record.cwd,
                record.env_json,
            ],
        )?;

        Ok(record)
    }

    /// Move a command from `pending` to `running` and stamp `started_at`. The
    /// caller is responsible for ensuring the subprocess has actually been
    /// spawned; this only records the transition.
    pub fn start_command(&self, id: &str) -> Result<()> {
        let now = current_timestamp();
        let rows_affected = self.connection.execute(
            r#"
            UPDATE commands
            SET status = ?1, started_at = ?2, updated_at = ?2
            WHERE id = ?3
            "#,
            params![CommandStatus::Running.as_str(), now, id],
        )?;
        if rows_affected == 0 {
            return Err(StackError::CommandNotFound { id: id.to_owned() });
        }
        Ok(())
    }

    /// Record the terminal state of a command run. `status` should be one of
    /// the non-pending/non-running variants of `CommandStatus`; the caller
    /// supplies the resolved exit status (or `None` if killed by signal).
    pub fn finish_command(
        &self,
        id: &str,
        status: CommandStatus,
        exit_status: Option<i32>,
        duration_ms: Option<i64>,
    ) -> Result<()> {
        let now = current_timestamp();
        let rows_affected = self.connection.execute(
            r#"
            UPDATE commands
            SET status = ?1,
                exit_status = ?2,
                finished_at = ?3,
                updated_at = ?3,
                duration_ms = ?4
            WHERE id = ?5
            "#,
            params![status.as_str(), exit_status, now, duration_ms, id],
        )?;
        if rows_affected == 0 {
            return Err(StackError::CommandNotFound { id: id.to_owned() });
        }
        Ok(())
    }

    /// Flip the `truncated` flag on a command row. Idempotent; called when the
    /// gateway hits its per-command output cap.
    pub fn mark_command_truncated(&self, id: &str) -> Result<()> {
        let rows_affected = self.connection.execute(
            "UPDATE commands SET truncated = 1, updated_at = ?1 WHERE id = ?2",
            params![current_timestamp(), id],
        )?;
        if rows_affected == 0 {
            return Err(StackError::CommandNotFound { id: id.to_owned() });
        }
        Ok(())
    }

    /// Append a single stdout/stderr chunk as a durable event. The `events`
    /// row carries the bytes; `commands.{id}` WebSocket subscribers receive
    /// the same payload via `EventHub::publish_command_event`. `seq` lets
    /// consumers reassemble interleaved streams in original write order.
    pub fn append_command_output(
        &self,
        command_id: &str,
        stream: &str,
        seq: u64,
        chunk: &str,
    ) -> Result<Event> {
        let payload = serde_json::json!({
            "command_id": command_id,
            "stream": stream,
            "seq": seq,
            "data": chunk,
        });
        let payload_json =
            serde_json::to_string(&payload).map_err(|_| StackError::InvalidEventPayload)?;
        let kind = format!("command.{stream}");
        self.append_event_with_source("info", &kind, EVENT_SOURCE_COMMAND, "", &payload_json)
    }

    pub fn append_auth_failure(
        &self,
        key_kind: &str,
        reason: &str,
        client_ip: Option<&str>,
        route: Option<&str>,
        payload_json: &str,
    ) -> Result<AuthFailure> {
        validate_auth_failure_payload(&self.connection, payload_json)?;
        let failure = AuthFailure {
            id: next_auth_failure_id(),
            created_at: current_timestamp(),
            key_kind: key_kind.to_owned(),
            reason: reason.to_owned(),
            client_ip: client_ip.map(str::to_owned),
            route: route.map(str::to_owned),
            payload_json: payload_json.to_owned(),
        };

        self.connection.execute(
            r#"
            INSERT INTO auth_failures
                (id, created_at, key_kind, reason, client_ip, route, payload_json)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            "#,
            params![
                failure.id,
                failure.created_at,
                failure.key_kind,
                failure.reason,
                failure.client_ip,
                failure.route,
                failure.payload_json,
            ],
        )?;

        Ok(failure)
    }

    pub fn query_auth_failures(&self, filter: AuthFailureFilter<'_>) -> Result<Vec<AuthFailure>> {
        let mut sql = String::from(
            "SELECT id, created_at, key_kind, reason, client_ip, route, payload_json \
             FROM auth_failures WHERE 1=1",
        );
        let mut bindings: Vec<rusqlite::types::Value> = Vec::new();
        if let Some(since) = filter.since {
            sql.push_str(" AND created_at >= ?");
            bindings.push(rusqlite::types::Value::Text(since.to_owned()));
        }
        if let Some(until) = filter.until {
            sql.push_str(" AND created_at < ?");
            bindings.push(rusqlite::types::Value::Text(until.to_owned()));
        }
        if let Some(after) = filter.after_id {
            sql.push_str(
                " AND (created_at, id) < (SELECT created_at, id FROM auth_failures WHERE id = ?)",
            );
            bindings.push(rusqlite::types::Value::Text(after.to_owned()));
        }
        sql.push_str(" ORDER BY created_at DESC, id DESC LIMIT ?");
        bindings.push(rusqlite::types::Value::Integer(i64::from(filter.limit)));
        let mut statement = self.connection.prepare(&sql)?;
        let rows = statement.query_map(
            rusqlite::params_from_iter(bindings.iter()),
            row_to_auth_failure,
        )?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn count_auth_failures_since(&self, since: &str) -> Result<i64> {
        Ok(self.connection.query_row(
            "SELECT COUNT(*) FROM auth_failures WHERE created_at >= ?1",
            params![since],
            |row| row.get(0),
        )?)
    }

    pub fn query_agent_lifecycle(&self, limit: u32) -> Result<Vec<AgentLifecycleEvent>> {
        let limit = i64::from(limit);
        let mut statement = self.connection.prepare(
            r#"
            SELECT id, created_at, event_kind, message, payload_json
            FROM agent_lifecycle
            ORDER BY created_at DESC, id DESC
            LIMIT ?1
            "#,
        )?;
        let rows = statement.query_map(params![limit], row_to_agent_lifecycle)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn append_agent_lifecycle(
        &self,
        event_kind: &str,
        message: &str,
        payload_json: &str,
    ) -> Result<AgentLifecycleEvent> {
        // Reuse the events table's json_valid invariant via the same helper.
        // The agent_lifecycle table has its own CHECK constraint, but failing
        // here gives a clearer error than letting sqlite reject the row.
        validate_json_payload(&self.connection, payload_json)?;
        let event = AgentLifecycleEvent {
            id: next_agent_lifecycle_id(),
            created_at: current_timestamp(),
            event_kind: event_kind.to_owned(),
            message: message.to_owned(),
            payload_json: payload_json.to_owned(),
        };

        self.connection.execute(
            r#"
            INSERT INTO agent_lifecycle (id, created_at, event_kind, message, payload_json)
            VALUES (?1, ?2, ?3, ?4, ?5)
            "#,
            params![
                event.id,
                event.created_at,
                event.event_kind,
                event.message,
                event.payload_json,
            ],
        )?;

        Ok(event)
    }

    /// Upsert the latest capabilities for an agent. We keep one row per agent_id;
    /// history lives in `agent_lifecycle` (`agent.started` events). `ON CONFLICT`
    /// ensures re-initialization after a restart simply refreshes the snapshot.
    pub fn upsert_agent_capabilities(
        &self,
        agent_id: &str,
        capabilities_json: &str,
    ) -> Result<AgentCapabilitiesRecord> {
        validate_json_payload(&self.connection, capabilities_json)?;
        let record = AgentCapabilitiesRecord {
            agent_id: agent_id.to_owned(),
            captured_at: current_timestamp(),
            capabilities_json: capabilities_json.to_owned(),
        };

        self.connection.execute(
            r#"
            INSERT INTO agent_capabilities (agent_id, captured_at, capabilities_json)
            VALUES (?1, ?2, ?3)
            ON CONFLICT(agent_id) DO UPDATE SET
                captured_at = excluded.captured_at,
                capabilities_json = excluded.capabilities_json
            "#,
            params![
                record.agent_id,
                record.captured_at,
                record.capabilities_json
            ],
        )?;

        Ok(record)
    }

    pub fn latest_agent_capabilities(
        &self,
        agent_id: &str,
    ) -> Result<Option<AgentCapabilitiesRecord>> {
        Ok(self
            .connection
            .query_row(
                r#"
                SELECT agent_id, captured_at, capabilities_json
                FROM agent_capabilities
                WHERE agent_id = ?1
                "#,
                params![agent_id],
                |row| {
                    Ok(AgentCapabilitiesRecord {
                        agent_id: row.get(0)?,
                        captured_at: row.get(1)?,
                        capabilities_json: row.get(2)?,
                    })
                },
            )
            .optional()?)
    }

    /// Append a row to `installer_runs`. Caller is responsible for capping
    /// stdout/stderr at `INSTALLER_OUTPUT_CAP_BYTES`; we re-truncate here as
    /// defense-in-depth so a buggy installer module cannot bloat the table.
    pub fn append_installer_run(&self, input: InstallerRunInput<'_>) -> Result<InstallerRun> {
        let stdout = truncate_for_storage(input.stdout);
        let stderr = truncate_for_storage(input.stderr);
        let run = InstallerRun {
            id: next_installer_run_id(),
            started_at: input.started_at.to_owned(),
            finished_at: input.finished_at.map(str::to_owned),
            status: input.status.to_owned(),
            stdout: stdout.clone(),
            stderr: stderr.clone(),
            exit_status: input.exit_status,
        };

        self.connection.execute(
            r#"
            INSERT INTO installer_runs
                (id, started_at, finished_at, status, stdout, stderr, exit_status)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            "#,
            params![
                run.id,
                run.started_at,
                run.finished_at,
                run.status,
                stdout,
                stderr,
                run.exit_status,
            ],
        )?;

        Ok(run)
    }

    pub fn query_installer_runs(&self, limit: u32) -> Result<Vec<InstallerRun>> {
        let limit = i64::from(limit);
        let mut statement = self.connection.prepare(
            r#"
            SELECT id, started_at, finished_at, status, stdout, stderr, exit_status
            FROM installer_runs
            ORDER BY started_at DESC, id DESC
            LIMIT ?1
            "#,
        )?;
        let rows = statement.query_map(params![limit], |row| {
            Ok(InstallerRun {
                id: row.get(0)?,
                started_at: row.get(1)?,
                finished_at: row.get(2)?,
                status: row.get(3)?,
                stdout: row.get(4)?,
                stderr: row.get(5)?,
                exit_status: row.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn latest_event_timestamp(&self) -> Result<Option<String>> {
        Ok(self
            .connection
            .query_row(
                "SELECT created_at FROM events ORDER BY created_at DESC, id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Insert a `permission_requests` row in the `pending` state. Returns the
    /// fully-populated record so callers can publish it without re-reading.
    pub fn append_permission_request(
        &self,
        input: NewPermissionRequest<'_>,
    ) -> Result<PermissionRequestRecord> {
        validate_json_payload(&self.connection, input.detail_json)?;
        let now = current_timestamp();
        let record = PermissionRequestRecord {
            id: next_permission_request_id(),
            created_at: now.clone(),
            updated_at: now,
            status: PermissionStatus::Pending.as_str().to_owned(),
            source: input.source.to_owned(),
            requester: input.requester.map(str::to_owned),
            subject_id: input.subject_id.map(str::to_owned),
            detail_json: input.detail_json.to_owned(),
            expires_at: input.expires_at.map(str::to_owned),
        };
        self.connection.execute(
            r#"
            INSERT INTO permission_requests
                (id, created_at, updated_at, status, source,
                 requester, subject_id, detail_json, expires_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
            "#,
            params![
                record.id,
                record.created_at,
                record.updated_at,
                record.status,
                record.source,
                record.requester,
                record.subject_id,
                record.detail_json,
                record.expires_at,
            ],
        )?;
        Ok(record)
    }

    /// Transition a permission request to a terminal status. Returns the
    /// pre-update status so the caller can validate the transition.
    pub fn transition_permission_status(
        &self,
        id: &str,
        new_status: PermissionStatus,
    ) -> Result<PermissionStatus> {
        let row: Option<String> = self
            .connection
            .query_row(
                "SELECT status FROM permission_requests WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()?;
        let current = row.ok_or_else(|| StackError::PermissionNotFound { id: id.to_owned() })?;
        let current_status = parse_permission_status(&current);

        // Reject any decision attempt once the row is terminal. Two competing
        // session-key holders trying to approve the same request — or a client
        // retrying after the first approve quietly landed — must see a clear
        // "already decided" error rather than a silent success that re-fires
        // the waiter (which has already been consumed).
        if current_status.is_terminal() {
            return Err(StackError::InvalidPermissionTransition {
                id: id.to_owned(),
                from: status_str(current_status),
                to: status_str(new_status),
            });
        }

        let affected = self.connection.execute(
            r#"
            UPDATE permission_requests
            SET status = ?1, updated_at = ?2
            WHERE id = ?3
            "#,
            params![new_status.as_str(), current_timestamp(), id],
        )?;
        if affected == 0 {
            return Err(StackError::PermissionNotFound { id: id.to_owned() });
        }
        Ok(current_status)
    }

    /// Atomically transition the request to a terminal status AND insert the
    /// matching `permission_decisions` row. Used by `PermissionService` so a
    /// partial failure between the two writes cannot leave the audit trail
    /// inconsistent (terminal row with no decision row). Returns the inserted
    /// decision.
    pub fn decide_permission(
        &self,
        id: &str,
        new_status: PermissionStatus,
        deciding_principal: Option<&str>,
        reason: Option<&str>,
    ) -> Result<PermissionDecisionRecord> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let row: Option<String> = transaction
            .query_row(
                "SELECT status FROM permission_requests WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()?;
        let current = row.ok_or_else(|| StackError::PermissionNotFound { id: id.to_owned() })?;
        let current_status = parse_permission_status(&current);
        if current_status.is_terminal() {
            return Err(StackError::InvalidPermissionTransition {
                id: id.to_owned(),
                from: status_str(current_status),
                to: status_str(new_status),
            });
        }
        let affected = transaction.execute(
            r#"
            UPDATE permission_requests
            SET status = ?1, updated_at = ?2
            WHERE id = ?3
            "#,
            params![new_status.as_str(), current_timestamp(), id],
        )?;
        if affected == 0 {
            return Err(StackError::PermissionNotFound { id: id.to_owned() });
        }
        let decision = PermissionDecisionRecord {
            id: next_permission_decision_id(),
            request_id: id.to_owned(),
            created_at: current_timestamp(),
            decision: new_status.as_str().to_owned(),
            deciding_principal: deciding_principal.map(str::to_owned),
            reason: reason.map(str::to_owned),
        };
        transaction.execute(
            r#"
            INSERT INTO permission_decisions
                (id, request_id, created_at, decision, deciding_principal, reason)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            "#,
            params![
                decision.id,
                decision.request_id,
                decision.created_at,
                decision.decision,
                decision.deciding_principal,
                decision.reason,
            ],
        )?;
        transaction.commit()?;
        Ok(decision)
    }

    pub fn record_permission_decision(
        &self,
        request_id: &str,
        decision: PermissionStatus,
        deciding_principal: Option<&str>,
        reason: Option<&str>,
    ) -> Result<PermissionDecisionRecord> {
        let record = PermissionDecisionRecord {
            id: next_permission_decision_id(),
            request_id: request_id.to_owned(),
            created_at: current_timestamp(),
            decision: decision.as_str().to_owned(),
            deciding_principal: deciding_principal.map(str::to_owned),
            reason: reason.map(str::to_owned),
        };
        self.connection.execute(
            r#"
            INSERT INTO permission_decisions
                (id, request_id, created_at, decision, deciding_principal, reason)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            "#,
            params![
                record.id,
                record.request_id,
                record.created_at,
                record.decision,
                record.deciding_principal,
                record.reason,
            ],
        )?;
        Ok(record)
    }

    pub fn get_permission_request(&self, id: &str) -> Result<Option<PermissionRequestRecord>> {
        Ok(self
            .connection
            .query_row(
                r#"
                SELECT id, created_at, updated_at, status, source,
                       requester, subject_id, detail_json, expires_at
                FROM permission_requests
                WHERE id = ?1
                "#,
                params![id],
                row_to_permission_request,
            )
            .optional()?)
    }

    pub fn query_pending_permissions(&self, limit: u32) -> Result<Vec<PermissionRequestRecord>> {
        let limit = i64::from(limit);
        let mut statement = self.connection.prepare(
            r#"
            SELECT id, created_at, updated_at, status, source,
                   requester, subject_id, detail_json, expires_at
            FROM permission_requests
            WHERE status = 'pending'
            ORDER BY created_at ASC, id ASC
            LIMIT ?1
            "#,
        )?;
        let rows = statement.query_map(params![limit], row_to_permission_request)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// On daemon startup, mark every `pending` permission row as terminal so
    /// clients polling the row see it settle. ACP-source rows become
    /// `canceled` (the ACP request channel is gone after restart). Command-
    /// source rows become `expired` so the caller's understanding (the
    /// command never executed) is preserved. Returns `(canceled, expired)`.
    pub fn reconcile_orphaned_permissions(&self) -> Result<(usize, usize)> {
        // Wrap the row transitions and the matching decision inserts in one
        // transaction so the audit-trail invariant — every terminal request
        // row has a corresponding `permission_decisions` row — holds even
        // across a crash mid-reconcile. Without the decision-row inserts the
        // bulk UPDATEs would re-introduce the very inconsistency that the
        // atomic `decide_permission` helper exists to prevent.
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let now = current_timestamp();

        // ACP-source pending rows become `canceled` — the request channel is
        // gone after restart.
        let acp_ids: Vec<String> = {
            let mut statement = transaction.prepare(
                "SELECT id FROM permission_requests WHERE status = 'pending' AND source = 'acp'",
            )?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let canceled = acp_ids.len();
        for id in &acp_ids {
            transaction.execute(
                "UPDATE permission_requests SET status = 'canceled', updated_at = ?1 WHERE id = ?2",
                params![now, id],
            )?;
            let decision_id = next_permission_decision_id();
            transaction.execute(
                r#"
                INSERT INTO permission_decisions
                    (id, request_id, created_at, decision, deciding_principal, reason)
                VALUES (?1, ?2, ?3, 'canceled', 'system', 'daemon-restart')
                "#,
                params![decision_id, id, now],
            )?;
        }

        // Command-source pending rows become `expired` — the command never
        // executed, so an expired decision (rather than canceled) preserves
        // the caller's understanding that the policy timer ran out.
        let cmd_ids: Vec<String> = {
            let mut statement = transaction.prepare(
                "SELECT id FROM permission_requests WHERE status = 'pending' AND source = 'command'",
            )?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let expired = cmd_ids.len();
        for id in &cmd_ids {
            transaction.execute(
                "UPDATE permission_requests SET status = 'expired', updated_at = ?1 WHERE id = ?2",
                params![now, id],
            )?;
            let decision_id = next_permission_decision_id();
            transaction.execute(
                r#"
                INSERT INTO permission_decisions
                    (id, request_id, created_at, decision, deciding_principal, reason)
                VALUES (?1, ?2, ?3, 'expired', 'system', 'daemon-restart')
                "#,
                params![decision_id, id, now],
            )?;
        }

        transaction.commit()?;
        Ok((canceled, expired))
    }

    pub fn counts(&self) -> Result<StateCounts> {
        Ok(StateCounts {
            events: self.count_table("events")?,
            sessions: self.count_table("sessions")?,
            commands: self.count_table("commands")?,
            auth_failures: self.count_table("auth_failures")?,
            agent_lifecycle: self.count_table("agent_lifecycle")?,
            installer_runs: self.count_table("installer_runs")?,
            agent_capabilities: self.count_table("agent_capabilities")?,
            prompts: self.count_table("prompts")?,
            permission_requests: self.count_table("permission_requests")?,
            permission_decisions: self.count_table("permission_decisions")?,
        })
    }

    /// Counts scoped to a `[since, until)` window keyed on the supplied
    /// timestamp column. SQLite optimizes the range scan via the
    /// `created_at` indexes added in migration 007.
    fn count_table_in_window(
        &self,
        table: &'static str,
        timestamp_column: &'static str,
        window: &MetricsWindow,
    ) -> Result<i64> {
        let sql = format!(
            "SELECT COUNT(*) FROM {table} WHERE {timestamp_column} >= ?1 AND {timestamp_column} < ?2"
        );
        Ok(self
            .connection
            .query_row(&sql, params![window.since, window.until], |row| row.get(0))?)
    }

    /// Compute the full metrics summary for a `[since, until)` window. The
    /// caller (HTTP handler / CLI) decides the window; pure-SQLite aggregation
    /// keeps this function side-effect-free and re-runnable. Percentiles are
    /// computed in Rust because SQLite has no `percentile_cont`; the row
    /// counts in a reasonable window (hours-to-days × thousands) are small
    /// enough that loading them into memory once per query is fine.
    pub fn metrics_summary(&self, window: MetricsWindow) -> Result<MetricsSummary> {
        let counts = StateCounts {
            events: self.count_table_in_window("events", "created_at", &window)?,
            sessions: self.count_table_in_window("sessions", "created_at", &window)?,
            commands: self.count_table_in_window("commands", "created_at", &window)?,
            auth_failures: self.count_table_in_window("auth_failures", "created_at", &window)?,
            agent_lifecycle: self.count_table_in_window(
                "agent_lifecycle",
                "created_at",
                &window,
            )?,
            installer_runs: self.count_table_in_window("installer_runs", "started_at", &window)?,
            agent_capabilities: self.count_table_in_window(
                "agent_capabilities",
                "captured_at",
                &window,
            )?,
            prompts: self.count_table_in_window("prompts", "created_at", &window)?,
            permission_requests: self.count_table_in_window(
                "permission_requests",
                "created_at",
                &window,
            )?,
            permission_decisions: self.count_table_in_window(
                "permission_decisions",
                "created_at",
                &window,
            )?,
        };

        let sessions = self.session_metrics(&window)?;
        let turns = self.turn_metrics(&window, sessions.active + sessions.closed)?;
        let commands = self.command_metrics(&window)?;
        let permissions = self.permission_metrics(&window)?;
        let security = self.security_metrics(&window)?;
        let api_connections = self.api_connection_metrics(&window)?;
        let ws_connections = self.ws_connection_metrics(&window)?;
        let usage = self.usage_metrics(&window)?;

        Ok(MetricsSummary {
            window,
            counts,
            sessions,
            turns,
            commands,
            permissions,
            security,
            api_connections,
            ws_connections,
            usage,
        })
    }

    fn session_metrics(&self, window: &MetricsWindow) -> Result<SessionMetrics> {
        let active: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM sessions \
             WHERE status = 'active' AND created_at >= ?1 AND created_at < ?2",
            params![window.since, window.until],
            |row| row.get(0),
        )?;
        let closed: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM sessions \
             WHERE status != 'active' AND created_at >= ?1 AND created_at < ?2",
            params![window.since, window.until],
            |row| row.get(0),
        )?;
        let mut statement = self.connection.prepare(
            "SELECT created_at, updated_at FROM sessions \
             WHERE status != 'active' AND created_at >= ?1 AND created_at < ?2",
        )?;
        let durations: Vec<i64> = statement
            .query_map(params![window.since, window.until], |row| {
                let created_at: String = row.get(0)?;
                let updated_at: String = row.get(1)?;
                Ok(duration_ms_between(&created_at, &updated_at))
            })?
            .filter_map(|res| match res {
                Ok(Some(ms)) => Some(Ok(ms)),
                Ok(None) => None,
                Err(err) => Some(Err(err)),
            })
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(SessionMetrics {
            active,
            closed,
            average_duration_ms: average_i64(&durations),
            p50_duration_ms: percentile_i64(&durations, 0.50),
            p95_duration_ms: percentile_i64(&durations, 0.95),
        })
    }

    fn turn_metrics(&self, window: &MetricsWindow, session_count: i64) -> Result<TurnMetrics> {
        let mut by_status = std::collections::BTreeMap::new();
        let mut statement = self.connection.prepare(
            "SELECT status, COUNT(*) FROM prompts \
             WHERE created_at >= ?1 AND created_at < ?2 GROUP BY status",
        )?;
        let rows = statement.query_map(params![window.since, window.until], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        let mut total: i64 = 0;
        for entry in rows {
            let (status, count) = entry?;
            total += count;
            by_status.insert(status, count);
        }
        let average_per_session = if session_count > 0 {
            Some(total as f64 / session_count as f64)
        } else {
            None
        };
        Ok(TurnMetrics {
            total,
            by_status,
            average_per_session,
        })
    }

    fn command_metrics(&self, window: &MetricsWindow) -> Result<CommandMetrics> {
        let mut by_status = std::collections::BTreeMap::new();
        let mut statement = self.connection.prepare(
            "SELECT status, COUNT(*) FROM commands \
             WHERE created_at >= ?1 AND created_at < ?2 GROUP BY status",
        )?;
        let rows = statement.query_map(params![window.since, window.until], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        let mut total: i64 = 0;
        for entry in rows {
            let (status, count) = entry?;
            total += count;
            by_status.insert(status, count);
        }
        let truncated_count: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM commands \
             WHERE truncated = 1 AND created_at >= ?1 AND created_at < ?2",
            params![window.since, window.until],
            |row| row.get(0),
        )?;
        let mut durations_statement = self.connection.prepare(
            "SELECT duration_ms FROM commands \
             WHERE duration_ms IS NOT NULL AND created_at >= ?1 AND created_at < ?2",
        )?;
        let durations: Vec<i64> = durations_statement
            .query_map(params![window.since, window.until], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(CommandMetrics {
            total,
            by_status,
            average_duration_ms: average_i64(&durations),
            p50_duration_ms: percentile_i64(&durations, 0.50),
            p95_duration_ms: percentile_i64(&durations, 0.95),
            truncated_count,
        })
    }

    fn permission_metrics(&self, window: &MetricsWindow) -> Result<PermissionMetrics> {
        let total: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM permission_requests \
             WHERE created_at >= ?1 AND created_at < ?2",
            params![window.since, window.until],
            |row| row.get(0),
        )?;
        let mut by_outcome = std::collections::BTreeMap::new();
        let mut statement = self.connection.prepare(
            "SELECT status, COUNT(*) FROM permission_requests \
             WHERE created_at >= ?1 AND created_at < ?2 GROUP BY status",
        )?;
        let rows = statement.query_map(params![window.since, window.until], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        for entry in rows {
            let (status, count) = entry?;
            by_outcome.insert(status, count);
        }
        // Response time = time between request created_at and decision
        // created_at. Joining on the decision row covers approve/deny/cancel/
        // expired terminal outcomes; pending rows are excluded (no decision).
        let mut response_statement = self.connection.prepare(
            "SELECT r.created_at, d.created_at FROM permission_requests r \
             JOIN permission_decisions d ON d.request_id = r.id \
             WHERE r.created_at >= ?1 AND r.created_at < ?2",
        )?;
        let response_durations: Vec<i64> = response_statement
            .query_map(params![window.since, window.until], |row| {
                let req: String = row.get(0)?;
                let dec: String = row.get(1)?;
                Ok(duration_ms_between(&req, &dec))
            })?
            .filter_map(|res| match res {
                Ok(Some(ms)) => Some(Ok(ms)),
                Ok(None) => None,
                Err(err) => Some(Err(err)),
            })
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(PermissionMetrics {
            total,
            by_outcome,
            average_response_ms: average_i64(&response_durations),
            p50_response_ms: percentile_i64(&response_durations, 0.50),
            p95_response_ms: percentile_i64(&response_durations, 0.95),
        })
    }

    fn security_metrics(&self, window: &MetricsWindow) -> Result<SecurityMetrics> {
        let auth_failures: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM auth_failures \
             WHERE created_at >= ?1 AND created_at < ?2",
            params![window.since, window.until],
            |row| row.get(0),
        )?;
        let mut by_reason = std::collections::BTreeMap::new();
        let mut reason_statement = self.connection.prepare(
            "SELECT reason, COUNT(*) FROM auth_failures \
             WHERE created_at >= ?1 AND created_at < ?2 GROUP BY reason",
        )?;
        let reason_rows = reason_statement
            .query_map(params![window.since, window.until], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?;
        for entry in reason_rows {
            let (reason, count) = entry?;
            by_reason.insert(reason, count);
        }
        let mut events_by_kind = std::collections::BTreeMap::new();
        let mut kind_statement = self.connection.prepare(
            "SELECT kind, COUNT(*) FROM events \
             WHERE kind LIKE 'security.%' AND created_at >= ?1 AND created_at < ?2 GROUP BY kind",
        )?;
        let kind_rows = kind_statement.query_map(params![window.since, window.until], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        for entry in kind_rows {
            let (kind, count) = entry?;
            events_by_kind.insert(kind, count);
        }
        Ok(SecurityMetrics {
            auth_failures,
            by_reason,
            events_by_kind,
        })
    }

    fn api_connection_metrics(&self, window: &MetricsWindow) -> Result<ApiConnectionMetrics> {
        let request_count: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM events \
             WHERE kind = 'api.request' AND created_at >= ?1 AND created_at < ?2",
            params![window.since, window.until],
            |row| row.get(0),
        )?;
        if request_count == 0 {
            return Ok(ApiConnectionMetrics::default());
        }
        let mut by_status = std::collections::BTreeMap::new();
        let mut status_statement = self.connection.prepare(
            "SELECT \
                CASE \
                    WHEN CAST(json_extract(payload_json, '$.status') AS INTEGER) BETWEEN 200 AND 299 THEN '2xx' \
                    WHEN CAST(json_extract(payload_json, '$.status') AS INTEGER) BETWEEN 300 AND 399 THEN '3xx' \
                    WHEN CAST(json_extract(payload_json, '$.status') AS INTEGER) BETWEEN 400 AND 499 THEN '4xx' \
                    WHEN CAST(json_extract(payload_json, '$.status') AS INTEGER) BETWEEN 500 AND 599 THEN '5xx' \
                    ELSE 'other' \
                END AS bucket, COUNT(*) \
             FROM events \
             WHERE kind = 'api.request' AND created_at >= ?1 AND created_at < ?2 \
             GROUP BY bucket",
        )?;
        let status_rows = status_statement
            .query_map(params![window.since, window.until], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?;
        for entry in status_rows {
            let (bucket, count) = entry?;
            by_status.insert(bucket, count);
        }
        let mut duration_statement = self.connection.prepare(
            "SELECT CAST(json_extract(payload_json, '$.duration_ms') AS INTEGER) FROM events \
             WHERE kind = 'api.request' \
               AND json_extract(payload_json, '$.duration_ms') IS NOT NULL \
               AND created_at >= ?1 AND created_at < ?2",
        )?;
        let durations: Vec<i64> = duration_statement
            .query_map(params![window.since, window.until], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(ApiConnectionMetrics {
            request_count: Some(request_count),
            by_status,
            average_duration_ms: average_i64(&durations),
        })
    }

    fn ws_connection_metrics(&self, window: &MetricsWindow) -> Result<WsConnectionMetrics> {
        let opened: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM events \
             WHERE kind = 'ws.client_connected' AND created_at >= ?1 AND created_at < ?2",
            params![window.since, window.until],
            |row| row.get(0),
        )?;
        let closed: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM events \
             WHERE kind = 'ws.client_disconnected' AND created_at >= ?1 AND created_at < ?2",
            params![window.since, window.until],
            |row| row.get(0),
        )?;
        if opened == 0 && closed == 0 {
            return Ok(WsConnectionMetrics::default());
        }
        let mut duration_statement = self.connection.prepare(
            "SELECT CAST(json_extract(payload_json, '$.duration_ms') AS INTEGER) FROM events \
             WHERE kind = 'ws.client_disconnected' \
               AND json_extract(payload_json, '$.duration_ms') IS NOT NULL \
               AND created_at >= ?1 AND created_at < ?2",
        )?;
        let durations: Vec<i64> = duration_statement
            .query_map(params![window.since, window.until], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(WsConnectionMetrics {
            connections_opened: Some(opened),
            connections_closed: Some(closed),
            average_duration_ms: average_i64(&durations),
        })
    }

    fn usage_metrics(&self, window: &MetricsWindow) -> Result<UsageMetrics> {
        // Sum across all `usage.reported` events. Agents that don't emit these
        // leave every field None. The `MAX(context_window_max)` keeps the
        // largest context size seen in the window — most agents emit this as
        // a static capability per session.
        let row = self
            .connection
            .query_row(
                "SELECT \
                    SUM(CAST(json_extract(payload_json, '$.input_tokens') AS INTEGER)), \
                    SUM(CAST(json_extract(payload_json, '$.output_tokens') AS INTEGER)), \
                    MAX(CAST(json_extract(payload_json, '$.context_window_max') AS INTEGER)) \
                 FROM events \
                 WHERE kind = 'usage.reported' AND created_at >= ?1 AND created_at < ?2",
                params![window.since, window.until],
                |row| {
                    Ok(UsageMetrics {
                        tokens_input: row.get(0)?,
                        tokens_output: row.get(1)?,
                        context_window_max: row.get(2)?,
                    })
                },
            )
            .optional()?;
        Ok(row.unwrap_or_default())
    }

    fn count_table(&self, table: &'static str) -> Result<i64> {
        let sql = match table {
            "events" => "SELECT COUNT(*) FROM events",
            "sessions" => "SELECT COUNT(*) FROM sessions",
            "commands" => "SELECT COUNT(*) FROM commands",
            "auth_failures" => "SELECT COUNT(*) FROM auth_failures",
            "agent_lifecycle" => "SELECT COUNT(*) FROM agent_lifecycle",
            "installer_runs" => "SELECT COUNT(*) FROM installer_runs",
            "agent_capabilities" => "SELECT COUNT(*) FROM agent_capabilities",
            "prompts" => "SELECT COUNT(*) FROM prompts",
            "permission_requests" => "SELECT COUNT(*) FROM permission_requests",
            "permission_decisions" => "SELECT COUNT(*) FROM permission_decisions",
            _ => unreachable!("count_table only accepts known migrated tables"),
        };
        Ok(self.connection.query_row(sql, [], |row| row.get(0))?)
    }

    fn ensure_migrations_table(&self) -> Result<()> {
        self.connection.execute(
            r#"
            CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                applied_at TEXT NOT NULL
            )
            "#,
            [],
        )?;
        Ok(())
    }

    fn reject_newer_schema_version(&self) -> Result<()> {
        if !self.table_exists("schema_migrations")? {
            return Ok(());
        }

        let version = self.schema_version()?;
        let supported = latest_known_schema_version()?;
        if version > supported {
            return Err(StackError::IncompatibleStateSchema {
                found: version,
                supported,
            });
        }

        Ok(())
    }

    fn reject_unversioned_managed_tables(&self) -> Result<()> {
        let schema_version = if self.table_exists("schema_migrations")? {
            self.schema_version()?
        } else {
            0
        };
        if schema_version > 0 {
            return Ok(());
        }

        for &(table, _) in MIGRATED_TABLES {
            if self.table_exists(table)? {
                return Err(StackError::UnmanagedStateTable { table });
            }
        }

        Ok(())
    }

    fn table_exists(&self, table: &str) -> Result<bool> {
        let exists: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            params![table],
            |row| row.get(0),
        )?;
        Ok(exists > 0)
    }
}

fn parse_manifest() -> Result<MigrationManifest> {
    let manifest: MigrationManifest =
        toml::from_str(MANIFEST_TOML).map_err(StackError::MigrationManifestParse)?;
    validate_manifest_order(&manifest)?;
    Ok(manifest)
}

fn migration_is_applied(connection: &Connection, id: i64) -> Result<bool> {
    let count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM schema_migrations WHERE version = ?1",
        params![id],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

fn validate_manifest_order(manifest: &MigrationManifest) -> Result<()> {
    let mut previous: i64 = 0;
    for entry in &manifest.migration {
        if entry.id <= previous {
            return Err(StackError::InvalidManifestOrder {
                id: entry.id,
                previous,
            });
        }
        previous = entry.id;
    }
    Ok(())
}

fn sqlite_sql_for(entry: &ManifestEntry) -> Result<&'static str> {
    match (entry.id, entry.sqlite_file.as_str()) {
        (1, "001_init.sqlite.sql") => Ok(SQL_001_INIT),
        (2, "002_auth_failures_schema.sqlite.sql") => Ok(SQL_002_AUTH_FAILURES_SCHEMA),
        (3, "003_agent_capabilities.sqlite.sql") => Ok(SQL_003_AGENT_CAPABILITIES),
        (4, "004_sessions.sqlite.sql") => Ok(SQL_004_SESSIONS),
        (5, "005_commands_schema.sqlite.sql") => Ok(SQL_005_COMMANDS_SCHEMA),
        (6, "006_permissions.sqlite.sql") => Ok(SQL_006_PERMISSIONS),
        (7, "007_events_source.sqlite.sql") => Ok(SQL_007_EVENTS_SOURCE),
        _ => Err(StackError::UnknownMigrationId { id: entry.id }),
    }
}

fn latest_known_schema_version() -> Result<i64> {
    let manifest = parse_manifest()?;
    Ok(manifest
        .migration
        .iter()
        .map(|entry| entry.id)
        .max()
        .unwrap_or(0))
}

fn validate_json_payload(connection: &Connection, payload_json: &str) -> Result<()> {
    let is_valid: i64 =
        connection.query_row("SELECT json_valid(?1)", params![payload_json], |row| {
            row.get(0)
        })?;
    if is_valid == 1 {
        return Ok(());
    }

    Err(StackError::InvalidEventPayload)
}

fn validate_auth_failure_payload(connection: &Connection, payload_json: &str) -> Result<()> {
    let is_valid: i64 =
        connection.query_row("SELECT json_valid(?1)", params![payload_json], |row| {
            row.get(0)
        })?;
    if is_valid == 1 {
        return Ok(());
    }

    Err(StackError::InvalidAuthFailurePayload)
}

fn row_to_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<Event> {
    Ok(Event {
        id: row.get(0)?,
        created_at: row.get(1)?,
        level: row.get(2)?,
        kind: row.get(3)?,
        message: row.get(4)?,
        payload_json: row.get(5)?,
        source: row.get(6)?,
    })
}

fn row_to_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRecord> {
    Ok(SessionRecord {
        id: row.get(0)?,
        created_at: row.get(1)?,
        updated_at: row.get(2)?,
        status: row.get(3)?,
        agent_id: row.get(4)?,
        cwd: row.get(5)?,
        title: row.get(6)?,
        metadata_json: row.get(7)?,
    })
}

fn row_to_prompt(row: &rusqlite::Row<'_>) -> rusqlite::Result<PromptRecord> {
    Ok(PromptRecord {
        id: row.get(0)?,
        session_id: row.get(1)?,
        created_at: row.get(2)?,
        updated_at: row.get(3)?,
        status: row.get(4)?,
        stop_reason: row.get(5)?,
        error_code: row.get(6)?,
        error_message: row.get(7)?,
        prompt_json: row.get(8)?,
    })
}

fn row_to_command(row: &rusqlite::Row<'_>) -> rusqlite::Result<CommandRecord> {
    let truncated: i64 = row.get(11)?;
    Ok(CommandRecord {
        id: row.get(0)?,
        created_at: row.get(1)?,
        updated_at: row.get(2)?,
        status: row.get(3)?,
        command: row.get(4)?,
        exit_status: row.get(5)?,
        started_at: row.get(6)?,
        finished_at: row.get(7)?,
        cwd: row.get(8)?,
        env_json: row.get(9)?,
        duration_ms: row.get(10)?,
        truncated: truncated != 0,
    })
}

fn row_to_auth_failure(row: &rusqlite::Row<'_>) -> rusqlite::Result<AuthFailure> {
    Ok(AuthFailure {
        id: row.get(0)?,
        created_at: row.get(1)?,
        key_kind: row.get(2)?,
        reason: row.get(3)?,
        client_ip: row.get(4)?,
        route: row.get(5)?,
        payload_json: row.get(6)?,
    })
}

fn row_to_permission_request(row: &rusqlite::Row<'_>) -> rusqlite::Result<PermissionRequestRecord> {
    Ok(PermissionRequestRecord {
        id: row.get(0)?,
        created_at: row.get(1)?,
        updated_at: row.get(2)?,
        status: row.get(3)?,
        source: row.get(4)?,
        requester: row.get(5)?,
        subject_id: row.get(6)?,
        detail_json: row.get(7)?,
        expires_at: row.get(8)?,
    })
}

fn parse_permission_status(value: &str) -> PermissionStatus {
    match value {
        "approved" => PermissionStatus::Approved,
        "denied" => PermissionStatus::Denied,
        "expired" => PermissionStatus::Expired,
        "canceled" => PermissionStatus::Canceled,
        _ => PermissionStatus::Pending,
    }
}

fn status_str(value: PermissionStatus) -> &'static str {
    value.as_str()
}

fn row_to_agent_lifecycle(row: &rusqlite::Row<'_>) -> rusqlite::Result<AgentLifecycleEvent> {
    Ok(AgentLifecycleEvent {
        id: row.get(0)?,
        created_at: row.get(1)?,
        event_kind: row.get(2)?,
        message: row.get(3)?,
        payload_json: row.get(4)?,
    })
}

fn collect_events(
    rows: impl Iterator<Item = rusqlite::Result<Event>>,
) -> rusqlite::Result<Vec<Event>> {
    rows.collect()
}

/// Push the optional dimensions of a `LogFilter` onto a SELECT against the
/// `events` table. Callers seed `sql` with `SELECT ... FROM events WHERE 1=1`
/// (plus any baseline kind scope) and an empty `bindings` Vec, then append
/// `ORDER BY ... LIMIT ?` themselves. `limit` is *not* pushed here so callers
/// can choose ASC / DESC ordering and add their own trailing predicates.
fn push_event_predicates(
    sql: &mut String,
    bindings: &mut Vec<rusqlite::types::Value>,
    filter: &LogFilter<'_>,
) {
    if let Some(level) = filter.level {
        sql.push_str(" AND level = ?");
        bindings.push(rusqlite::types::Value::Text(level.to_owned()));
    }
    if let Some(kind) = filter.kind {
        sql.push_str(" AND kind = ?");
        bindings.push(rusqlite::types::Value::Text(kind.to_owned()));
    }
    if let Some(prefix) = filter.kind_prefix {
        // SQLite `LIKE` treats `%` as wildcard; the caller passes a literal
        // dotted prefix (e.g. `permission.`) and we add the wildcard here.
        sql.push_str(" AND kind LIKE ?");
        bindings.push(rusqlite::types::Value::Text(format!("{prefix}%")));
    }
    if let Some(source) = filter.source {
        sql.push_str(" AND source = ?");
        bindings.push(rusqlite::types::Value::Text(source.to_owned()));
    }
    if let Some(session_id) = filter.session_id {
        sql.push_str(" AND session_id = ?");
        bindings.push(rusqlite::types::Value::Text(session_id.to_owned()));
    }
    if let Some(command_id) = filter.command_id {
        // Command-lifecycle events embed the command id in their payload JSON.
        // `json_extract` is null-safe and short-circuits when the path is
        // absent, so non-command events drop out without raising.
        sql.push_str(" AND json_extract(payload_json, '$.command_id') = ?");
        bindings.push(rusqlite::types::Value::Text(command_id.to_owned()));
    }
    if let Some(permission_id) = filter.permission_id {
        // New permission events write `permission_id`; older/legacy rows and
        // previously emitted timeout events may only carry `id`. Keep that
        // fallback scoped to permission-shaped rows so unrelated payload ids
        // don't satisfy a permission lookup.
        sql.push_str(
            " AND (json_extract(payload_json, '$.permission_id') = ? \
             OR (json_extract(payload_json, '$.id') = ? \
                 AND (kind LIKE 'permission.%' OR kind LIKE 'permissions.%' OR source = 'permission')))",
        );
        bindings.push(rusqlite::types::Value::Text(permission_id.to_owned()));
        bindings.push(rusqlite::types::Value::Text(permission_id.to_owned()));
    }
    if let Some(since) = filter.since {
        sql.push_str(" AND created_at >= ?");
        bindings.push(rusqlite::types::Value::Text(since.to_owned()));
    }
    if let Some(until) = filter.until {
        sql.push_str(" AND created_at < ?");
        bindings.push(rusqlite::types::Value::Text(until.to_owned()));
    }
    if let Some(after) = filter.after_id {
        sql.push_str(" AND (created_at, id) < (SELECT created_at, id FROM events WHERE id = ?)");
        bindings.push(rusqlite::types::Value::Text(after.to_owned()));
    }
}

fn current_timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
}

/// Convert an RFC3339 timestamp to an `Option<DateTime<Utc>>`. Returns None
/// on parse failure rather than propagating an error: metrics aggregations
/// tolerate (and skip) malformed rows so a single bad input cannot blank out
/// an entire summary.
fn parse_rfc3339(input: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(input)
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc))
}

/// Return the wall-clock duration between two RFC3339 timestamps in
/// milliseconds (b - a). Returns None when either side fails to parse OR the
/// computed duration is negative (clock skew across rows).
fn duration_ms_between(start: &str, end: &str) -> Option<i64> {
    let start = parse_rfc3339(start)?;
    let end = parse_rfc3339(end)?;
    let delta = end.signed_duration_since(start).num_milliseconds();
    if delta < 0 { None } else { Some(delta) }
}

fn average_i64(values: &[i64]) -> Option<i64> {
    if values.is_empty() {
        return None;
    }
    let sum: i128 = values.iter().map(|v| *v as i128).sum();
    Some((sum / values.len() as i128) as i64)
}

/// Linear-interpolated percentile (matches numpy default). For a near-empty
/// or single-value sample this collapses to that value.
fn percentile_i64(values: &[i64], q: f64) -> Option<i64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted: Vec<i64> = values.to_vec();
    sorted.sort_unstable();
    if sorted.len() == 1 {
        return Some(sorted[0]);
    }
    let rank = q * (sorted.len() - 1) as f64;
    let lower = rank.floor() as usize;
    let upper = rank.ceil() as usize;
    if lower == upper {
        return Some(sorted[lower]);
    }
    let weight = rank - lower as f64;
    let interp = sorted[lower] as f64 + weight * (sorted[upper] - sorted[lower]) as f64;
    Some(interp.round() as i64)
}

fn next_event_id() -> String {
    // timestamp_nanos_opt() returns Option; for real clocks since 1970 it is always
    // Some and positive. Falling back to 0 keeps IDs sortable on a wildly skewed clock.
    let nanos = Utc::now().timestamp_nanos_opt().unwrap_or(0).max(0) as u128;
    let sequence = EVENT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    // PID disambiguates events from concurrent acps invocations that land in the same
    // nanosecond with the same per-process sequence value, since EVENT_SEQUENCE resets
    // on every process start.
    let pid = std::process::id();
    format!("evt_{nanos:020}_{sequence:010}_{pid:010}")
}

fn next_auth_failure_id() -> String {
    let nanos = Utc::now().timestamp_nanos_opt().unwrap_or(0).max(0) as u128;
    let sequence = AUTH_FAILURE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    format!("af_{nanos:020}_{sequence:010}_{pid:010}")
}

fn next_agent_lifecycle_id() -> String {
    let nanos = Utc::now().timestamp_nanos_opt().unwrap_or(0).max(0) as u128;
    let sequence = AGENT_LIFECYCLE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    format!("agl_{nanos:020}_{sequence:010}_{pid:010}")
}

fn next_installer_run_id() -> String {
    let nanos = Utc::now().timestamp_nanos_opt().unwrap_or(0).max(0) as u128;
    let sequence = INSTALLER_RUN_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    format!("ins_{nanos:020}_{sequence:010}_{pid:010}")
}

pub fn next_session_id() -> String {
    let nanos = Utc::now().timestamp_nanos_opt().unwrap_or(0).max(0) as u128;
    let sequence = SESSION_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    format!("sess_{nanos:020}_{sequence:010}_{pid:010}")
}

pub fn next_prompt_id() -> String {
    let nanos = Utc::now().timestamp_nanos_opt().unwrap_or(0).max(0) as u128;
    let sequence = PROMPT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    format!("prm_{nanos:020}_{sequence:010}_{pid:010}")
}

pub fn next_command_id() -> String {
    let nanos = Utc::now().timestamp_nanos_opt().unwrap_or(0).max(0) as u128;
    let sequence = COMMAND_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    format!("cmd_{nanos:020}_{sequence:010}_{pid:010}")
}

pub fn next_permission_request_id() -> String {
    let nanos = Utc::now().timestamp_nanos_opt().unwrap_or(0).max(0) as u128;
    let sequence = PERMISSION_REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    format!("perm_{nanos:020}_{sequence:010}_{pid:010}")
}

pub fn next_permission_decision_id() -> String {
    let nanos = Utc::now().timestamp_nanos_opt().unwrap_or(0).max(0) as u128;
    let sequence = PERMISSION_DECISION_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    format!("pdec_{nanos:020}_{sequence:010}_{pid:010}")
}

/// Defense-in-depth cap on installer_runs row size. The agent_installer module
/// caps as it captures; this re-truncates so a future regression upstream
/// cannot still bloat SQLite. Truncates on a UTF-8 char boundary.
fn truncate_for_storage(input: &str) -> String {
    if input.len() <= INSTALLER_OUTPUT_CAP_BYTES {
        return input.to_owned();
    }
    let mut cutoff = INSTALLER_OUTPUT_CAP_BYTES;
    while cutoff > 0 && !input.is_char_boundary(cutoff) {
        cutoff -= 1;
    }
    let mut out = String::with_capacity(cutoff + 64);
    out.push_str(&input[..cutoff]);
    let dropped = input.len() - cutoff;
    out.push_str(&format!("\n... [truncated, {dropped} bytes]"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_entries_resolve_to_embedded_sql() {
        let manifest = parse_manifest().expect("manifest must parse");
        assert!(
            !manifest.migration.is_empty(),
            "manifest must list at least one migration"
        );
        for entry in &manifest.migration {
            let sql = sqlite_sql_for(entry).expect("manifest entry must have embedded SQL");
            assert!(!sql.trim().is_empty());
        }
    }

    #[test]
    fn latest_schema_version_matches_max_manifest_id() {
        let manifest = parse_manifest().expect("manifest must parse");
        let expected = manifest
            .migration
            .iter()
            .map(|entry| entry.id)
            .max()
            .expect("manifest must list at least one migration");
        assert_eq!(latest_known_schema_version().unwrap(), expected);
    }

    #[test]
    fn validate_manifest_order_rejects_duplicate_ids() {
        let manifest = MigrationManifest {
            migration: vec![
                ManifestEntry {
                    id: 1,
                    name: "init".into(),
                    sqlite_file: "001_init.sqlite.sql".into(),
                },
                ManifestEntry {
                    id: 1,
                    name: "dup".into(),
                    sqlite_file: "001_init.sqlite.sql".into(),
                },
            ],
        };
        let error =
            validate_manifest_order(&manifest).expect_err("duplicate ids should be rejected");
        assert!(matches!(
            error,
            StackError::InvalidManifestOrder { id: 1, previous: 1 }
        ));
    }

    #[test]
    fn validate_manifest_order_rejects_out_of_order_ids() {
        let manifest = MigrationManifest {
            migration: vec![
                ManifestEntry {
                    id: 2,
                    name: "later".into(),
                    sqlite_file: "002_later.sqlite.sql".into(),
                },
                ManifestEntry {
                    id: 1,
                    name: "init".into(),
                    sqlite_file: "001_init.sqlite.sql".into(),
                },
            ],
        };
        let error =
            validate_manifest_order(&manifest).expect_err("out-of-order ids should be rejected");
        assert!(matches!(
            error,
            StackError::InvalidManifestOrder { id: 1, previous: 2 }
        ));
    }

    #[test]
    fn validate_manifest_order_rejects_non_positive_ids() {
        let manifest = MigrationManifest {
            migration: vec![ManifestEntry {
                id: 0,
                name: "zero".into(),
                sqlite_file: "000_zero.sqlite.sql".into(),
            }],
        };
        let error =
            validate_manifest_order(&manifest).expect_err("non-positive ids should be rejected");
        assert!(matches!(
            error,
            StackError::InvalidManifestOrder { id: 0, previous: 0 }
        ));
    }
}
