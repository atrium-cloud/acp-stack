//! acp-stack self-update attempt history.

use crate::error::Result;
use rusqlite::params;

use super::core::StateStore;
use super::ids::{current_timestamp, next_stack_update_run_id};
use super::rows::validate_json_payload;

pub const STACK_UPDATE_OPERATION_CHECK: &str = "check";
pub const STACK_UPDATE_OPERATION_INSTALL: &str = "install";
pub const STACK_UPDATE_STATUS_SUCCEEDED: &str = "succeeded";
pub const STACK_UPDATE_STATUS_FAILED: &str = "failed";
pub const STACK_UPDATE_STATUS_SKIPPED: &str = "skipped";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackUpdateRun {
    pub id: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub operation: String,
    pub status: String,
    pub current_version: String,
    pub target_version: Option<String>,
    pub target_tag: Option<String>,
    pub classification: Option<String>,
    pub breaking: bool,
    pub major_upgrade: bool,
    pub policy: String,
    pub auto: bool,
    pub message: Option<String>,
    pub payload_json: String,
}

#[derive(Debug, Clone, Copy)]
pub struct NewStackUpdateRun<'a> {
    pub operation: &'a str,
    pub status: &'a str,
    pub current_version: &'a str,
    pub target_version: Option<&'a str>,
    pub target_tag: Option<&'a str>,
    pub classification: Option<&'a str>,
    pub breaking: bool,
    pub major_upgrade: bool,
    pub policy: &'a str,
    pub auto: bool,
    pub message: Option<&'a str>,
    pub payload_json: &'a str,
}

fn row_to_stack_update_run(row: &rusqlite::Row<'_>) -> rusqlite::Result<StackUpdateRun> {
    Ok(StackUpdateRun {
        id: row.get(0)?,
        started_at: row.get(1)?,
        finished_at: row.get(2)?,
        operation: row.get(3)?,
        status: row.get(4)?,
        current_version: row.get(5)?,
        target_version: row.get(6)?,
        target_tag: row.get(7)?,
        classification: row.get(8)?,
        breaking: row.get::<_, i64>(9)? != 0,
        major_upgrade: row.get::<_, i64>(10)? != 0,
        policy: row.get(11)?,
        auto: row.get::<_, i64>(12)? != 0,
        message: row.get(13)?,
        payload_json: row.get(14)?,
    })
}

impl StateStore {
    pub fn append_stack_update_run(&self, input: NewStackUpdateRun<'_>) -> Result<StackUpdateRun> {
        validate_json_payload(self.connection(), input.payload_json)?;
        let run = StackUpdateRun {
            id: next_stack_update_run_id(),
            started_at: current_timestamp(),
            finished_at: Some(current_timestamp()),
            operation: input.operation.to_owned(),
            status: input.status.to_owned(),
            current_version: input.current_version.to_owned(),
            target_version: input.target_version.map(str::to_owned),
            target_tag: input.target_tag.map(str::to_owned),
            classification: input.classification.map(str::to_owned),
            breaking: input.breaking,
            major_upgrade: input.major_upgrade,
            policy: input.policy.to_owned(),
            auto: input.auto,
            message: input.message.map(str::to_owned),
            payload_json: input.payload_json.to_owned(),
        };
        self.connection().execute(
            r#"
            INSERT INTO stack_update_runs
                (id, started_at, finished_at, operation, status, current_version,
                 target_version, target_tag, classification, breaking, major_upgrade,
                 policy, auto, message, payload_json)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
            "#,
            params![
                run.id,
                run.started_at,
                run.finished_at,
                run.operation,
                run.status,
                run.current_version,
                run.target_version,
                run.target_tag,
                run.classification,
                if run.breaking { 1 } else { 0 },
                if run.major_upgrade { 1 } else { 0 },
                run.policy,
                if run.auto { 1 } else { 0 },
                run.message,
                run.payload_json,
            ],
        )?;
        Ok(run)
    }

    /// Latest auto `install` run that contacted upstream. This is the
    /// reference point for the auto-update frequency window, so it must
    /// exclude frequency skips (which are recorded without any upstream
    /// contact and would re-arm the window on every timer fire) while still
    /// counting up-to-date/blocked/manual-only skips, which did resolve a
    /// release. Frequency skips are the only runs recorded as `skipped` with
    /// neither a resolved `target_version` nor a `target_tag`. The query is
    /// unbounded because a fixed-size recent-row scan would let accumulated
    /// skip rows push the reference out of view.
    pub fn latest_stack_auto_install_attempt(&self) -> Result<Option<StackUpdateRun>> {
        let mut statement = self.connection().prepare(
            r#"
            SELECT id, started_at, finished_at, operation, status, current_version,
                   target_version, target_tag, classification, breaking, major_upgrade,
                   policy, auto, message, payload_json
            FROM stack_update_runs
            WHERE operation = ?1 AND auto = 1
              AND NOT (status = ?2 AND target_version IS NULL AND target_tag IS NULL)
            ORDER BY started_at DESC, id DESC
            LIMIT 1
            "#,
        )?;
        let mut rows = statement.query_map(
            params![STACK_UPDATE_OPERATION_INSTALL, STACK_UPDATE_STATUS_SKIPPED],
            row_to_stack_update_run,
        )?;
        rows.next().transpose().map_err(Into::into)
    }

    pub fn query_stack_update_runs(&self, limit: u32) -> Result<Vec<StackUpdateRun>> {
        let mut statement = self.connection().prepare(
            r#"
            SELECT id, started_at, finished_at, operation, status, current_version,
                   target_version, target_tag, classification, breaking, major_upgrade,
                   policy, auto, message, payload_json
            FROM stack_update_runs
            ORDER BY started_at DESC, id DESC
            LIMIT ?1
            "#,
        )?;
        let rows = statement.query_map(params![i64::from(limit)], row_to_stack_update_run)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}
