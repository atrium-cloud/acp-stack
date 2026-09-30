//! Session change feed: every write that touches a session stamps it with the
//! next value of one process-wide counter, so a reader asks which sessions
//! changed after the last position it saw.
//!
//! The counter is a single row rather than `MAX(change_seq) + 1`: the maximum
//! over live rows goes backwards when the session holding it is deleted, and
//! a reissued value would hide the next change from a reader already past it.
//! The increment is one statement under the write lock, so it is serialized
//! across every process that opens the database.

use super::*;

/// One entry of the change feed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionChangeRecord {
    pub session_id: String,
    pub change_seq: u64,
    pub deleted: bool,
}

/// A page of the change feed with the positions a reader checks its cursor
/// against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionChangesPage {
    pub changes: Vec<SessionChangeRecord>,
    /// The last `change_seq` issued.
    pub head: u64,
    /// The highest `change_seq` of a pruned tombstone; a cursor below it may
    /// have missed a deletion.
    pub pruned_through: u64,
    /// Names this opening of the database; a reader that sees another epoch
    /// re-reads the feed from the start.
    pub feed_epoch: String,
}

/// Issue the next `change_seq`. Runs inside the caller's write transaction, so
/// a write that rolls back gives its value back.
pub(super) fn next_change_seq(conn: &rusqlite::Connection) -> Result<u64> {
    Ok(conn.query_row(
        "UPDATE session_change_counter SET change_seq = change_seq + 1 RETURNING change_seq",
        [],
        |row| sequence_column(row, 0),
    )?)
}

/// Stamp `session_id` with the next `change_seq`. A write scoped to a session
/// with no row (an event logged for a session that was never stored) issues
/// no value.
pub(in crate::state) fn bump_session_change(
    conn: &rusqlite::Connection,
    session_id: &str,
) -> Result<()> {
    let change_seq = conn
        .query_row(
            r#"
            UPDATE session_change_counter
            SET change_seq = change_seq + 1
            WHERE EXISTS (SELECT 1 FROM sessions WHERE id = ?1)
            RETURNING change_seq
            "#,
            params![session_id],
            |row| sequence_column(row, 0),
        )
        .optional()?;
    if let Some(change_seq) = change_seq {
        conn.execute(
            "UPDATE sessions SET change_seq = ?1 WHERE id = ?2",
            params![sequence_param(change_seq)?, session_id],
        )?;
    }
    Ok(())
}

impl StateStore {
    /// Sessions whose latest change is after `after`, ascending by
    /// `change_seq`, with the counter's head read in the same snapshot.
    pub fn query_session_changes(&self, after: u64, limit: u32) -> Result<SessionChangesPage> {
        // A position beyond SQLite's integer range is past every stored value.
        let after = i64::try_from(after).unwrap_or(i64::MAX);
        let transaction = rusqlite::Transaction::new_unchecked(
            self.connection(),
            rusqlite::TransactionBehavior::Deferred,
        )?;
        let (head, pruned_through) = transaction.query_row(
            "SELECT change_seq, pruned_through FROM session_change_counter WHERE id = 1",
            [],
            |row| Ok((sequence_column(row, 0)?, sequence_column(row, 1)?)),
        )?;
        let changes = {
            let mut statement = transaction.prepare(
                r#"
                SELECT id, change_seq, 0 AS deleted
                FROM sessions
                WHERE change_seq > ?1
                UNION ALL
                SELECT session_id, change_seq, 1 AS deleted
                FROM session_tombstones
                WHERE change_seq > ?1
                ORDER BY change_seq ASC
                LIMIT ?2
                "#,
            )?;
            let rows = statement.query_map(params![after, i64::from(limit)], |row| {
                Ok(SessionChangeRecord {
                    session_id: row.get(0)?,
                    change_seq: sequence_column(row, 1)?,
                    deleted: row.get::<_, i64>(2)? != 0,
                })
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        transaction.commit()?;
        Ok(SessionChangesPage {
            changes,
            head,
            pruned_through,
            feed_epoch: self.feed_epoch().to_owned(),
        })
    }

    /// The live session row, or the error a per-session route answers with:
    /// `SessionDeleted` for a tombstoned id, `SessionNotFound` otherwise.
    pub fn require_live_session(&self, id: &str) -> Result<SessionRecord> {
        if let Some(record) = self.get_session(id)? {
            return Ok(record);
        }
        let tombstoned = self
            .connection()
            .query_row(
                "SELECT 1 FROM session_tombstones WHERE session_id = ?1",
                params![id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if tombstoned {
            return Err(StackError::SessionDeleted { id: id.to_owned() });
        }
        Err(StackError::SessionNotFound { id: id.to_owned() })
    }

    /// Drop tombstones older than `retention`, returning how many went, and
    /// raise `pruned_through` to the highest `change_seq` dropped.
    pub fn prune_session_tombstones(&self, retention: std::time::Duration) -> Result<usize> {
        let retention =
            chrono::Duration::from_std(retention).map_err(|err| StackError::InvalidParam {
                field: "sessions.tombstone_retention",
                reason: format!("retention out of range: {err}"),
            })?;
        let cutoff = Utc::now()
            .checked_sub_signed(retention)
            .ok_or(StackError::InvalidParam {
                field: "sessions.tombstone_retention",
                reason: "retention subtraction underflowed the chrono range".to_owned(),
            })?;
        // Formatted like `current_timestamp`, which the string comparison needs.
        let cutoff = cutoff.to_rfc3339_opts(SecondsFormat::Nanos, true);
        let transaction = rusqlite::Transaction::new_unchecked(
            self.connection(),
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let pruned: Vec<u64> = {
            let mut statement = transaction.prepare(
                "DELETE FROM session_tombstones WHERE deleted_at < ?1 RETURNING change_seq",
            )?;
            let rows = statement.query_map(params![cutoff], |row| sequence_column(row, 0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        if let Some(highest) = pruned.iter().max() {
            transaction.execute(
                r#"
                UPDATE session_change_counter
                SET pruned_through = MAX(pruned_through, ?1)
                WHERE id = 1
                "#,
                params![sequence_param(*highest)?],
            )?;
        }
        transaction.commit()?;
        Ok(pruned.len())
    }
}

/// Record the deletion of `session_id` at the next `change_seq`, inside the
/// transaction that removes its rows.
pub(super) fn insert_session_tombstone(
    conn: &rusqlite::Connection,
    session_id: &str,
    deleted_at: &str,
) -> Result<()> {
    let change_seq = next_change_seq(conn)?;
    conn.execute(
        r#"
        INSERT INTO session_tombstones (session_id, deleted_at, change_seq)
        VALUES (?1, ?2, ?3)
        ON CONFLICT(session_id) DO UPDATE SET
            deleted_at = excluded.deleted_at,
            change_seq = excluded.change_seq
        "#,
        params![session_id, deleted_at, sequence_param(change_seq)?],
    )?;
    Ok(())
}

/// A session id is live or tombstoned, never both: inserting a row under an
/// id clears its tombstone.
pub(super) fn clear_session_tombstone(conn: &rusqlite::Connection, session_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM session_tombstones WHERE session_id = ?1",
        params![session_id],
    )?;
    Ok(())
}
