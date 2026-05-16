//! SQLite schema migration runner, manifest parser, and embedded DDL.
//!
//! Migrations are applied inside a single `BEGIN IMMEDIATE` transaction per
//! step (DDL plus the matching `schema_migrations` bookkeeping row) so a
//! crash between them cannot leave managed tables without a version row.
//! Pre-flight checks reject databases that look corrupted (newer schema than
//! we know how to read, or managed tables present with no `schema_migrations`
//! at all).

use crate::error::{Result, StackError};
use rusqlite::{Connection, Transaction, TransactionBehavior, params};
use serde::Deserialize;

use super::core::StateStore;
use super::ids::current_timestamp;

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

const MANIFEST_TOML: &str = include_str!("../../migrations/manifest.toml");
const SQL_001_INIT: &str = include_str!("../../migrations/001_init.sqlite.sql");
const SQL_002_AUTH_FAILURES_SCHEMA: &str =
    include_str!("../../migrations/002_auth_failures_schema.sqlite.sql");
const SQL_003_AGENT_CAPABILITIES: &str =
    include_str!("../../migrations/003_agent_capabilities.sqlite.sql");
const SQL_004_SESSIONS: &str = include_str!("../../migrations/004_sessions.sqlite.sql");
const SQL_005_COMMANDS_SCHEMA: &str =
    include_str!("../../migrations/005_commands_schema.sqlite.sql");
const SQL_006_PERMISSIONS: &str = include_str!("../../migrations/006_permissions.sqlite.sql");
const SQL_007_EVENTS_SOURCE: &str = include_str!("../../migrations/007_events_source.sqlite.sql");

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

impl StateStore {
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
                Transaction::new_unchecked(self.connection(), TransactionBehavior::Immediate)?;
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
        Ok(self.connection().query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
            [],
            |row| row.get(0),
        )?)
    }

    fn ensure_migrations_table(&self) -> Result<()> {
        self.connection().execute(
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
        let exists: i64 = self.connection().query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            params![table],
            |row| row.get(0),
        )?;
        Ok(exists > 0)
    }
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
