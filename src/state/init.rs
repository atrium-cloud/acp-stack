//! `init_runs`/`init_steps` records backing the init orchestrator, which drives resume by
//! replaying any `succeeded` step whose postcondition still verifies as `skipped`.

use crate::error::Result;
use rusqlite::{OptionalExtension, params};

use super::core::StateStore;
use super::ids::{current_timestamp, next_init_run_id, next_init_step_id};
use super::rows::validate_json_payload;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitRunRecord {
    pub id: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub status: String,
    pub runtime_user: Option<String>,
    pub agent_id: Option<String>,
    pub args_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitStepRecord {
    pub id: String,
    pub run_id: String,
    pub ordinal: i64,
    pub kind: String,
    pub status: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub log_dir: Option<String>,
    pub error_kind: Option<String>,
    pub error_detail: Option<String>,
    pub payload_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewInitRun<'a> {
    pub runtime_user: Option<&'a str>,
    pub agent_id: Option<&'a str>,
    pub args_json: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewInitStep<'a> {
    pub run_id: &'a str,
    pub ordinal: i64,
    pub kind: &'a str,
    pub payload_json: &'a str,
}

/// Step status sentinels persisted to `init_steps.status`.
pub const INIT_STEP_PENDING: &str = "pending";
pub const INIT_STEP_RUNNING: &str = "running";
pub const INIT_STEP_SUCCEEDED: &str = "succeeded";
pub const INIT_STEP_SKIPPED: &str = "skipped";
pub const INIT_STEP_FAILED: &str = "failed";

/// Run-level status sentinels persisted to `init_runs.status`; `pending` covers not-yet-started
/// and in-progress alike, and a terminal status is only set once every step has settled.
pub const INIT_RUN_PENDING: &str = "pending";
pub const INIT_RUN_RUNNING: &str = "running";
pub const INIT_RUN_SUCCEEDED: &str = "succeeded";
pub const INIT_RUN_FAILED: &str = "failed";

fn row_to_init_run(row: &rusqlite::Row<'_>) -> rusqlite::Result<InitRunRecord> {
    Ok(InitRunRecord {
        id: row.get(0)?,
        started_at: row.get(1)?,
        finished_at: row.get(2)?,
        status: row.get(3)?,
        runtime_user: row.get(4)?,
        agent_id: row.get(5)?,
        args_json: row.get(6)?,
    })
}

fn row_to_init_step(row: &rusqlite::Row<'_>) -> rusqlite::Result<InitStepRecord> {
    Ok(InitStepRecord {
        id: row.get(0)?,
        run_id: row.get(1)?,
        ordinal: row.get(2)?,
        kind: row.get(3)?,
        status: row.get(4)?,
        started_at: row.get(5)?,
        finished_at: row.get(6)?,
        log_dir: row.get(7)?,
        error_kind: row.get(8)?,
        error_detail: row.get(9)?,
        payload_json: row.get(10)?,
    })
}

impl StateStore {
    /// Create a fresh `init_runs` row with status `pending`, returning its generated id.
    pub fn create_init_run(&self, input: NewInitRun<'_>) -> Result<InitRunRecord> {
        validate_json_payload(self.connection(), input.args_json)?;
        let record = InitRunRecord {
            id: next_init_run_id(),
            started_at: current_timestamp(),
            finished_at: None,
            status: INIT_RUN_PENDING.to_owned(),
            runtime_user: input.runtime_user.map(str::to_owned),
            agent_id: input.agent_id.map(str::to_owned),
            args_json: input.args_json.to_owned(),
        };
        self.connection().execute(
            r#"
            INSERT INTO init_runs (id, started_at, finished_at, status, runtime_user, agent_id, args_json)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            "#,
            params![
                record.id,
                record.started_at,
                record.finished_at,
                record.status,
                record.runtime_user,
                record.agent_id,
                record.args_json,
            ],
        )?;
        Ok(record)
    }

    /// Settle an init run's aggregate status, once every step has settled.
    pub fn finalize_init_run(&self, run_id: &str, status: &str) -> Result<()> {
        let finished_at = current_timestamp();
        self.connection().execute(
            r#"
            UPDATE init_runs
            SET status = ?1, finished_at = ?2
            WHERE id = ?3
            "#,
            params![status, finished_at, run_id],
        )?;
        Ok(())
    }

    pub fn latest_init_run(&self) -> Result<Option<InitRunRecord>> {
        Ok(self
            .connection()
            .query_row(
                r#"
                SELECT id, started_at, finished_at, status, runtime_user, agent_id, args_json
                FROM init_runs
                ORDER BY started_at DESC, id DESC
                LIMIT 1
                "#,
                [],
                row_to_init_run,
            )
            .optional()?)
    }

    /// Most recent non-terminal init run, scanning past newer terminal rows so a later
    /// `succeeded` run cannot shadow an earlier failed one. Backs `acps init --resume`.
    pub fn latest_non_terminal_init_run(&self) -> Result<Option<InitRunRecord>> {
        Ok(self
            .connection()
            .query_row(
                r#"
                SELECT id, started_at, finished_at, status, runtime_user, agent_id, args_json
                FROM init_runs
                WHERE status IN ('pending','running','failed')
                ORDER BY started_at DESC, id DESC
                LIMIT 1
                "#,
                [],
                row_to_init_run,
            )
            .optional()?)
    }

    pub fn lookup_init_run(&self, run_id: &str) -> Result<Option<InitRunRecord>> {
        Ok(self
            .connection()
            .query_row(
                r#"
                SELECT id, started_at, finished_at, status, runtime_user, agent_id, args_json
                FROM init_runs
                WHERE id = ?1
                "#,
                params![run_id],
                row_to_init_run,
            )
            .optional()?)
    }

    pub fn query_init_runs(&self, limit: u32) -> Result<Vec<InitRunRecord>> {
        let limit = i64::from(limit);
        let mut statement = self.connection().prepare(
            r#"
            SELECT id, started_at, finished_at, status, runtime_user, agent_id, args_json
            FROM init_runs
            ORDER BY started_at DESC, id DESC
            LIMIT ?1
            "#,
        )?;
        let rows = statement.query_map(params![limit], row_to_init_run)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Insert a `pending` step row pinned to (`run_id`, `ordinal`), whose uniqueness constraint
    /// guards against double-registration drifting step numbering.
    pub fn append_init_step(&self, input: NewInitStep<'_>) -> Result<InitStepRecord> {
        validate_json_payload(self.connection(), input.payload_json)?;
        let record = InitStepRecord {
            id: next_init_step_id(),
            run_id: input.run_id.to_owned(),
            ordinal: input.ordinal,
            kind: input.kind.to_owned(),
            status: INIT_STEP_PENDING.to_owned(),
            started_at: None,
            finished_at: None,
            log_dir: None,
            error_kind: None,
            error_detail: None,
            payload_json: input.payload_json.to_owned(),
        };
        self.connection().execute(
            r#"
            INSERT INTO init_steps
                (id, run_id, ordinal, kind, status, started_at, finished_at, log_dir,
                 error_kind, error_detail, payload_json)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
            "#,
            params![
                record.id,
                record.run_id,
                record.ordinal,
                record.kind,
                record.status,
                record.started_at,
                record.finished_at,
                record.log_dir,
                record.error_kind,
                record.error_detail,
                record.payload_json,
            ],
        )?;
        Ok(record)
    }

    /// Mark a step `running` and stamp `started_at`; verifier-only skips bypass this.
    pub fn mark_init_step_running(&self, step_id: &str) -> Result<()> {
        let started_at = current_timestamp();
        self.connection().execute(
            r#"
            UPDATE init_steps
            SET status = ?1, started_at = ?2, finished_at = NULL,
                error_kind = NULL, error_detail = NULL
            WHERE id = ?3
            "#,
            params![INIT_STEP_RUNNING, started_at, step_id],
        )?;
        Ok(())
    }

    pub fn mark_init_step_succeeded(
        &self,
        step_id: &str,
        log_dir: Option<&str>,
        payload_json: &str,
    ) -> Result<()> {
        validate_json_payload(self.connection(), payload_json)?;
        let finished_at = current_timestamp();
        self.connection().execute(
            r#"
            UPDATE init_steps
            SET status = ?1, finished_at = ?2, log_dir = ?3,
                error_kind = NULL, error_detail = NULL, payload_json = ?4
            WHERE id = ?5
            "#,
            params![
                INIT_STEP_SUCCEEDED,
                finished_at,
                log_dir,
                payload_json,
                step_id,
            ],
        )?;
        Ok(())
    }

    /// Record that a `succeeded` step's postcondition still holds and was reused. `started_at`
    /// keeps the original run's timestamp; `finished_at` records this verification.
    pub fn mark_init_step_skipped(&self, step_id: &str, payload_json: &str) -> Result<()> {
        validate_json_payload(self.connection(), payload_json)?;
        let finished_at = current_timestamp();
        self.connection().execute(
            r#"
            UPDATE init_steps
            SET status = ?1, finished_at = ?2,
                error_kind = NULL, error_detail = NULL, payload_json = ?3
            WHERE id = ?4
            "#,
            params![INIT_STEP_SKIPPED, finished_at, payload_json, step_id],
        )?;
        Ok(())
    }

    pub fn mark_init_step_failed(
        &self,
        step_id: &str,
        log_dir: Option<&str>,
        error_kind: &str,
        error_detail: &str,
        payload_json: &str,
    ) -> Result<()> {
        validate_json_payload(self.connection(), payload_json)?;
        let finished_at = current_timestamp();
        self.connection().execute(
            r#"
            UPDATE init_steps
            SET status = ?1, finished_at = ?2, log_dir = ?3,
                error_kind = ?4, error_detail = ?5, payload_json = ?6
            WHERE id = ?7
            "#,
            params![
                INIT_STEP_FAILED,
                finished_at,
                log_dir,
                error_kind,
                error_detail,
                payload_json,
                step_id,
            ],
        )?;
        Ok(())
    }

    pub fn query_init_steps(&self, run_id: &str) -> Result<Vec<InitStepRecord>> {
        let mut statement = self.connection().prepare(
            r#"
            SELECT id, run_id, ordinal, kind, status, started_at, finished_at,
                   log_dir, error_kind, error_detail, payload_json
            FROM init_steps
            WHERE run_id = ?1
            ORDER BY ordinal ASC
            "#,
        )?;
        let rows = statement.query_map(params![run_id], row_to_init_step)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Look up a recorded step, used by resume to decide between re-execute and skip.
    pub fn lookup_init_step(&self, run_id: &str, ordinal: i64) -> Result<Option<InitStepRecord>> {
        Ok(self
            .connection()
            .query_row(
                r#"
                SELECT id, run_id, ordinal, kind, status, started_at, finished_at,
                       log_dir, error_kind, error_detail, payload_json
                FROM init_steps
                WHERE run_id = ?1 AND ordinal = ?2
                "#,
                params![run_id, ordinal],
                row_to_init_step,
            )
            .optional()?)
    }
}
