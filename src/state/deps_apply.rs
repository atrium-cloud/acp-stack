//! Dependency-apply run records.
//!
//! `deps_apply_runs` holds one row per dependency-apply invocation, keyed by
//! the same `apply_run_id` that groups the per-action `installer_runs` rows.
//! A partial unique index on `status = 'running'` makes "at most one live
//! apply" a transactional guarantee across processes — the daemon, the CLI,
//! a synchronous init step, and a detached init child all claim through the
//! same table, so none of them can interleave install snippets.
//!
//! Liveness is judged by the caller: every claim and reconcile takes an
//! `is_live(pid, boot_id)` predicate so the state layer stays free of
//! process-probing syscalls and tests can inject fake liveness.

use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use crate::error::{Result, StackError};

use super::core::StateStore;
use super::ids::current_timestamp;
use super::rows::validate_json_payload;

/// Terminal and in-flight status sentinels persisted to
/// `deps_apply_runs.status`.
pub const DEPS_APPLY_RUN_RUNNING: &str = "running";
pub const DEPS_APPLY_RUN_SUCCEEDED: &str = "succeeded";
pub const DEPS_APPLY_RUN_FAILED: &str = "failed";
pub const DEPS_APPLY_RUN_PRIVILEGE_BLOCKED: &str = "privilege_blocked";

/// Which surface started the apply. `init_background` marks a detached child
/// spawned by `acps init --deps-apply-async`.
pub const DEPS_APPLY_ORIGIN_INIT: &str = "init";
pub const DEPS_APPLY_ORIGIN_INIT_BACKGROUND: &str = "init_background";
pub const DEPS_APPLY_ORIGIN_CLI: &str = "cli";
pub const DEPS_APPLY_ORIGIN_API: &str = "api";

/// `error_code` stamped on a `running` row whose owning process is gone.
/// An abandoned row reads as `failed` and retryable; without this reconcile
/// the partial unique index would block every future apply.
pub const DEPS_APPLY_ABANDONED_ERROR_CODE: &str = "deps.apply_abandoned";

/// A `running` row with `pid IS NULL` older than this is abandoned. Every
/// in-process apply stamps its own pid inside the claim transaction, and the
/// detached path stamps the child pid immediately after spawn — so a null pid
/// can only mean a parent that died in the claim-to-spawn gap, which this
/// window comfortably covers.
pub const DEPS_APPLY_NULL_PID_GRACE: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepsApplyRunRecord {
    pub id: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub status: String,
    pub origin: String,
    pub init_run_id: Option<String>,
    pub feature: Option<String>,
    pub pid: Option<i64>,
    pub boot_id: Option<String>,
    pub total: i64,
    pub completed: i64,
    pub installed: i64,
    pub already_present: i64,
    pub privilege_required: i64,
    pub failed: i64,
    pub current_dep: Option<String>,
    pub log_dir: Option<String>,
    pub error_code: Option<String>,
    pub error_detail: Option<String>,
    pub payload_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewDepsApplyRun<'a> {
    /// The apply_run_id (`dap_*`) — caller-generated via
    /// [`super::ids::next_deps_apply_run_id`] so the per-action
    /// `installer_runs.apply_run_id` values match this row's key.
    pub id: &'a str,
    pub origin: &'a str,
    pub init_run_id: Option<&'a str>,
    pub feature: Option<&'a str>,
    /// Owning pid. In-process applies stamp their own pid at claim time; the
    /// detached-spawn path claims with `None` and stamps the child pid via
    /// [`StateStore::stamp_deps_apply_child`] right after the spawn.
    pub pid: Option<i64>,
    pub boot_id: Option<&'a str>,
    pub total: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepsApplyRunFinish<'a> {
    pub status: &'a str,
    pub completed: i64,
    pub installed: i64,
    pub already_present: i64,
    pub privilege_required: i64,
    pub failed: i64,
    pub error_code: Option<&'a str>,
    pub error_detail: Option<&'a str>,
    pub payload_json: &'a str,
}

const RUN_COLUMNS: &str = "id, started_at, finished_at, status, origin, init_run_id, feature, \
     pid, boot_id, total, completed, installed, already_present, privilege_required, failed, \
     current_dep, log_dir, error_code, error_detail, payload_json";

