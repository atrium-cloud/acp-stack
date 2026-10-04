//! Fork inheritance: the part of a parent session's durable record that a fork
//! child carries, written together with the child row.

use rusqlite::{Transaction, TransactionBehavior};

use super::events::next_session_event_seq;
use super::queries::{insert_session_row, new_active_session_row};
use super::*;

/// Session-scoped event kinds that make up a conversation. A fork child
/// inherits these rows; every other session-scoped kind records the parent's
/// own lifecycle or derived accounting and stays with the parent, so a
/// lifecycle transition or a usage report lifted from an inherited
/// `session.update` is never counted on two sessions.
const FORK_INHERITED_EVENT_KINDS: &[&str] = &[
    EVENT_KIND_SESSION_UPDATE,
    EVENT_KIND_PROMPT_INFERENCE_FAILED,
    EVENT_KIND_PROMPT_STALLED,
    EVENT_KIND_PROMPT_STALL_RESOLVED,
    EVENT_KIND_PROMPT_ERRORED,
    // Held turns keep their token usage; no inherited row rebuilds it.
    EVENT_KIND_PROMPT_USAGE_REPORTED,
    EVENT_KIND_SESSION_CANCEL_REQUESTED,
    EVENT_KIND_TERMINAL_FINISHED,
    // The session-scoped decisions on a turn's ACP permission requests.
    EVENT_KIND_PERMISSION_APPROVED,
    EVENT_KIND_PERMISSION_DENIED,
    EVENT_KIND_PERMISSION_CANCELLED,
    EVENT_KIND_PERMISSION_EXPIRED,
];

impl StateStore {
    /// The last prompt a head fork of `session_id` holds: the newest prompt
    /// with no turn in flight at or before it. A turn still running when the
    /// fork is dispatched stays with the parent, matching adapters that fork
    /// from the last settled turn.
    pub fn newest_settled_prompt_id(&self, session_id: &str) -> Result<Option<String>> {
        Ok(self
            .connection()
            .query_row(
                r#"
                SELECT id
                FROM prompts
                WHERE session_id = ?1
                  AND NOT EXISTS (
                      SELECT 1 FROM prompts live
                      WHERE live.session_id = ?1
                        AND live.status IN ('pending', 'running')
                        AND live.id <= prompts.id
                  )
                ORDER BY id DESC
                LIMIT 1
                "#,
                params![session_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Insert a fork child together with the part of the parent's record the
    /// fork holds, in one transaction, so the child's transcript and a
    /// breakpoint fork of the child read the kept conversation exactly as the
    /// parent recorded it.
    ///
    /// `held_through_prompt_id` is the parent's last prompt the fork holds,
    /// `None` when it holds none. The child receives:
    /// - a copy of each parent prompt row through that prompt, under a fresh
    ///   id minted in submission order, so inherited prompts sort before every
    ///   prompt later sent to the child;
    /// - a copy of each parent conversation event older than the parent's next
    ///   prompt (all of them when there is none), in the parent's log order.
    ///
    /// Copies keep every column but the row id, the session id, and `seq`, so
    /// a carried payload still names the prompt row that recorded its turn.
    /// Copies take the child's own seqs, contiguous from 1 in log order.
    pub fn insert_forked_session(
        &self,
        target_id: &str,
        agent_session_id: String,
        record: NewSessionRecord,
        parent_session_id: &str,
        held_through_prompt_id: Option<&str>,
    ) -> Result<SessionRecord> {
        validate_json_payload(self.connection(), &record.metadata_json)?;
        let mut row = new_active_session_row(target_id, agent_session_id, record);
        let transaction =
            Transaction::new_unchecked(self.connection(), TransactionBehavior::Immediate)?;
        row.change_seq = next_change_seq(&transaction)?;
        insert_session_row(&transaction, &row, row.change_seq)?;
        let mut copied: Vec<(&'static str, String, String)> = Vec::new();

        let held_prompts: Vec<(String, String)> = match held_through_prompt_id {
            Some(held_through) => {
                let mut statement = transaction.prepare(
                    r#"
                    SELECT id, created_at
                    FROM prompts
                    WHERE session_id = ?1 AND id <= ?2
                    ORDER BY id ASC
                    "#,
                )?;
                let rows = statement
                    .query_map(params![parent_session_id, held_through], |row| {
                        Ok((row.get(0)?, row.get(1)?))
                    })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            }
            None => Vec::new(),
        };
        {
            let mut copy_prompt = transaction.prepare(
                r#"
                INSERT INTO prompts
                    (id, session_id, created_at, updated_at, status, stop_reason,
                     error_code, error_message, prompt_json, message_id,
                     message_id_acknowledged, failure_class, failure_detail_json,
                     agent_message_id)
                SELECT ?1, ?2, created_at, updated_at, status, stop_reason,
                       error_code, error_message, prompt_json, message_id,
                       message_id_acknowledged, failure_class, failure_detail_json,
                       agent_message_id
                FROM prompts
                WHERE id = ?3
                "#,
            )?;
            for (parent_prompt_id, created_at) in held_prompts {
                let prompt_id = next_prompt_id();
                copy_prompt.execute(params![prompt_id, row.id, parent_prompt_id])?;
                copied.push(("prompts", prompt_id, created_at));
            }
        }

        // The cut follows the parent's log order: the next prompt's row is
        // written after the turn before it settled and before its own user
        // chunks, so the rows logged ahead of it are the held turns.
        let cut: Option<String> = transaction
            .query_row(
                r#"
                SELECT created_at
                FROM prompts
                WHERE session_id = ?1 AND (?2 IS NULL OR id > ?2)
                ORDER BY id ASC
                LIMIT 1
                "#,
                params![parent_session_id, held_through_prompt_id],
                |row| row.get(0),
            )
            .optional()?;
        let parent_events: Vec<(String, String, String)> = {
            let mut statement = transaction.prepare(
                r#"
                SELECT id, created_at, kind
                FROM events
                WHERE session_id = ?1 AND (?2 IS NULL OR created_at < ?2)
                ORDER BY seq ASC
                "#,
            )?;
            let rows = statement.query_map(params![parent_session_id, cut], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        {
            let mut copy_event = transaction.prepare(
                r#"
                INSERT INTO events
                    (id, created_at, level, kind, message, payload_json, source, session_id, seq)
                SELECT ?1, created_at, level, kind, message, payload_json, source, ?2, ?3
                FROM events
                WHERE id = ?4
                "#,
            )?;
            let mut child_seq = next_session_event_seq(&transaction, &row.id)?;
            for (parent_event_id, created_at, kind) in parent_events {
                if !FORK_INHERITED_EVENT_KINDS.contains(&kind.as_str()) {
                    continue;
                }
                // Minted in log order, so copies that share a timestamp keep
                // the order their originals had.
                let event_id = next_event_id();
                copy_event.execute(params![
                    event_id,
                    row.id,
                    sequence_param(child_seq)?,
                    parent_event_id
                ])?;
                row.event_seq = child_seq;
                child_seq += 1;
                copied.push(("events", event_id, created_at));
            }
        }

        if self.external_logging_enabled() {
            sink_outbox::enqueue(&transaction, "sessions", &row.id, &row.created_at)?;
            for (source_table, source_id, created_at) in &copied {
                sink_outbox::enqueue(&transaction, source_table, source_id, created_at)?;
            }
        }
        transaction.commit()?;
        Ok(row)
    }
}
