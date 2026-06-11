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
    ("sink_outbox", 8),
    ("sink_failures_summary", 8),
    ("init_runs", 12),
    ("init_steps", 12),
    ("security_runs", 14),
    ("security_findings", 14),
];

const MANIFEST_TOML: &str = include_str!("../../migrations/manifest.toml");

/// One migration step in the shared logical sequence. Both dialects ship in
/// every entry so consumers (the SQLite runtime here, and operator-facing
/// schema dumps / parity tests) can look up either by id without diverging
/// from the manifest.
#[derive(Debug, Clone, Copy)]
pub struct Migration {
    pub id: i64,
    pub name: &'static str,
    pub sqlite_file: &'static str,
    pub postgres_file: &'static str,
    pub sqlite: &'static str,
    // Used by parity tests and reserved for the operator-facing schema dump.
    // Runtime never applies Postgres DDL itself, hence the lint suppression.
    #[allow(dead_code)]
    pub postgres: &'static str,
}

/// Source of truth for migrations. The TOML manifest is reduced to a
/// human-readable catalog whose entries must line up with this slice;
/// `validate_manifest_matches_registry` enforces that. New migrations are
/// added by appending an entry here AND a `[[migration]]` block to the
/// manifest in the same change.
pub(crate) const MIGRATIONS: &[Migration] = &[
    Migration {
        id: 1,
        name: "init",
        sqlite_file: "001_init.sqlite.sql",
        postgres_file: "001_init.postgres.sql",
        sqlite: include_str!("../../migrations/001_init.sqlite.sql"),
        postgres: include_str!("../../migrations/001_init.postgres.sql"),
    },
    Migration {
        id: 2,
        name: "auth_failures_schema",
        sqlite_file: "002_auth_failures_schema.sqlite.sql",
        postgres_file: "002_auth_failures_schema.postgres.sql",
        sqlite: include_str!("../../migrations/002_auth_failures_schema.sqlite.sql"),
        postgres: include_str!("../../migrations/002_auth_failures_schema.postgres.sql"),
    },
    Migration {
        id: 3,
        name: "agent_capabilities",
        sqlite_file: "003_agent_capabilities.sqlite.sql",
        postgres_file: "003_agent_capabilities.postgres.sql",
        sqlite: include_str!("../../migrations/003_agent_capabilities.sqlite.sql"),
        postgres: include_str!("../../migrations/003_agent_capabilities.postgres.sql"),
    },
    Migration {
        id: 4,
        name: "sessions",
        sqlite_file: "004_sessions.sqlite.sql",
        postgres_file: "004_sessions.postgres.sql",
        sqlite: include_str!("../../migrations/004_sessions.sqlite.sql"),
        postgres: include_str!("../../migrations/004_sessions.postgres.sql"),
    },
    Migration {
        id: 5,
        name: "commands_schema",
        sqlite_file: "005_commands_schema.sqlite.sql",
        postgres_file: "005_commands_schema.postgres.sql",
        sqlite: include_str!("../../migrations/005_commands_schema.sqlite.sql"),
        postgres: include_str!("../../migrations/005_commands_schema.postgres.sql"),
    },
    Migration {
        id: 6,
        name: "permissions",
        sqlite_file: "006_permissions.sqlite.sql",
        postgres_file: "006_permissions.postgres.sql",
        sqlite: include_str!("../../migrations/006_permissions.sqlite.sql"),
        postgres: include_str!("../../migrations/006_permissions.postgres.sql"),
    },
    Migration {
        id: 7,
        name: "events_source",
        sqlite_file: "007_events_source.sqlite.sql",
        postgres_file: "007_events_source.postgres.sql",
        sqlite: include_str!("../../migrations/007_events_source.sqlite.sql"),
        postgres: include_str!("../../migrations/007_events_source.postgres.sql"),
    },
    Migration {
        id: 8,
        name: "sink_outbox",
        sqlite_file: "008_sink_outbox.sqlite.sql",
        postgres_file: "008_sink_outbox.postgres.sql",
        sqlite: include_str!("../../migrations/008_sink_outbox.sqlite.sql"),
        postgres: include_str!("../../migrations/008_sink_outbox.postgres.sql"),
    },
    Migration {
        id: 9,
        name: "installer_runs_step",
        sqlite_file: "009_installer_runs_step.sqlite.sql",
        postgres_file: "009_installer_runs_step.postgres.sql",
        sqlite: include_str!("../../migrations/009_installer_runs_step.sqlite.sql"),
        postgres: include_str!("../../migrations/009_installer_runs_step.postgres.sql"),
    },
    Migration {
        id: 10,
        name: "installer_runs_version",
        sqlite_file: "010_installer_runs_version.sqlite.sql",
        postgres_file: "010_installer_runs_version.postgres.sql",
        sqlite: include_str!("../../migrations/010_installer_runs_version.sqlite.sql"),
        postgres: include_str!("../../migrations/010_installer_runs_version.postgres.sql"),
    },
    Migration {
        id: 11,
        name: "installer_runs_log_dir",
        sqlite_file: "011_installer_runs_log_dir.sqlite.sql",
        postgres_file: "011_installer_runs_log_dir.postgres.sql",
        sqlite: include_str!("../../migrations/011_installer_runs_log_dir.sqlite.sql"),
        postgres: include_str!("../../migrations/011_installer_runs_log_dir.postgres.sql"),
    },
    Migration {
        id: 12,
        name: "init_runs",
        sqlite_file: "012_init_runs.sqlite.sql",
        postgres_file: "012_init_runs.postgres.sql",
        sqlite: include_str!("../../migrations/012_init_runs.sqlite.sql"),
        postgres: include_str!("../../migrations/012_init_runs.postgres.sql"),
    },
    Migration {
        id: 13,
        name: "installer_runs_apply_run_id",
        sqlite_file: "013_installer_runs_apply_run_id.sqlite.sql",
        postgres_file: "013_installer_runs_apply_run_id.postgres.sql",
        sqlite: include_str!("../../migrations/013_installer_runs_apply_run_id.sqlite.sql"),
        postgres: include_str!("../../migrations/013_installer_runs_apply_run_id.postgres.sql"),
    },
    Migration {
        id: 14,
        name: "security_runs",
        sqlite_file: "014_security_runs.sqlite.sql",
        postgres_file: "014_security_runs.postgres.sql",
        sqlite: include_str!("../../migrations/014_security_runs.sqlite.sql"),
        postgres: include_str!("../../migrations/014_security_runs.postgres.sql"),
    },
    Migration {
        id: 15,
        name: "prompts_lifecycle_extension",
        sqlite_file: "015_prompts_lifecycle_extension.sqlite.sql",
        postgres_file: "015_prompts_lifecycle_extension.postgres.sql",
        sqlite: include_str!("../../migrations/015_prompts_lifecycle_extension.sqlite.sql"),
        postgres: include_str!("../../migrations/015_prompts_lifecycle_extension.postgres.sql"),
    },
    Migration {
        id: 16,
        name: "command_output_reconnect",
        sqlite_file: "016_command_output_reconnect.sqlite.sql",
        postgres_file: "016_command_output_reconnect.postgres.sql",
        sqlite: include_str!("../../migrations/016_command_output_reconnect.sqlite.sql"),
        postgres: include_str!("../../migrations/016_command_output_reconnect.postgres.sql"),
    },
    Migration {
        id: 17,
        name: "prompt_message_ids",
        sqlite_file: "017_prompt_message_ids.sqlite.sql",
        postgres_file: "017_prompt_message_ids.postgres.sql",
        sqlite: include_str!("../../migrations/017_prompt_message_ids.sqlite.sql"),
        postgres: include_str!("../../migrations/017_prompt_message_ids.postgres.sql"),
    },
    Migration {
        id: 18,
        name: "installer_runs_operation_method",
        sqlite_file: "018_installer_runs_operation_method.sqlite.sql",
        postgres_file: "018_installer_runs_operation_method.postgres.sql",
        sqlite: include_str!("../../migrations/018_installer_runs_operation_method.sqlite.sql"),
        postgres: include_str!("../../migrations/018_installer_runs_operation_method.postgres.sql"),
    },
];

