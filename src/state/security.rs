//! Security self-check run history.
//!
//! Two tables back the operator-facing history view:
//!
//! - `security_runs` — one row per `GET /v1/security/check` invocation,
//!   recording the aggregate verdict (`succeeded` when no critical findings
//!   were emitted, `failed` otherwise), counts, and a redacted snapshot of
//!   the inputs that drove the check.
//! - `security_findings` — one row per emitted finding, keyed by
//!   `(run_id, ordinal)` so the show view replays a run in the order the
//!   orchestrator produced it.
//!
//! Runs are kept indefinitely; trimming is left to future operations work.

use crate::error::Result;
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};

use super::core::StateStore;
use super::ids::{current_timestamp, next_security_run_id};
use super::rows::validate_json_payload;

pub const SECURITY_RUN_SUCCEEDED: &str = "succeeded";
pub const SECURITY_RUN_FAILED: &str = "failed";

pub const SECURITY_FINDING_SEVERITY_WARNING: &str = "warning";
pub const SECURITY_FINDING_SEVERITY_CRITICAL: &str = "critical";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecurityRunRecord {
    pub id: String,
    pub started_at: String,
    pub finished_at: String,
    pub status: String,
    pub ok: bool,
    pub critical_count: i64,
    pub warning_count: i64,
    pub auth_failure_count: i64,
    pub inputs_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecurityFindingRow {
    pub run_id: String,
    pub ordinal: i64,
    pub code: String,
    pub severity: String,
    pub message: String,
    pub details_json: Option<String>,
    pub remediation: Option<String>,
}

/// Input shape for [`StateStore::record_security_run`]. The store generates
/// the run id and stamps timestamps; the caller supplies the aggregate
/// outcome plus the ordered list of findings.
#[derive(Debug, Clone)]
pub struct NewSecurityRun<'a> {
    pub started_at: &'a str,
    pub auth_failure_count: i64,
    pub inputs_json: &'a str,
    pub findings: &'a [NewSecurityFinding<'a>],
}

#[derive(Debug, Clone)]
pub struct NewSecurityFinding<'a> {
    pub code: &'a str,
    pub severity: &'a str,
    pub message: &'a str,
    pub details_json: Option<&'a str>,
    pub remediation: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SecurityRunFilter<'a> {
    pub limit: u32,
    pub after_id: Option<&'a str>,
    pub since: Option<&'a str>,
    pub until: Option<&'a str>,
}

fn row_to_security_run(row: &rusqlite::Row<'_>) -> rusqlite::Result<SecurityRunRecord> {
    let ok_int: i64 = row.get(4)?;
    Ok(SecurityRunRecord {
        id: row.get(0)?,
        started_at: row.get(1)?,
        finished_at: row.get(2)?,
        status: row.get(3)?,
        ok: ok_int != 0,
        critical_count: row.get(5)?,
        warning_count: row.get(6)?,
        auth_failure_count: row.get(7)?,
        inputs_json: row.get(8)?,
    })
}

fn row_to_security_finding(row: &rusqlite::Row<'_>) -> rusqlite::Result<SecurityFindingRow> {
    Ok(SecurityFindingRow {
        run_id: row.get(0)?,
        ordinal: row.get(1)?,
        code: row.get(2)?,
        severity: row.get(3)?,
        message: row.get(4)?,
        details_json: row.get(5)?,
        remediation: row.get(6)?,
    })
}

impl StateStore {
    /// Persist a completed self-check run plus its findings inside a single
    /// transaction. Returns the generated run id so the route layer can echo
    /// it back to the caller.
    pub fn record_security_run(&self, input: NewSecurityRun<'_>) -> Result<SecurityRunRecord> {
        validate_json_payload(self.connection(), input.inputs_json)?;

        let mut critical_count = 0i64;
        let mut warning_count = 0i64;
        for finding in input.findings {
            match finding.severity {
                SECURITY_FINDING_SEVERITY_CRITICAL => critical_count += 1,
                SECURITY_FINDING_SEVERITY_WARNING => warning_count += 1,
                other => {
                    return Err(crate::error::StackError::SecurityFindingSeverityInvalid {
                        severity: other.to_owned(),
                    });
                }
            }
            if let Some(details) = finding.details_json {
                validate_json_payload(self.connection(), details)?;
            }
        }
        let ok = critical_count == 0 && warning_count == 0;
        let status = if critical_count == 0 {
            SECURITY_RUN_SUCCEEDED
        } else {
            SECURITY_RUN_FAILED
        };

        let record = SecurityRunRecord {
            id: next_security_run_id(),
            started_at: input.started_at.to_owned(),
            finished_at: current_timestamp(),
            status: status.to_owned(),
            ok,
            critical_count,
            warning_count,
            auth_failure_count: input.auth_failure_count,
            inputs_json: input.inputs_json.to_owned(),
        };

        // `Transaction::new_unchecked` paired with `commit()` lets rusqlite's
        // `Drop` impl run a real rollback if we bail out early, so we never
        // leave the shared connection sitting on an open transaction. The
        // older "manual BEGIN IMMEDIATE / COMMIT / ROLLBACK" sequence cannot
        // recover from a COMMIT failure cleanly.
        let connection = self.connection();
        let transaction = Transaction::new_unchecked(connection, TransactionBehavior::Immediate)?;
        transaction.execute(
            r#"
            INSERT INTO security_runs
                (id, started_at, finished_at, status, ok, critical_count,
                 warning_count, auth_failure_count, inputs_json)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
            "#,
            params![
                record.id,
                record.started_at,
                record.finished_at,
                record.status,
                if record.ok { 1i64 } else { 0i64 },
                record.critical_count,
                record.warning_count,
                record.auth_failure_count,
                record.inputs_json,
            ],
        )?;
        for (ordinal, finding) in input.findings.iter().enumerate() {
            transaction.execute(
                r#"
                INSERT INTO security_findings
                    (run_id, ordinal, code, severity, message, details_json, remediation)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                "#,
                params![
                    record.id,
                    ordinal as i64,
                    finding.code,
                    finding.severity,
                    finding.message,
                    finding.details_json,
                    finding.remediation,
                ],
            )?;
        }
        transaction.commit()?;
        Ok(record)
    }

