//! Agent lifecycle records, capability snapshots, and installer runs.

use crate::error::{Result, StackError};
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
pub struct AgentFailureRecord {
    pub id: String,
    pub created_at: String,
    pub event_kind: String,
    pub message: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentStartedProcess {
    pub created_at: String,
    pub agent_id: Option<String>,
    pub pid: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallerRun {
    pub id: String,
    pub agent_id: Option<String>,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub status: String,
    pub stdout: String,
    pub stderr: String,
    pub exit_status: Option<i32>,
    pub step: String,
    pub version: Option<String>,
    pub operation: String,
    pub method: Option<String>,
    /// On-disk directory holding the unbounded stdout/stderr capture; the columns
    /// above are only previews.
    pub log_dir: Option<String>,
    /// Groups rows written by one `acps deps apply` invocation.
    pub apply_run_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallerRunInput<'a> {
    pub agent_id: &'a str,
    pub started_at: &'a str,
    pub finished_at: Option<&'a str>,
    pub status: &'a str,
    pub stdout: &'a str,
    pub stderr: &'a str,
    pub exit_status: Option<i32>,
    pub step: &'a str,
    pub version: Option<&'a str>,
    pub operation: &'a str,
    pub method: Option<&'a str>,
    pub log_dir: Option<&'a str>,
    pub apply_run_id: Option<&'a str>,
}

/// Final state written over a `running` installer row when its step finishes.
/// Identity fields fixed at INSERT time are deliberately not updatable here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallerRunFinish<'a> {
    /// The step's own start timestamp, re-written so the row carries the execution
    /// layer's canonical value rather than the insert-time one.
    pub started_at: &'a str,
    pub finished_at: Option<&'a str>,
    pub status: &'a str,
    pub stdout: &'a str,
    pub stderr: &'a str,
    pub exit_status: Option<i32>,
    pub version: Option<&'a str>,
    pub log_dir: Option<&'a str>,
}

pub const INSTALLER_OPERATION_INSTALL: &str = "install";
pub const INSTALLER_OPERATION_UPDATE: &str = "update";
/// In-flight step marker; a row left `running` means the daemon died mid-step.
pub const INSTALLER_STATUS_RUNNING: &str = "running";
pub const INSTALLER_METHOD_SHELL: &str = "shell";
pub const INSTALLER_METHOD_NPM: &str = "npm";
pub const INSTALLER_METHOD_GITHUB: &str = "github";
pub const INSTALLER_METHOD_APT: &str = "apt";
pub const INSTALLER_METHOD_NATIVE: &str = "native";

/// Canonical on-disk location for installer step logs, alongside `state.sqlite`.
pub fn default_installer_log_base(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".local")
        .join("share")
        .join("acp-stack")
        .join("installer-logs")
}

/// Per-stream byte cap applied before INSERT to keep installer_runs rows bounded.
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

fn row_to_installer_run(row: &rusqlite::Row<'_>) -> rusqlite::Result<InstallerRun> {
    Ok(InstallerRun {
        id: row.get(0)?,
        agent_id: row.get(1)?,
        started_at: row.get(2)?,
        finished_at: row.get(3)?,
        status: row.get(4)?,
        stdout: row.get(5)?,
        stderr: row.get(6)?,
        exit_status: row.get(7)?,
        step: row.get(8)?,
        version: row.get(9)?,
        log_dir: row.get(10)?,
        apply_run_id: row.get(11)?,
        operation: row.get(12)?,
        method: row.get(13)?,
    })
}

