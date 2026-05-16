//! Agent lifecycle records, capability snapshots, and installer runs.

use crate::error::Result;
use rusqlite::{OptionalExtension, params};

use super::core::StateStore;
use super::ids::{current_timestamp, next_agent_lifecycle_id, next_installer_run_id};
use super::rows::validate_json_payload;

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

pub(super) fn row_to_agent_lifecycle(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<AgentLifecycleEvent> {
    Ok(AgentLifecycleEvent {
        id: row.get(0)?,
        created_at: row.get(1)?,
        event_kind: row.get(2)?,
        message: row.get(3)?,
        payload_json: row.get(4)?,
    })
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

impl StateStore {
    pub fn query_agent_lifecycle(&self, limit: u32) -> Result<Vec<AgentLifecycleEvent>> {
        let limit = i64::from(limit);
        let mut statement = self.connection().prepare(
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
        validate_json_payload(self.connection(), payload_json)?;
        let event = AgentLifecycleEvent {
            id: next_agent_lifecycle_id(),
            created_at: current_timestamp(),
            event_kind: event_kind.to_owned(),
            message: message.to_owned(),
            payload_json: payload_json.to_owned(),
        };

        self.persist_with_outbox("agent_lifecycle", &event.id, &event.created_at, |conn| {
            conn.execute(
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
            Ok(())
        })?;

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
        validate_json_payload(self.connection(), capabilities_json)?;
        let record = AgentCapabilitiesRecord {
            agent_id: agent_id.to_owned(),
            captured_at: current_timestamp(),
            capabilities_json: capabilities_json.to_owned(),
        };

        self.connection().execute(
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
            .connection()
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

        self.connection().execute(
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
        let mut statement = self.connection().prepare(
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
}