    pub fn query_security_runs(
        &self,
        filter: SecurityRunFilter<'_>,
    ) -> Result<Vec<SecurityRunRecord>> {
        let mut sql = String::from(
            "SELECT id, started_at, finished_at, status, ok, critical_count, \
             warning_count, auth_failure_count, inputs_json \
             FROM security_runs WHERE 1=1",
        );
        let mut bindings: Vec<rusqlite::types::Value> = Vec::new();
        if let Some(since) = filter.since {
            sql.push_str(" AND started_at >= ?");
            bindings.push(rusqlite::types::Value::Text(since.to_owned()));
        }
        if let Some(until) = filter.until {
            sql.push_str(" AND started_at < ?");
            bindings.push(rusqlite::types::Value::Text(until.to_owned()));
        }
        if let Some(after) = filter.after_id {
            sql.push_str(
                " AND (started_at, id) < (SELECT started_at, id FROM security_runs WHERE id = ?)",
            );
            bindings.push(rusqlite::types::Value::Text(after.to_owned()));
        }
        sql.push_str(" ORDER BY started_at DESC, id DESC LIMIT ?");
        bindings.push(rusqlite::types::Value::Integer(i64::from(filter.limit)));

        let mut statement = self.connection().prepare(&sql)?;
        let rows = statement.query_map(
            rusqlite::params_from_iter(bindings.iter()),
            row_to_security_run,
        )?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn get_security_run(&self, id: &str) -> Result<Option<SecurityRunRecord>> {
        Ok(self
            .connection()
            .query_row(
                r#"
                SELECT id, started_at, finished_at, status, ok, critical_count,
                       warning_count, auth_failure_count, inputs_json
                FROM security_runs
                WHERE id = ?1
                "#,
                params![id],
                row_to_security_run,
            )
            .optional()?)
    }

    pub fn get_findings_for_run(&self, run_id: &str) -> Result<Vec<SecurityFindingRow>> {
        let mut statement = self.connection().prepare(
            r#"
            SELECT run_id, ordinal, code, severity, message, details_json, remediation
            FROM security_findings
            WHERE run_id = ?1
            ORDER BY ordinal ASC
            "#,
        )?;
        let rows = statement.query_map(params![run_id], row_to_security_finding)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::core::StateStore;
    use tempfile::TempDir;

    fn open_store() -> (TempDir, StateStore) {
        let dir = TempDir::new().expect("tempdir");
        let store = StateStore::open(dir.path().join("state.sqlite")).expect("open store");
        store.migrate().expect("run migrations");
        (dir, store)
    }

    fn finding<'a>(code: &'a str, severity: &'a str, message: &'a str) -> NewSecurityFinding<'a> {
        NewSecurityFinding {
            code,
            severity,
            message,
            details_json: None,
            remediation: None,
        }
    }

    #[test]
    fn record_run_with_no_findings_marks_ok_and_succeeded() {
        let (_dir, store) = open_store();
        let run = store
            .record_security_run(NewSecurityRun {
                started_at: "2026-05-28T00:00:00Z",
                auth_failure_count: 0,
                inputs_json: "{}",
                findings: &[],
            })
            .expect("record run");
        assert!(run.ok);
        assert_eq!(run.status, SECURITY_RUN_SUCCEEDED);
        assert_eq!(run.critical_count, 0);
        assert_eq!(run.warning_count, 0);

        let fetched = store
            .get_security_run(&run.id)
            .expect("get")
            .expect("present");
        assert_eq!(fetched, run);
        let findings = store.get_findings_for_run(&run.id).expect("findings");
        assert!(findings.is_empty());
    }

