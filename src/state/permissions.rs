//! `permission_requests` / `permission_decisions` persistence.

use crate::error::{Result, StackError};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};

use super::core::StateStore;
use super::ids::{current_timestamp, next_permission_decision_id, next_permission_request_id};
use super::rows::validate_json_payload;

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

pub(super) fn row_to_permission_request(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<PermissionRequestRecord> {
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

impl StateStore {
    /// Insert a `permission_requests` row in the `pending` state. Returns the
    /// fully-populated record so callers can publish it without re-reading.
    pub fn append_permission_request(
        &self,
        input: NewPermissionRequest<'_>,
    ) -> Result<PermissionRequestRecord> {
        validate_json_payload(self.connection(), input.detail_json)?;
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
        self.connection().execute(
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
            .connection()
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

        let affected = self.connection().execute(
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
            Transaction::new_unchecked(self.connection(), TransactionBehavior::Immediate)?;
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
        self.connection().execute(
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
            .connection()
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
        let mut statement = self.connection().prepare(
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
            Transaction::new_unchecked(self.connection(), TransactionBehavior::Immediate)?;
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
}
