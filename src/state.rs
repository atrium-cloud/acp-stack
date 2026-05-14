use crate::error::{Result, StackError};
use chrono::{SecondsFormat, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

// RFC3339 with always-9-digit subseconds so durable timestamps sort lexicographically.
// chrono's SecondsFormat::Nanos always emits 9 fractional digits, which keeps the
// ORDER BY consistent with chronological order.

const MIGRATED_TABLES: &[&str] = &[
    "events",
    "sessions",
    "commands",
    "agent_lifecycle",
    "auth_failures",
    "installer_runs",
];

const MANIFEST_TOML: &str = include_str!("../migrations/manifest.toml");
const SQL_001_INIT: &str = include_str!("../migrations/001_init.sqlite.sql");

static EVENT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub id: String,
    pub created_at: String,
    pub level: String,
    pub kind: String,
    pub message: String,
    pub payload_json: String,
}

#[derive(Debug, Clone, Copy)]
pub struct EventFilter<'a> {
    pub limit: u32,
    pub level: Option<&'a str>,
}

pub struct StateStore {
    connection: Connection,
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
        Ok(Self { connection })
    }

    pub fn migrate(&self) -> Result<()> {
        self.reject_newer_schema_version()?;
        self.reject_unversioned_managed_tables()?;
        self.ensure_migrations_table()?;

        let manifest = parse_manifest()?;
        for entry in &manifest.migration {
            if self.is_applied(entry.id)? {
                continue;
            }
            let sql = sqlite_sql_for(entry)?;
            let applied_at = current_timestamp();
            // Migration DDL and the schema_migrations bookkeeping must commit together
            // so a crash between them cannot leave managed tables without a version row,
            // which would later trip reject_unversioned_managed_tables permanently.
            let transaction = self.connection.unchecked_transaction()?;
            transaction.execute_batch(sql)?;
            // OR IGNORE keeps concurrent acps invocations from clashing on the primary
            // key: if a second process wins the race after our is_applied check, the
            // schema_migrations row is already present and we no-op the insert.
            transaction.execute(
                r#"
                INSERT OR IGNORE INTO schema_migrations (version, name, applied_at)
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
        for &table in MIGRATED_TABLES {
            if !self.table_exists(table)? {
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

    pub fn append_event(
        &self,
        level: &str,
        kind: &str,
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
        };

        self.connection.execute(
            r#"
            INSERT INTO events (id, created_at, level, kind, message, payload_json)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            "#,
            params![
                event.id,
                event.created_at,
                event.level,
                event.kind,
                event.message,
                event.payload_json
            ],
        )?;

        Ok(event)
    }

    pub fn query_events(&self, filter: EventFilter<'_>) -> Result<Vec<Event>> {
        let limit = i64::from(filter.limit);
        match filter.level {
            Some(level) => {
                let mut statement = self.connection.prepare(
                    r#"
                    SELECT id, created_at, level, kind, message, payload_json
                    FROM events
                    WHERE level = ?1
                    ORDER BY created_at DESC, id DESC
                    LIMIT ?2
                    "#,
                )?;
                let rows = statement.query_map(params![level, limit], row_to_event)?;
                Ok(collect_events(rows)?)
            }
            None => {
                let mut statement = self.connection.prepare(
                    r#"
                    SELECT id, created_at, level, kind, message, payload_json
                    FROM events
                    ORDER BY created_at DESC, id DESC
                    LIMIT ?1
                    "#,
                )?;
                let rows = statement.query_map(params![limit], row_to_event)?;
                Ok(collect_events(rows)?)
            }
        }
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

    fn is_applied(&self, id: i64) -> Result<bool> {
        let count: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM schema_migrations WHERE version = ?1",
            params![id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
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

        for &table in MIGRATED_TABLES {
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

fn row_to_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<Event> {
    Ok(Event {
        id: row.get(0)?,
        created_at: row.get(1)?,
        level: row.get(2)?,
        kind: row.get(3)?,
        message: row.get(4)?,
        payload_json: row.get(5)?,
    })
}

fn collect_events(
    rows: impl Iterator<Item = rusqlite::Result<Event>>,
) -> rusqlite::Result<Vec<Event>> {
    rows.collect()
}

fn current_timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
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