    #[test]
    fn record_run_with_invalid_severity_returns_typed_error_and_persists_nothing() {
        let (_dir, store) = open_store();
        let findings = vec![finding("auth.failure_threshold", "info", "bogus severity")];
        let err = store
            .record_security_run(NewSecurityRun {
                started_at: "2026-05-28T00:00:00Z",
                auth_failure_count: 0,
                inputs_json: "{}",
                findings: &findings,
            })
            .expect_err("invalid severity must be rejected");
        assert!(matches!(
            err,
            crate::error::StackError::SecurityFindingSeverityInvalid { ref severity }
                if severity == "info"
        ));
        let runs = store
            .query_security_runs(SecurityRunFilter {
                limit: 10,
                after_id: None,
                since: None,
                until: None,
            })
            .expect("list runs");
        assert!(runs.is_empty(), "rejected run must not be persisted");
    }

    #[test]
    fn record_run_with_warning_only_is_succeeded_but_not_ok() {
        let (_dir, store) = open_store();
        let findings = vec![finding(
            "auth.failure_threshold",
            "warning",
            "auth failures",
        )];
        let run = store
            .record_security_run(NewSecurityRun {
                started_at: "2026-05-28T00:00:01Z",
                auth_failure_count: 3,
                inputs_json: "{}",
                findings: &findings,
            })
            .expect("record run");
        assert!(!run.ok);
        assert_eq!(run.status, SECURITY_RUN_SUCCEEDED);
        assert_eq!(run.warning_count, 1);
        assert_eq!(run.critical_count, 0);
        assert_eq!(run.auth_failure_count, 3);

        let rows = store.get_findings_for_run(&run.id).expect("findings");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].code, "auth.failure_threshold");
        assert_eq!(rows[0].ordinal, 0);
    }

    #[test]
    fn record_run_with_critical_marks_failed() {
        let (_dir, store) = open_store();
        let findings = vec![
            finding("runtime.path_ownership", "critical", "ownership"),
            finding("auth.failure_threshold", "warning", "auth failures"),
        ];
        let run = store
            .record_security_run(NewSecurityRun {
                started_at: "2026-05-28T00:00:02Z",
                auth_failure_count: 0,
                inputs_json: "{}",
                findings: &findings,
            })
            .expect("record run");
        assert!(!run.ok);
        assert_eq!(run.status, SECURITY_RUN_FAILED);
        assert_eq!(run.critical_count, 1);
        assert_eq!(run.warning_count, 1);

        let rows = store.get_findings_for_run(&run.id).expect("findings");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].ordinal, 0);
        assert_eq!(rows[0].code, "runtime.path_ownership");
        assert_eq!(rows[1].ordinal, 1);
        assert_eq!(rows[1].code, "auth.failure_threshold");
    }

    #[test]
    fn query_runs_keyset_paginates_in_started_at_desc_order() {
        let (_dir, store) = open_store();
        // Different started_at values so the keyset cursor is unambiguous.
        let mut ids = Vec::new();
        for i in 0..3 {
            let run = store
                .record_security_run(NewSecurityRun {
                    started_at: &format!("2026-05-28T00:00:1{i}Z"),
                    auth_failure_count: 0,
                    inputs_json: "{}",
                    findings: &[],
                })
                .expect("record");
            ids.push(run.id);
        }

        let first = store
            .query_security_runs(SecurityRunFilter {
                limit: 2,
                after_id: None,
                since: None,
                until: None,
            })
            .expect("first page");
        assert_eq!(first.len(), 2);
        // Inserted in ascending order; expect descending starts.
        assert_eq!(first[0].id, ids[2]);
        assert_eq!(first[1].id, ids[1]);

        let cursor = first.last().expect("cursor").id.clone();
        let second = store
            .query_security_runs(SecurityRunFilter {
                limit: 2,
                after_id: Some(cursor.as_str()),
                since: None,
                until: None,
            })
            .expect("second page");
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].id, ids[0]);
    }

    #[test]
    fn get_security_run_returns_none_for_unknown_id() {
        let (_dir, store) = open_store();
        let missing = store
            .get_security_run("srun_does_not_exist")
            .expect("query");
        assert!(missing.is_none());
    }

    #[test]
    fn record_run_validates_details_json() {
        let (_dir, store) = open_store();
        let bad = NewSecurityFinding {
            code: "runtime.path_ownership",
            severity: "critical",
            message: "oops",
            details_json: Some("{not json"),
            remediation: None,
        };
        let err = store
            .record_security_run(NewSecurityRun {
                started_at: "2026-05-28T00:00:00Z",
                auth_failure_count: 0,
                inputs_json: "{}",
                findings: std::slice::from_ref(&bad),
            })
            .expect_err("invalid json should fail");
        let message = format!("{err}");
        assert!(
            message.to_ascii_lowercase().contains("json"),
            "expected json validation error, got: {message}"
        );
        // Confirm rollback: no rows persisted.
        let runs = store
            .query_security_runs(SecurityRunFilter {
                limit: 10,
                ..Default::default()
            })
            .expect("query");
        assert!(runs.is_empty());
    }
}