fn row_to_run(row: &rusqlite::Row<'_>) -> rusqlite::Result<DepsApplyRunRecord> {
    Ok(DepsApplyRunRecord {
        id: row.get(0)?,
        started_at: row.get(1)?,
        finished_at: row.get(2)?,
        status: row.get(3)?,
        origin: row.get(4)?,
        init_run_id: row.get(5)?,
        feature: row.get(6)?,
        pid: row.get(7)?,
        boot_id: row.get(8)?,
        total: row.get(9)?,
        completed: row.get(10)?,
        installed: row.get(11)?,
        already_present: row.get(12)?,
        privilege_required: row.get(13)?,
        failed: row.get(14)?,
        current_dep: row.get(15)?,
        log_dir: row.get(16)?,
        error_code: row.get(17)?,
        error_detail: row.get(18)?,
        payload_json: row.get(19)?,
    })
}

/// Whether a `running` row no longer has a live owner. `is_live` judges a
/// stamped pid (with its boot id); a null pid is abandoned once older than
/// [`DEPS_APPLY_NULL_PID_GRACE`].
fn row_is_abandoned(
    started_at: &str,
    pid: Option<i64>,
    boot_id: Option<&str>,
    is_live: &dyn Fn(i64, Option<&str>) -> bool,
) -> bool {
    match pid {
        Some(pid) => !is_live(pid, boot_id),
        None => match chrono::DateTime::parse_from_rfc3339(started_at) {
            Ok(started) => {
                let age = chrono::Utc::now().signed_duration_since(started);
                age.num_seconds() >= DEPS_APPLY_NULL_PID_GRACE.as_secs() as i64
            }
            // An unparseable timestamp cannot age out of the grace window;
            // treat it as abandoned so it cannot wedge the claim forever.
            Err(_) => true,
        },
    }
}

/// Mark every abandoned `running` row `failed` with
/// [`DEPS_APPLY_ABANDONED_ERROR_CODE`]. Shared by the claim transaction and
/// the standalone reconcile so both apply one rule.
fn reconcile_in_connection(
    connection: &Connection,
    is_live: &dyn Fn(i64, Option<&str>) -> bool,
) -> Result<usize> {
    let mut statement = connection.prepare(
        r#"
        SELECT id, started_at, pid, boot_id
        FROM deps_apply_runs
        WHERE status = ?1
        "#,
    )?;
    let rows = statement.query_map(params![DEPS_APPLY_RUN_RUNNING], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<i64>>(2)?,
            row.get::<_, Option<String>>(3)?,
        ))
    })?;
    let mut abandoned = Vec::new();
    for row in rows {
        let (id, started_at, pid, boot_id) = row?;
        if row_is_abandoned(&started_at, pid, boot_id.as_deref(), is_live) {
            abandoned.push((id, pid));
        }
    }
    drop(statement);
    let finished_at = current_timestamp();
    for (id, pid) in &abandoned {
        let detail = match pid {
            Some(pid) => format!("apply process (pid={pid}) is no longer running"),
            None => "apply claimed but its owner never stamped a pid".to_owned(),
        };
        connection.execute(
            r#"
            UPDATE deps_apply_runs
            SET status = ?1, finished_at = ?2, error_code = ?3, error_detail = ?4
            WHERE id = ?5 AND status = ?6
            "#,
            params![
                DEPS_APPLY_RUN_FAILED,
                finished_at,
                DEPS_APPLY_ABANDONED_ERROR_CODE,
                detail,
                id,
                DEPS_APPLY_RUN_RUNNING,
            ],
        )?;
    }
    Ok(abandoned.len())
}

fn is_unique_violation(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(failure, _)
            if failure.code == rusqlite::ErrorCode::ConstraintViolation
    )
}

