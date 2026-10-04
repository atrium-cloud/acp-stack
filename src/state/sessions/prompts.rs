//! Prompt lifecycle persistence and reconciliation.

use std::collections::HashMap;

use super::*;

// CONSTANTS

/// ACP `sessionUpdate` discriminators that carry a tool call's lifecycle.
const SESSION_UPDATE_TOOL_CALL: &str = "tool_call";
const SESSION_UPDATE_TOOL_CALL_UPDATE: &str = "tool_call_update";
/// ACP leaves the default status off the wire, so a `tool_call` without one
/// is `pending`.
const TOOL_CALL_STATUS_PENDING: &str = "pending";
/// The ACP tool-call statuses after which the call is no longer running.
const TOOL_CALL_CLOSED_STATUSES: &[&str] = &["completed", "failed"];

const STALE_THRESHOLD_FIELD: &str = "prompts.stale_threshold";
const TOOL_CALL_STALE_THRESHOLD_FIELD: &str = "prompts.tool_call_stale_threshold";

/// An in-flight prompt past its stale threshold.
struct StuckPrompt {
    id: String,
    session_id: String,
    updated_at: String,
    threshold: std::time::Duration,
}

/// `now - threshold` in the `SecondsFormat::Nanos` shape `current_timestamp`
/// writes; any other formatting breaks the string-level `<` comparison.
fn stale_cutoff(
    now: chrono::DateTime<Utc>,
    threshold: std::time::Duration,
    field: &'static str,
) -> Result<String> {
    let threshold_chrono =
        chrono::Duration::from_std(threshold).map_err(|err| StackError::InvalidParam {
            field,
            reason: format!("threshold out of range: {err}"),
        })?;
    let cutoff = now
        .checked_sub_signed(threshold_chrono)
        .ok_or(StackError::InvalidParam {
            field,
            reason: "threshold subtraction underflowed the chrono range".to_owned(),
        })?;
    Ok(cutoff.to_rfc3339_opts(SecondsFormat::Nanos, true))
}