/// Defense-in-depth cap on installer_runs row size, on a UTF-8 char boundary.
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

    pub fn latest_agent_failure(&self, agent_id: &str) -> Result<Option<AgentFailureRecord>> {
        let mut statement = self.connection().prepare(
            r#"
            SELECT id, created_at, event_kind, message, payload_json
            FROM agent_lifecycle
	            WHERE event_kind IN (
	                'agent.spawn_failed',
	                'agent.initialize_failed',
	                'agent.restart_failed'
	            )
            ORDER BY created_at DESC, id DESC
            "#,
        )?;
        let rows = statement.query_map([], row_to_agent_lifecycle)?;
        for row in rows {
            let event = row?;
            let payload: serde_json::Value =
                serde_json::from_str(&event.payload_json).map_err(|source| {
                    StackError::StateInvalidJson {
                        field: "agent_lifecycle.payload_json",
                        reason: source.to_string(),
                    }
                })?;
            if payload.get("agent_id").and_then(serde_json::Value::as_str) != Some(agent_id) {
                continue;
            }
            let reason = payload
                .get("reason")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(&event.message)
                .to_owned();
            return Ok(Some(AgentFailureRecord {
                id: event.id,
                created_at: event.created_at,
                event_kind: event.event_kind,
                message: event.message,
                reason,
            }));
        }
        Ok(None)
    }

    pub fn append_agent_lifecycle(
        &self,
        event_kind: &str,
        message: &str,
        payload_json: &str,
    ) -> Result<AgentLifecycleEvent> {
        // The table has its own CHECK constraint; failing here is clearer.
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

    pub fn query_agent_started_processes(&self) -> Result<Vec<AgentStartedProcess>> {
        let mut statement = self.connection().prepare(
            r#"
            SELECT created_at, payload_json
            FROM agent_lifecycle
            WHERE event_kind = 'agent.started'
            ORDER BY created_at DESC, id DESC
            "#,
        )?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (created_at, payload_json) = row?;
            let payload: serde_json::Value =
                serde_json::from_str(&payload_json).map_err(|source| {
                    StackError::StateInvalidJson {
                        field: "agent_lifecycle.payload_json",
                        reason: source.to_string(),
                    }
                })?;
            let Some(raw_pid) = payload.get("pid").and_then(serde_json::Value::as_u64) else {
                continue;
            };
            let Ok(pid) = u32::try_from(raw_pid) else {
                continue;
            };
            if pid == 0 {
                continue;
            }
            out.push(AgentStartedProcess {
                created_at,
                agent_id: payload
                    .get("agent_id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
                pid,
            });
        }
        Ok(out)
    }

    /// Upsert the latest capabilities for an agent; one row per agent_id, with
    /// history living in `agent_lifecycle`.
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

    /// Append a row to `installer_runs`, re-truncating stdout/stderr defensively.
    pub fn append_installer_run(&self, input: InstallerRunInput<'_>) -> Result<InstallerRun> {
        let stdout = truncate_for_storage(input.stdout);
        let stderr = truncate_for_storage(input.stderr);
        let run = InstallerRun {
            id: next_installer_run_id(),
            agent_id: Some(input.agent_id.to_owned()),
            started_at: input.started_at.to_owned(),
            finished_at: input.finished_at.map(str::to_owned),
            status: input.status.to_owned(),
            stdout: stdout.clone(),
            stderr: stderr.clone(),
            exit_status: input.exit_status,
            step: input.step.to_owned(),
            version: input.version.map(str::to_owned),
            operation: input.operation.to_owned(),
            method: input.method.map(str::to_owned),
            log_dir: input.log_dir.map(str::to_owned),
            apply_run_id: input.apply_run_id.map(str::to_owned),
        };

        self.connection().execute(
            r#"
            INSERT INTO installer_runs
                (id, agent_id, started_at, finished_at, status, stdout, stderr, exit_status, step, version, log_dir, apply_run_id, operation, method)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
            "#,
            params![
                run.id,
                run.agent_id,
                run.started_at,
                run.finished_at,
                run.status,
                stdout,
                stderr,
                run.exit_status,
                run.step,
                run.version,
                run.log_dir,
                run.apply_run_id,
                run.operation,
                run.method,
            ],
        )?;

        Ok(run)
    }

    /// Settle a `running` installer row. Matching on `status = 'running'` keeps a
    /// double-finish from rewriting a completed audit row; zero matches errors.
    pub fn finish_installer_run(&self, id: &str, finish: InstallerRunFinish<'_>) -> Result<()> {
        let stdout = truncate_for_storage(finish.stdout);
        let stderr = truncate_for_storage(finish.stderr);
        let updated = self.connection().execute(
            r#"
            UPDATE installer_runs
            SET started_at = ?2, finished_at = ?3, status = ?4, stdout = ?5,
                stderr = ?6, exit_status = ?7, version = ?8, log_dir = ?9
            WHERE id = ?1 AND status = ?10
            "#,
            params![
                id,
                finish.started_at,
                finish.finished_at,
                finish.status,
                stdout,
                stderr,
                finish.exit_status,
                finish.version,
                finish.log_dir,
                INSTALLER_STATUS_RUNNING,
            ],
        )?;
        if updated == 0 {
            return Err(StackError::State(rusqlite::Error::StatementChangedRows(0)));
        }
        Ok(())
    }

    /// In-flight installer steps (`status = 'running'`), oldest first.
    pub fn query_active_installer_runs(&self, agent_id: Option<&str>) -> Result<Vec<InstallerRun>> {
        if let Some(agent_id) = agent_id {
            let mut statement = self.connection().prepare(
                r#"
                SELECT id, agent_id, started_at, finished_at, status, stdout, stderr, exit_status, step, version, log_dir, apply_run_id, operation, method
                FROM installer_runs
                WHERE status = ?1 AND agent_id = ?2
                ORDER BY started_at ASC, id ASC
                "#,
            )?;
            let rows = statement.query_map(
                params![INSTALLER_STATUS_RUNNING, agent_id],
                row_to_installer_run,
            )?;
            return Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?);
        }
        let mut statement = self.connection().prepare(
            r#"
            SELECT id, agent_id, started_at, finished_at, status, stdout, stderr, exit_status, step, version, log_dir, apply_run_id, operation, method
            FROM installer_runs
            WHERE status = ?1
            ORDER BY started_at ASC, id ASC
            "#,
        )?;
        let rows = statement.query_map(params![INSTALLER_STATUS_RUNNING], row_to_installer_run)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn query_installer_runs(&self, limit: u32) -> Result<Vec<InstallerRun>> {
        self.query_installer_runs_filtered(None, limit)
    }

    /// Like [`query_installer_runs`] but filters by agent id when provided.
    pub fn query_installer_runs_filtered(
        &self,
        agent_id: Option<&str>,
        limit: u32,
    ) -> Result<Vec<InstallerRun>> {
        let limit = i64::from(limit);
        if let Some(agent_id) = agent_id {
            let mut statement = self.connection().prepare(
                r#"
                SELECT id, agent_id, started_at, finished_at, status, stdout, stderr, exit_status, step, version, log_dir, apply_run_id, operation, method
                FROM installer_runs
                WHERE agent_id = ?1
                ORDER BY started_at DESC, id DESC
                LIMIT ?2
                "#,
            )?;
            let rows = statement.query_map(params![agent_id, limit], row_to_installer_run)?;
            return Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?);
        }
        let mut statement = self.connection().prepare(
            r#"
            SELECT id, agent_id, started_at, finished_at, status, stdout, stderr, exit_status, step, version, log_dir, apply_run_id, operation, method
            FROM installer_runs
            ORDER BY started_at DESC, id DESC
            LIMIT ?1
            "#,
        )?;
        let rows = statement.query_map(params![limit], row_to_installer_run)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn query_installer_runs_for_apply_run(
        &self,
        agent_id: &str,
        step: &str,
        apply_run_id: &str,
    ) -> Result<Vec<InstallerRun>> {
        let mut statement = self.connection().prepare(
            r#"
            SELECT id, agent_id, started_at, finished_at, status, stdout, stderr, exit_status, step, version, log_dir, apply_run_id, operation, method
            FROM installer_runs
            WHERE agent_id = ?1
              AND step = ?2
              AND apply_run_id = ?3
            ORDER BY started_at DESC, id DESC
            "#,
        )?;
        let rows =
            statement.query_map(params![agent_id, step, apply_run_id], row_to_installer_run)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The most recent successful installer row for each `step` of the given agent.
    pub fn latest_successful_installer_runs_for_agent(
        &self,
        agent_id: &str,
    ) -> Result<Vec<InstallerRun>> {
        let mut statement = self.connection().prepare(
            r#"
                SELECT id, agent_id, started_at, finished_at, status, stdout, stderr, exit_status, step, version, log_dir, apply_run_id, operation, method
            FROM installer_runs
            WHERE id IN (
                SELECT MAX(id) FROM installer_runs
                WHERE status = 'ran' AND agent_id = ?1
                GROUP BY step
            )
            ORDER BY step
            "#,
        )?;
        let rows = statement.query_map(params![agent_id], row_to_installer_run)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}