impl StateStore {
    /// Claim the single live apply slot: reconcile abandoned rows, then insert
    /// a `running` row — all inside one `BEGIN IMMEDIATE` transaction so two
    /// processes cannot both pass the reconcile and race the insert. A
    /// surviving live row surfaces as [`StackError::DepsApplyInFlight`]
    /// carrying its apply_run_id.
    pub fn claim_deps_apply_run(
        &self,
        input: NewDepsApplyRun<'_>,
        is_live: &dyn Fn(i64, Option<&str>) -> bool,
    ) -> Result<DepsApplyRunRecord> {
        let transaction =
            Transaction::new_unchecked(self.connection(), TransactionBehavior::Immediate)?;
        reconcile_in_connection(&transaction, is_live)?;
        let record = DepsApplyRunRecord {
            id: input.id.to_owned(),
            started_at: current_timestamp(),
            finished_at: None,
            status: DEPS_APPLY_RUN_RUNNING.to_owned(),
            origin: input.origin.to_owned(),
            init_run_id: input.init_run_id.map(str::to_owned),
            feature: input.feature.map(str::to_owned),
            pid: input.pid,
            boot_id: input.boot_id.map(str::to_owned),
            total: input.total,
            completed: 0,
            installed: 0,
            already_present: 0,
            privilege_required: 0,
            failed: 0,
            current_dep: None,
            log_dir: None,
            error_code: None,
            error_detail: None,
            payload_json: "{}".to_owned(),
        };
        let inserted = transaction.execute(
            r#"
            INSERT INTO deps_apply_runs
                (id, started_at, finished_at, status, origin, init_run_id, feature,
                 pid, boot_id, total, completed, installed, already_present,
                 privilege_required, failed, current_dep, log_dir, error_code,
                 error_detail, payload_json)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                    ?16, ?17, ?18, ?19, ?20)
            "#,
            params![
                record.id,
                record.started_at,
                record.finished_at,
                record.status,
                record.origin,
                record.init_run_id,
                record.feature,
                record.pid,
                record.boot_id,
                record.total,
                record.completed,
                record.installed,
                record.already_present,
                record.privilege_required,
                record.failed,
                record.current_dep,
                record.log_dir,
                record.error_code,
                record.error_detail,
                record.payload_json,
            ],
        );
        match inserted {
            Ok(_) => {}
            Err(error) if is_unique_violation(&error) => {
                let live: Option<String> = transaction
                    .query_row(
                        "SELECT id FROM deps_apply_runs WHERE status = ?1",
                        params![DEPS_APPLY_RUN_RUNNING],
                        |row| row.get(0),
                    )
                    .optional()?;
                // Commit so the reconcile above still lands even though this
                // claim lost the slot.
                transaction.commit()?;
                return Err(StackError::DepsApplyInFlight {
                    apply_run_id: live.unwrap_or_default(),
                });
            }
            Err(error) => return Err(error.into()),
        }
        transaction.commit()?;
        Ok(record)
    }

    /// Standalone reconcile for read paths (status probes, per-run reads) so
    /// an abandoned apply surfaces as failed-retryable without waiting for
    /// the next claim. Returns the number of rows reconciled. The common case
    /// (no running row) stays a plain read: `/v1/status` polls this, and
    /// taking SQLite's write lock per poll would serialize every status hit
    /// against the audit middleware and any in-flight apply writes.
    pub fn reconcile_stale_deps_apply_runs(
        &self,
        is_live: &dyn Fn(i64, Option<&str>) -> bool,
    ) -> Result<usize> {
        let running: i64 = self.connection().query_row(
            "SELECT COUNT(*) FROM deps_apply_runs WHERE status = ?1",
            params![DEPS_APPLY_RUN_RUNNING],
            |row| row.get(0),
        )?;
        if running == 0 {
            return Ok(0);
        }
        let transaction =
            Transaction::new_unchecked(self.connection(), TransactionBehavior::Immediate)?;
        let reconciled = reconcile_in_connection(&transaction, is_live)?;
        transaction.commit()?;
        Ok(reconciled)
    }

    /// Stamp the detached child's identity onto a claimed row. Until this
    /// lands the row's null pid is covered by [`DEPS_APPLY_NULL_PID_GRACE`].
    pub fn stamp_deps_apply_child(
        &self,
        apply_run_id: &str,
        pid: i64,
        boot_id: Option<&str>,
        log_dir: Option<&str>,
    ) -> Result<()> {
        self.connection().execute(
            r#"
            UPDATE deps_apply_runs
            SET pid = ?1, boot_id = ?2, log_dir = ?3
            WHERE id = ?4
            "#,
            params![pid, boot_id, log_dir, apply_run_id],
        )?;
        Ok(())
    }

    /// Advance the progress counters while the apply iterates its actions.
    pub fn update_deps_apply_progress(
        &self,
        apply_run_id: &str,
        completed: i64,
        current_dep: Option<&str>,
    ) -> Result<()> {
        self.connection().execute(
            r#"
            UPDATE deps_apply_runs
            SET completed = ?1, current_dep = ?2
            WHERE id = ?3
            "#,
            params![completed, current_dep, apply_run_id],
        )?;
        Ok(())
    }

    /// Settle a run's terminal status, outcome counts, and timestamps.
    pub fn finish_deps_apply_run(
        &self,
        apply_run_id: &str,
        finish: DepsApplyRunFinish<'_>,
    ) -> Result<()> {
        validate_json_payload(self.connection(), finish.payload_json)?;
        let finished_at = current_timestamp();
        self.connection().execute(
            r#"
            UPDATE deps_apply_runs
            SET status = ?1, finished_at = ?2, completed = ?3, installed = ?4,
                already_present = ?5, privilege_required = ?6, failed = ?7,
                current_dep = NULL, error_code = ?8, error_detail = ?9,
                payload_json = ?10
            WHERE id = ?11
            "#,
            params![
                finish.status,
                finished_at,
                finish.completed,
                finish.installed,
                finish.already_present,
                finish.privilege_required,
                finish.failed,
                finish.error_code,
                finish.error_detail,
                finish.payload_json,
                apply_run_id,
            ],
        )?;
        Ok(())
    }

    /// Force-settle every `running` row this process itself owns (matching pid
    /// AND boot id) to `failed`. The API apply path calls this while holding the
    /// in-process `deps_apply_lock`, which proves no in-process apply is live —
    /// so a surviving self-owned `running` row can only be a prior apply whose
    /// terminal write failed. The daemon's pid stays live for its whole
    /// lifetime, so the liveness reconcile never frees such a row; without this
    /// it would block every future apply until the daemon restarts. Returns the
    /// number of rows settled.
    pub fn fail_self_owned_stale_deps_apply_runs(
        &self,
        pid: i64,
        boot_id: Option<&str>,
    ) -> Result<usize> {
        let finished_at = current_timestamp();
        let affected = self.connection().execute(
            r#"
            UPDATE deps_apply_runs
            SET status = ?1, finished_at = ?2, current_dep = NULL,
                error_code = ?3, error_detail = ?4
            WHERE status = ?5 AND pid = ?6 AND boot_id IS ?7
            "#,
            params![
                DEPS_APPLY_RUN_FAILED,
                finished_at,
                DEPS_APPLY_ABANDONED_ERROR_CODE,
                "apply row left running by a prior in-process apply whose terminal write failed",
                DEPS_APPLY_RUN_RUNNING,
                pid,
                boot_id,
            ],
        )?;
        Ok(affected)
    }

    pub fn lookup_deps_apply_run(&self, apply_run_id: &str) -> Result<Option<DepsApplyRunRecord>> {
        Ok(self
            .connection()
            .query_row(
                &format!("SELECT {RUN_COLUMNS} FROM deps_apply_runs WHERE id = ?1"),
                params![apply_run_id],
                row_to_run,
            )
            .optional()?)
    }

    pub fn latest_deps_apply_run(&self) -> Result<Option<DepsApplyRunRecord>> {
        Ok(self
            .connection()
            .query_row(
                &format!(
                    "SELECT {RUN_COLUMNS} FROM deps_apply_runs \
                     ORDER BY started_at DESC, id DESC LIMIT 1"
                ),
                [],
                row_to_run,
            )
            .optional()?)
    }

    /// The live `running` row, if any. Does not reconcile — callers that need
    /// an honest answer pair this with [`Self::reconcile_stale_deps_apply_runs`].
    pub fn running_deps_apply_run(&self) -> Result<Option<DepsApplyRunRecord>> {
        Ok(self
            .connection()
            .query_row(
                &format!("SELECT {RUN_COLUMNS} FROM deps_apply_runs WHERE status = ?1"),
                params![DEPS_APPLY_RUN_RUNNING],
                row_to_run,
            )
            .optional()?)
    }

    pub fn query_deps_apply_runs(&self, limit: u32) -> Result<Vec<DepsApplyRunRecord>> {
        let limit = i64::from(limit);
        let mut statement = self.connection().prepare(&format!(
            "SELECT {RUN_COLUMNS} FROM deps_apply_runs \
             ORDER BY started_at DESC, id DESC LIMIT ?1"
        ))?;
        let rows = statement.query_map(params![limit], row_to_run)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

#[cfg(test)]
mod tests;