/// In-flight prompts the sweeper should give up on. A prompt whose turn has a
/// tool call open is held to `open_tool_call` instead of `quiet`, because ACP
/// sends nothing while a tool runs.
fn stuck_prompts(
    conn: &rusqlite::Connection,
    thresholds: PromptStaleThresholds,
) -> Result<Vec<StuckPrompt>> {
    let now = Utc::now();
    let quiet_cutoff = stale_cutoff(now, thresholds.quiet, STALE_THRESHOLD_FIELD)?;
    let tool_call_cutoff = stale_cutoff(
        now,
        thresholds.open_tool_call,
        TOOL_CALL_STALE_THRESHOLD_FIELD,
    )?;
    let candidates: Vec<(String, String, String, String)> = {
        let mut statement = conn.prepare(
            r#"
            SELECT id, session_id, created_at, updated_at
            FROM prompts
            WHERE status IN ('pending', 'running')
              AND updated_at < ?1
            ORDER BY updated_at ASC, id ASC
            "#,
        )?;
        let rows = statement.query_map(params![quiet_cutoff], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut stuck = Vec::new();
    for (id, session_id, created_at, updated_at) in candidates {
        let threshold = if session_has_open_tool_call(conn, &session_id, &created_at)? {
            if updated_at >= tool_call_cutoff {
                continue;
            }
            thresholds.open_tool_call
        } else {
            thresholds.quiet
        };
        stuck.push(StuckPrompt {
            id,
            session_id,
            updated_at,
            threshold,
        });
    }
    Ok(stuck)
}

/// Whether a tool call the agent announced on `session_id` at or after `since`
/// has yet to reach a closed status. ACP notifications carry no prompt id, so
/// the turn is the session's activity since the prompt row was created.
fn session_has_open_tool_call(
    conn: &rusqlite::Connection,
    session_id: &str,
    since: &str,
) -> Result<bool> {
    let mut statement = conn.prepare(
        r#"
        SELECT json_extract(payload_json, '$.update.sessionUpdate'),
               json_extract(payload_json, '$.update.toolCallId'),
               json_extract(payload_json, '$.update.status')
        FROM events
        WHERE session_id = ?1
          AND kind = ?2
          AND created_at >= ?3
          AND json_extract(payload_json, '$.update.sessionUpdate') IN (?4, ?5)
        ORDER BY seq ASC
        "#,
    )?;
    let rows = statement.query_map(
        params![
            session_id,
            EVENT_KIND_SESSION_UPDATE,
            since,
            SESSION_UPDATE_TOOL_CALL,
            SESSION_UPDATE_TOOL_CALL_UPDATE
        ],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        },
    )?;
    let mut statuses: HashMap<String, String> = HashMap::new();
    for row in rows {
        let (update_kind, tool_call_id, status) = row?;
        let Some(tool_call_id) = tool_call_id else {
            continue;
        };
        // Updates count only for calls this turn announced, so one for a call
        // an earlier turn left running cannot hold this turn open. A bare
        // update keeps whatever status the call last reported.
        if update_kind == SESSION_UPDATE_TOOL_CALL {
            statuses.insert(
                tool_call_id,
                status.unwrap_or_else(|| TOOL_CALL_STATUS_PENDING.to_owned()),
            );
        } else if let Some(current) = statuses.get_mut(&tool_call_id)
            && let Some(status) = status
        {
            *current = status;
        }
    }
    Ok(statuses
        .values()
        .any(|status| !TOOL_CALL_CLOSED_STATUSES.contains(&status.as_str())))
}

/// Stamp the session that owns `prompt_id` with the next `change_seq`.
fn bump_prompt_session_change(conn: &rusqlite::Connection, prompt_id: &str) -> Result<()> {
    let session_id: String = conn.query_row(
        "SELECT session_id FROM prompts WHERE id = ?1",
        params![prompt_id],
        |row| row.get(0),
    )?;
    bump_session_change(conn, &session_id)
}

impl StateStore {
    pub fn insert_prompt(&self, record: NewPromptRecord) -> Result<PromptRecord> {
        self.insert_prompt_with_message_id(record, None)
    }

    pub fn insert_prompt_with_message_id(
        &self,
        record: NewPromptRecord,
        message_id: Option<String>,
    ) -> Result<PromptRecord> {
        validate_json_payload(self.connection(), &record.prompt_json)?;
        let now = current_timestamp();
        let row = PromptRecord {
            id: record.id,
            session_id: record.session_id,
            created_at: now.clone(),
            updated_at: now,
            status: PromptStatus::Pending.as_str().to_owned(),
            stop_reason: None,
            error_code: None,
            error_message: None,
            prompt_json: record.prompt_json,
            message_id,
            message_id_acknowledged: false,
            failure_class: None,
            failure_detail_json: None,
        };
        self.persist_with_outbox("prompts", &row.id, &row.created_at, |conn| {
            conn.execute(
                r#"
                INSERT INTO prompts
                    (id, session_id, created_at, updated_at, status, prompt_json, message_id)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                "#,
                params![
                    row.id,
                    row.session_id,
                    row.created_at,
                    row.updated_at,
                    row.status,
                    row.prompt_json,
                    row.message_id,
                ],
            )?;
            bump_session_change(conn, &row.session_id)
        })?;
        Ok(row)
    }

    pub fn get_prompt(&self, id: &str) -> Result<Option<PromptRecord>> {
        Ok(self
            .connection()
            .query_row(
                r#"
                SELECT id, session_id, created_at, updated_at, status,
                       stop_reason, error_code, error_message, prompt_json,
                       message_id, message_id_acknowledged,
                       failure_class, failure_detail_json
                FROM prompts
                WHERE id = ?1
                "#,
                params![id],
                row_to_prompt,
            )
            .optional()?)
    }

    pub fn get_prompt_by_message_id(
        &self,
        session_id: &str,
        message_id: &str,
    ) -> Result<Option<PromptRecord>> {
        Ok(self
            .connection()
            .query_row(
                r#"
                SELECT id, session_id, created_at, updated_at, status,
                       stop_reason, error_code, error_message, prompt_json,
                       message_id, message_id_acknowledged,
                       failure_class, failure_detail_json
                FROM prompts
                WHERE session_id = ?1 AND message_id = ?2
                "#,
                params![session_id, message_id],
                row_to_prompt,
            )
            .optional()?)
    }

    pub fn acknowledge_prompt_message_id(&self, prompt_id: &str, message_id: &str) -> Result<()> {
        let now = current_timestamp();
        self.persist_with_outbox("prompts", prompt_id, &now, |conn| {
            let affected = conn.execute(
                r#"
                UPDATE prompts
                SET message_id_acknowledged = 1,
                    updated_at = ?1
                WHERE id = ?2 AND message_id = ?3
                "#,
                params![now, prompt_id, message_id],
            )?;
            if affected == 0 {
                return Err(StackError::PromptNotFound {
                    id: prompt_id.to_owned(),
                });
            }
            bump_prompt_session_change(conn, prompt_id)
        })
    }

    /// Record the adapter's own id for the last agent message of this prompt's
    /// turn. It is the anchor a breakpoint fork translates into for adapters
    /// that resolve a fork point against their own transcript.
    pub fn record_prompt_agent_message_id(
        &self,
        prompt_id: &str,
        agent_message_id: &str,
    ) -> Result<()> {
        let now = current_timestamp();
        self.persist_with_outbox("prompts", prompt_id, &now, |conn| {
            let affected = conn.execute(
                r#"
                UPDATE prompts
                SET agent_message_id = ?1,
                    updated_at = ?2
                WHERE id = ?3
                "#,
                params![agent_message_id, now, prompt_id],
            )?;
            if affected == 0 {
                return Err(StackError::PromptNotFound {
                    id: prompt_id.to_owned(),
                });
            }
            bump_prompt_session_change(conn, prompt_id)
        })
    }

    /// The prompt submitted immediately before `prompt_id` in the same session.
    /// Prompt ids carry a zero-padded nanosecond prefix, so id order is
    /// submission order.
    pub fn preceding_prompt(
        &self,
        session_id: &str,
        prompt_id: &str,
    ) -> Result<Option<PrecedingPromptRecord>> {
        Ok(self
            .connection()
            .query_row(
                r#"
                SELECT id, agent_message_id
                FROM prompts
                WHERE session_id = ?1 AND id < ?2
                ORDER BY id DESC
                LIMIT 1
                "#,
                params![session_id, prompt_id],
                |row| {
                    Ok(PrecedingPromptRecord {
                        id: row.get(0)?,
                        agent_message_id: row.get(1)?,
                    })
                },
            )
            .optional()?)
    }

    /// Update a prompt's lifecycle row. `failure_class` and
    /// `failure_detail_json` are three-valued: `None` preserves the existing
    /// column, `Some("")` writes SQL NULL, `Some(value)` overwrites.
    #[allow(clippy::too_many_arguments)]
    pub fn update_prompt_status(
        &self,
        id: &str,
        status: PromptStatus,
        stop_reason: Option<&str>,
        error_code: Option<&str>,
        error_message: Option<&str>,
        failure_class: Option<&str>,
        failure_detail_json: Option<&str>,
    ) -> Result<bool> {
        let now = current_timestamp();
        let failure_class_param = failure_class.map(|value| {
            if value.is_empty() {
                None
            } else {
                Some(value.to_owned())
            }
        });
        let failure_detail_param = failure_detail_json.map(|value| {
            if value.is_empty() {
                None
            } else {
                Some(value.to_owned())
            }
        });

        let update = |conn: &rusqlite::Connection| -> Result<bool> {
            // The WHERE excludes terminal statuses (`stalled` included) so the
            // running flip and the activity touch never revive a settled row;
            // only `settle_prompt` may replace a `stalled` verdict.
            let affected = conn.execute(
                r#"
                UPDATE prompts
                SET status = ?1,
                    updated_at = ?2,
                    stop_reason = ?3,
                    error_code = ?4,
                    error_message = ?5,
                    failure_class = CASE WHEN ?6 = 1 THEN ?7 ELSE failure_class END,
                    failure_detail_json = CASE WHEN ?8 = 1 THEN ?9 ELSE failure_detail_json END
                WHERE id = ?10
                  AND status NOT IN ('completed', 'errored', 'cancelled', 'stalled')
                "#,
                params![
                    status.as_str(),
                    now,
                    stop_reason,
                    error_code,
                    error_message,
                    i64::from(failure_class_param.is_some()),
                    failure_class_param
                        .as_ref()
                        .and_then(|inner| inner.as_deref()),
                    i64::from(failure_detail_param.is_some()),
                    failure_detail_param
                        .as_ref()
                        .and_then(|inner| inner.as_deref()),
                    id
                ],
            )?;
            if affected == 0 {
                let exists: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM prompts WHERE id = ?1",
                    params![id],
                    |row| row.get(0),
                )?;
                if exists == 0 {
                    return Err(StackError::PromptNotFound { id: id.to_owned() });
                }
                tracing::warn!(
                    prompt_id = %id,
                    new_status = %status.as_str(),
                    "skipping update_prompt_status on already-terminal prompt"
                );
                return Ok(false);
            }
            Ok(true)
        };

        let tx = rusqlite::Transaction::new_unchecked(
            self.connection(),
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let updated = update(&tx)?;
        if updated {
            bump_prompt_session_change(&tx, id)?;
            if self.external_logging_enabled() {
                sink_outbox::enqueue(&tx, "prompts", id, &now)?;
            }
        }
        tx.commit()?;
        Ok(updated)
    }

    /// Write the terminal row for a finished turn. `stalled` is the sweeper's
    /// inference rather than the agent's verdict, so with `replace_stalled`
    /// the agent's own result takes its place; every other terminal row stays.
    pub fn settle_prompt(
        &self,
        id: &str,
        settlement: &PromptSettlement<'_>,
        replace_stalled: bool,
    ) -> Result<PromptSettle> {
        if !settlement.status.terminal() {
            return Err(StackError::InvalidParam {
                field: "prompt_status",
                reason: format!(
                    "`{}` is not a terminal prompt status",
                    settlement.status.as_str()
                ),
            });
        }
        let now = current_timestamp();
        let tx = rusqlite::Transaction::new_unchecked(
            self.connection(),
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let current: Option<String> = tx
            .query_row(
                "SELECT status FROM prompts WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(current) = current else {
            return Err(StackError::PromptNotFound { id: id.to_owned() });
        };
        let current: PromptStatus = current.parse()?;
        let settle = match current {
            PromptStatus::Pending | PromptStatus::Running => PromptSettle::Applied,
            PromptStatus::Stalled if replace_stalled => PromptSettle::ReplacedStall,
            PromptStatus::Completed
            | PromptStatus::Errored
            | PromptStatus::Cancelled
            | PromptStatus::Stalled => {
                tracing::warn!(
                    prompt_id = %id,
                    current_status = %current.as_str(),
                    new_status = %settlement.status.as_str(),
                    "skipping settle_prompt on already-terminal prompt"
                );
                return Ok(PromptSettle::AlreadyTerminal);
            }
        };
        tx.execute(
            r#"
            UPDATE prompts
            SET status = ?1,
                updated_at = ?2,
                stop_reason = ?3,
                error_code = ?4,
                error_message = ?5,
                failure_class = ?6,
                failure_detail_json = ?7
            WHERE id = ?8
            "#,
            params![
                settlement.status.as_str(),
                now,
                settlement.stop_reason,
                settlement.error_code,
                settlement.error_message,
                settlement.failure_class,
                settlement.failure_detail_json,
                id
            ],
        )?;
        bump_prompt_session_change(&tx, id)?;
        if self.external_logging_enabled() {
            sink_outbox::enqueue(&tx, "prompts", id, &now)?;
        }
        tx.commit()?;
        Ok(settle)
    }

    /// Mark every `pending`/`running` prompt row as `errored`, called on daemon
    /// startup so prompts orphaned by a crash still settle for pollers.
    pub fn reconcile_orphaned_prompts(&self, reason: &str) -> Result<usize> {
        let now = current_timestamp();
        // One transaction covers the UPDATE, the change-feed stamps, and the
        // outbox enqueue, so the settled rows reach every reader together.
        let tx = rusqlite::Transaction::new_unchecked(
            self.connection(),
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let settled: Vec<(String, String)> = {
            let mut statement = tx.prepare(
                r#"
                UPDATE prompts
                SET status = 'errored',
                    updated_at = ?1,
                    error_code = 'agent.daemon_restart',
                    error_message = ?2,
                    failure_class = 'agent_process'
                WHERE status IN ('pending', 'running')
                RETURNING id, session_id
                "#,
            )?;
            let rows = statement.query_map(params![now, reason], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        for (id, session_id) in &settled {
            bump_session_change(&tx, session_id)?;
            if self.external_logging_enabled() {
                sink_outbox::enqueue(&tx, "prompts", id, &now)?;
            }
        }
        tx.commit()?;
        Ok(settled.len())
    }

    /// Mark every in-flight prompt past its stale threshold as `Stalled`,
    /// returning the flipped rows with the threshold each one exceeded.
    pub fn mark_stalled_prompts(
        &self,
        thresholds: PromptStaleThresholds,
        reason: &str,
    ) -> Result<Vec<StalledPrompt>> {
        let now_string = current_timestamp();
        // The selection, the UPDATEs, the change-feed stamps, and the per-prompt
        // outbox enqueue share one IMMEDIATE transaction, so no touch lands
        // between a row qualifying and its flip, and the terminal status
        // reaches every reader atomically.
        let tx = rusqlite::Transaction::new_unchecked(
            self.connection(),
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let mut stalled = Vec::new();
        for stuck in stuck_prompts(&tx, thresholds)? {
            let affected = tx.execute(
                r#"
                UPDATE prompts
                SET status = 'stalled',
                    updated_at = ?1,
                    error_code = 'prompt.stalled',
                    error_message = ?2,
                    failure_class = 'stalled'
                WHERE id = ?3
                  AND status IN ('pending', 'running')
                "#,
                params![now_string, reason, stuck.id],
            )?;
            if affected == 0 {
                continue;
            }
            bump_session_change(&tx, &stuck.session_id)?;
            if self.external_logging_enabled() {
                sink_outbox::enqueue(&tx, "prompts", &stuck.id, &now_string)?;
            }
            stalled.push(StalledPrompt {
                prompt_id: stuck.id,
                session_id: stuck.session_id,
                threshold: stuck.threshold,
            });
        }
        tx.commit()?;
        Ok(stalled)
    }

    /// Count of in-flight prompts the next sweep would stall, plus the oldest
    /// such row's `updated_at`, driving `PromptsHealth`.
    pub fn count_stuck_prompts(
        &self,
        thresholds: PromptStaleThresholds,
    ) -> Result<(i64, Option<String>)> {
        let stuck = stuck_prompts(self.connection(), thresholds)?;
        let count = i64::try_from(stuck.len()).unwrap_or(i64::MAX);
        let oldest = stuck.into_iter().map(|prompt| prompt.updated_at).min();
        Ok((count, oldest))
    }

    pub fn in_flight_prompts_for_session(&self, session_id: &str) -> Result<Vec<PromptRecord>> {
        let mut statement = self.connection().prepare(
            r#"
            SELECT id, session_id, created_at, updated_at, status,
                   stop_reason, error_code, error_message, prompt_json,
                   message_id, message_id_acknowledged,
                   failure_class, failure_detail_json
            FROM prompts
            WHERE session_id = ?1 AND status IN ('pending', 'running')
            ORDER BY created_at ASC, id ASC
            "#,
        )?;
        let rows = statement.query_map(params![session_id], row_to_prompt)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}