/// Read-only accessor for the migration registry, including bundled Postgres
/// SQL. Consumed by parity tests today and reserved for operator-facing schema
/// dumps; the runtime applies SQLite only.
#[allow(dead_code)]
pub fn migrations_postgres_ddl() -> &'static [Migration] {
    MIGRATIONS
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
    postgres_file: String,
}

fn parse_manifest() -> Result<MigrationManifest> {
    let manifest: MigrationManifest =
        toml::from_str(MANIFEST_TOML).map_err(StackError::MigrationManifestParse)?;
    validate_manifest_order(&manifest)?;
    validate_manifest_matches_registry(&manifest)?;
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

/// The manifest is a checked catalog; the registry above is the source of
/// truth. If a manifest entry drifts from the registry the runtime refuses
/// to start so a misnamed Postgres file or a missing manifest row gets
/// caught immediately rather than producing a silently wrong Supabase dump.
fn validate_manifest_matches_registry(manifest: &MigrationManifest) -> Result<()> {
    if manifest.migration.len() != MIGRATIONS.len() {
        return Err(StackError::ManifestRegistryMismatch {
            reason: format!(
                "manifest lists {} migrations but registry has {}",
                manifest.migration.len(),
                MIGRATIONS.len()
            ),
        });
    }
    for (entry, registered) in manifest.migration.iter().zip(MIGRATIONS.iter()) {
        if entry.id != registered.id
            || entry.name != registered.name
            || entry.sqlite_file != registered.sqlite_file
            || entry.postgres_file != registered.postgres_file
        {
            return Err(StackError::ManifestRegistryMismatch {
                reason: format!(
                    "manifest entry (id={}, name={}, sqlite_file={}, postgres_file={}) \
                     does not match registry entry (id={}, name={}, sqlite_file={}, postgres_file={})",
                    entry.id,
                    entry.name,
                    entry.sqlite_file,
                    entry.postgres_file,
                    registered.id,
                    registered.name,
                    registered.sqlite_file,
                    registered.postgres_file,
                ),
            });
        }
    }
    Ok(())
}

fn latest_known_schema_version() -> i64 {
    MIGRATIONS.iter().map(|m| m.id).max().unwrap_or(0)
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

        // Manifest parse is a sanity check that the catalog still lines up with
        // the registry below; the iteration drives off MIGRATIONS, which is the
        // source of truth and the only place include_str! reaches.
        parse_manifest()?;
        for entry in MIGRATIONS {
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
            let applied_at = current_timestamp();
            transaction.execute_batch(entry.sqlite)?;
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
        let supported = latest_known_schema_version();
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
    use std::collections::{BTreeMap, BTreeSet};

    fn entry(id: i64, name: &str) -> ManifestEntry {
        ManifestEntry {
            id,
            name: name.into(),
            sqlite_file: format!("{id:03}_{name}.sqlite.sql"),
            postgres_file: format!("{id:03}_{name}.postgres.sql"),
        }
    }

    #[test]
    fn manifest_matches_registry() {
        parse_manifest().expect("manifest must parse and align with registry");
    }

    #[test]
    fn every_registry_entry_has_nonempty_dialect_sql() {
        for migration in MIGRATIONS {
            assert!(
                !migration.sqlite.trim().is_empty(),
                "missing SQLite SQL for {}",
                migration.name
            );
            assert!(
                !migration.postgres.trim().is_empty(),
                "missing Postgres SQL for {}",
                migration.name
            );
        }
    }

    #[test]
    fn latest_schema_version_matches_registry_max() {
        let expected = MIGRATIONS
            .iter()
            .map(|m| m.id)
            .max()
            .expect("registry must list at least one migration");
        assert_eq!(latest_known_schema_version(), expected);
    }

    #[test]
    fn validate_manifest_order_rejects_duplicate_ids() {
        let manifest = MigrationManifest {
            migration: vec![entry(1, "init"), entry(1, "dup")],
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
            migration: vec![entry(2, "later"), entry(1, "init")],
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
            migration: vec![entry(0, "zero")],
        };
        let error =
            validate_manifest_order(&manifest).expect_err("non-positive ids should be rejected");
        assert!(matches!(
            error,
            StackError::InvalidManifestOrder { id: 0, previous: 0 }
        ));
    }

    #[test]
    fn manifest_registry_mismatch_is_caught() {
        let mut migrations: Vec<ManifestEntry> = (1..=MIGRATIONS.len() as i64)
            .map(|id| {
                let m = &MIGRATIONS[(id - 1) as usize];
                ManifestEntry {
                    id: m.id,
                    name: m.name.to_owned(),
                    sqlite_file: m.sqlite_file.to_owned(),
                    postgres_file: m.postgres_file.to_owned(),
                }
            })
            .collect();
        if let Some(last) = migrations.last_mut() {
            last.name = format!("{}_tampered", last.name);
        }
        let manifest = MigrationManifest {
            migration: migrations,
        };
        let error = validate_manifest_matches_registry(&manifest)
            .expect_err("tampered manifest entry must be rejected");
        assert!(matches!(error, StackError::ManifestRegistryMismatch { .. }));
    }

    /// Captures CREATE TABLE / ALTER TABLE ADD COLUMN / CREATE INDEX from a
    /// dialect file in a normalized form. Comments and CHECK clauses are
    /// stripped before extraction so dialect-specific predicates (Postgres
    /// type literals, SQLite `json_valid` calls) don't confuse the parser.
    #[derive(Default, Debug)]
    struct DialectShape {
        /// Map of `table -> ordered set of columns added by this migration`.
        added_columns: BTreeMap<String, BTreeSet<String>>,
        /// Map of `index_name -> (table, columns joined by comma)`.
        indexes: BTreeMap<String, (String, String)>,
    }

    fn extract_shape(sql: &str) -> DialectShape {
        let cleaned = strip_comments_and_checks(sql);
        let mut shape = DialectShape::default();
        for table_block in iter_create_table_blocks(&cleaned) {
            let (name, cols) = table_block;
            let entry = shape.added_columns.entry(name).or_default();
            for col in cols {
                entry.insert(col);
            }
        }
        for (table, col) in iter_alter_add_column(&cleaned) {
            shape.added_columns.entry(table).or_default().insert(col);
        }
        for (name, table, cols) in iter_create_index(&cleaned) {
            shape.indexes.insert(name, (table, cols));
        }
        shape
    }

    /// Strip `--` line comments and parenthesized `CHECK (...)` predicates so
    /// the column extractor sees just `name TYPE [NOT NULL] [DEFAULT ...]`.
    fn strip_comments_and_checks(sql: &str) -> String {
        let mut out = String::with_capacity(sql.len());
        for raw_line in sql.lines() {
            let line = match raw_line.find("--") {
                Some(idx) => &raw_line[..idx],
                None => raw_line,
            };
            out.push_str(line);
            out.push('\n');
        }
        strip_parenthesized_after_keyword(&out, "CHECK")
    }

    /// Remove every `CHECK (...)` clause (handling nested parens) so the
    /// column-name extractor isn't fooled by SQL inside the predicate.
    fn strip_parenthesized_after_keyword(sql: &str, keyword: &str) -> String {
        let upper = sql.to_ascii_uppercase();
        let upper_keyword = keyword.to_ascii_uppercase();
        let mut out = String::with_capacity(sql.len());
        let bytes = sql.as_bytes();
        let upper_bytes = upper.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if upper_bytes[i..].starts_with(upper_keyword.as_bytes()) {
                let after = i + upper_keyword.len();
                let mut j = after;
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b'(' {
                    let mut depth = 1;
                    j += 1;
                    while j < bytes.len() && depth > 0 {
                        match bytes[j] {
                            b'(' => depth += 1,
                            b')' => depth -= 1,
                            _ => {}
                        }
                        j += 1;
                    }
                    i = j;
                    continue;
                }
            }
            out.push(bytes[i] as char);
            i += 1;
        }
        out
    }

    fn iter_create_table_blocks(sql: &str) -> Vec<(String, Vec<String>)> {
        let mut blocks = Vec::new();
        let upper = sql.to_ascii_uppercase();
        let mut search_from = 0;
        while let Some(rel) = upper[search_from..].find("CREATE TABLE") {
            let start = search_from + rel;
            let after_keyword = start + "CREATE TABLE".len();
            let rest = &sql[after_keyword..];
            let rest_upper = &upper[after_keyword..];
            let header_end = rest.find('(').unwrap_or(rest.len());
            let header = &rest[..header_end];
            let header_clean = header
                .replace("IF NOT EXISTS", "")
                .replace("if not exists", "")
                .to_string();
            let table = header_clean
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_string();
            let body_start = after_keyword + header_end;
            if body_start >= sql.len() || sql.as_bytes()[body_start] != b'(' {
                search_from = after_keyword;
                continue;
            }
            let mut depth = 1;
            let mut end = body_start + 1;
            while end < sql.len() && depth > 0 {
                match sql.as_bytes()[end] {
                    b'(' => depth += 1,
                    b')' => depth -= 1,
                    _ => {}
                }
                end += 1;
            }
            let body = &sql[body_start + 1..end.saturating_sub(1)];
            let cols = body
                .split(',')
                .filter_map(parse_column_name)
                .collect::<Vec<_>>();
            blocks.push((table, cols));
            search_from = end;
            let _ = rest_upper;
        }
        blocks
    }

    fn parse_column_name(fragment: &str) -> Option<String> {
        let trimmed = fragment.trim();
        if trimmed.is_empty() {
            return None;
        }
        let upper = trimmed.to_ascii_uppercase();
        // Skip table-level constraints: PRIMARY KEY (...), FOREIGN KEY (...),
        // UNIQUE (...), CONSTRAINT name ..., CHECK (...) (CHECK already
        // stripped, but FOREIGN KEY constructs survive).
        for prefix in [
            "PRIMARY KEY",
            "FOREIGN KEY",
            "UNIQUE",
            "CONSTRAINT",
            "CHECK",
        ] {
            if upper.starts_with(prefix) {
                return None;
            }
        }
        let name = trimmed.split_whitespace().next()?;
        Some(name.trim_matches('"').to_string())
    }

    fn iter_alter_add_column(sql: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let upper = sql.to_ascii_uppercase();
        let mut search_from = 0;
        let needle = "ALTER TABLE";
        while let Some(rel) = upper[search_from..].find(needle) {
            let start = search_from + rel;
            let after = start + needle.len();
            let rest = &sql[after..];
            let mut tokens = rest.split_whitespace();
            let table = match tokens.next() {
                Some(t) => t.to_string(),
                None => break,
            };
            // Re-anchor on the substring starting at the table name to find the
            // `ADD COLUMN <name>` triple that follows.
            let table_offset = rest.find(&table).unwrap_or(0);
            let after_table = &rest[table_offset + table.len()..];
            let after_upper = after_table.to_ascii_uppercase();
            let advance = after + table_offset + table.len();
            if let Some(add_rel) = after_upper.find("ADD COLUMN") {
                let after_add = &after_table[add_rel + "ADD COLUMN".len()..];
                if let Some(name) = after_add.split_whitespace().next() {
                    out.push((
                        table.trim_matches('"').to_string(),
                        name.trim_matches('"').to_string(),
                    ));
                }
            }
            search_from = advance;
        }
        out
    }

    fn iter_create_index(sql: &str) -> Vec<(String, String, String)> {
        let mut out = Vec::new();
        let upper = sql.to_ascii_uppercase();
        let mut search_from = 0;
        let needle = "CREATE INDEX";
        while let Some(rel) = upper[search_from..].find(needle) {
            let start = search_from + rel;
            let after = start + needle.len();
            let rest = &sql[after..];
            let cleaned = rest
                .replacen(" IF NOT EXISTS", "", 1)
                .replacen(" if not exists", "", 1);
            let mut iter = cleaned.split_whitespace();
            let name = iter.next().unwrap_or("").to_string();
            let on = iter.next().unwrap_or("");
            if !on.eq_ignore_ascii_case("ON") {
                search_from = after;
                continue;
            }
            let after_on_pos = cleaned
                .find(" ON ")
                .or_else(|| cleaned.find(" on "))
                .unwrap_or(0);
            let after_on = &cleaned[after_on_pos + 4..];
            let paren_pos = match after_on.find('(') {
                Some(p) => p,
                None => {
                    search_from = after;
                    continue;
                }
            };
            let table = after_on[..paren_pos].trim().trim_matches('"').to_string();
            let close = match after_on[paren_pos + 1..].find(')') {
                Some(c) => c,
                None => {
                    search_from = after;
                    continue;
                }
            };
            let cols_raw = &after_on[paren_pos + 1..paren_pos + 1 + close];
            let cols = normalize_index_columns(cols_raw);
            out.push((name, table, cols));
            search_from = after + paren_pos + close;
        }
        out
    }

    /// Drop direction markers (`ASC`/`DESC`) and whitespace so two dialects
    /// that disagree only on case or spacing still compare equal.
    fn normalize_index_columns(raw: &str) -> String {
        raw.split(',')
            .map(|chunk| {
                chunk
                    .split_whitespace()
                    .filter(|tok| {
                        let u = tok.to_ascii_uppercase();
                        u != "ASC" && u != "DESC"
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
                    .trim_matches('"')
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join(",")
    }

    #[test]
    fn dialects_have_matching_tables_columns_and_indexes_per_migration() {
        // Use the public accessor so any future caller of
        // migrations_postgres_ddl() exercises the same invariant.
        let registry = migrations_postgres_ddl();
        assert_eq!(registry.len(), MIGRATIONS.len());
        for migration in registry {
            let sqlite = extract_shape(migration.sqlite);
            let postgres = extract_shape(migration.postgres);
            assert_eq!(
                sqlite.added_columns, postgres.added_columns,
                "table/column mismatch in migration {} ({})",
                migration.id, migration.name
            );
            assert_eq!(
                sqlite.indexes, postgres.indexes,
                "index mismatch in migration {} ({})",
                migration.id, migration.name
            );
        }
    }
}
