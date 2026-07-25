//! Prompt lifecycle persistence and reconciliation.

use super::*;

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
            Ok(())
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
            Ok(())
        })
    }

    /// Update a prompt's lifecycle row. `failure_class` and
    /// `failure_detail_json` follow a three-valued convention to keep callers
    /// from clobbering prior taxonomy on a status flip:
    ///
    ///   * `None` preserves the existing column value.
    ///   * `Some("")` writes SQL NULL — used to explicitly clear a value.
    ///   * `Some(value)` overwrites with the new value.
    ///
    /// Phase 1 callers all pass `None, None`; Phase 2 will populate real
    /// failure taxonomies at the supervisor settle path.
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
            // The WHERE excludes terminal statuses so a late settle from the
            // supervisor cannot overwrite a prompt that the stale-prompt
            // sweeper (or any earlier path) already moved to a terminal state.
            // `stalled` is documented as terminal; without this guard the
            // supervisor's eventual `completed`/`errored`/`cancelled` write
            // would race the sweeper.
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
                // Disambiguate: row missing entirely vs row already terminal.
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

        if !self.external_logging_enabled() {
            return update(self.connection());
        }
        let tx = rusqlite::Transaction::new_unchecked(
            self.connection(),
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let updated = update(&tx)?;
        if updated {
            sink_outbox::enqueue(&tx, "prompts", id, &now)?;
        }
        tx.commit()?;
        Ok(updated)
    }

    /// Mark every `pending`/`running` prompt row as `errored` with the given
    /// reason. Called on daemon startup so prompts orphaned by a crash get a
    /// terminal status — otherwise clients polling those prompts would never
    /// see them settle. Returns the number of rows transitioned. The rows are
    /// classified `agent_process` because the daemon restart implies the
    /// underlying agent subprocess died with the daemon.
    pub fn reconcile_orphaned_prompts(&self, reason: &str) -> Result<usize> {
        let now = current_timestamp();
        if !self.external_logging_enabled() {
            let affected = self.connection().execute(
                r#"
                UPDATE prompts
                SET status = 'errored',
                    updated_at = ?1,
                    error_code = 'agent.daemon_restart',
                    error_message = ?2,
                    failure_class = 'agent_process'
                WHERE status IN ('pending', 'running')
                "#,
                params![now, reason],
            )?;
            return Ok(affected);
        }
        // External logging path: collect affected ids first so we can enqueue
        // them transactionally with the UPDATE.
        let tx = rusqlite::Transaction::new_unchecked(
            self.connection(),
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let ids: Vec<String> = {
            let mut statement =
                tx.prepare("SELECT id FROM prompts WHERE status IN ('pending', 'running')")?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let affected = tx.execute(
            r#"
            UPDATE prompts
            SET status = 'errored',
                updated_at = ?1,
                error_code = 'agent.daemon_restart',
                error_message = ?2,
                failure_class = 'agent_process'
            WHERE status IN ('pending', 'running')
            "#,
            params![now, reason],
        )?;
        for id in &ids {
            sink_outbox::enqueue(&tx, "prompts", id, &now)?;
        }
        tx.commit()?;
        Ok(affected)
    }

    /// Mark every `pending`/`running` prompt row whose `updated_at` is
    /// older than `now - threshold` as `Stalled`. Used by the background
    /// sweeper so prompts whose agent stopped streaming ACP `session/update`
    /// notifications still settle to a terminal status — otherwise clients
    /// polling those rows would never see them resolve.
    ///
    /// Returns `(prompt_id, session_id)` pairs for every flipped row so the
    /// caller can emit a per-session `prompt.stalled` event. Idempotent:
    /// rows already in a terminal status (`completed`, `errored`,
    /// `cancelled`, `stalled`) are filtered out by the `WHERE` clause.
    pub fn mark_stalled_prompts(
        &self,
        threshold: std::time::Duration,
        reason: &str,
    ) -> Result<Vec<(String, String)>> {
        let now = Utc::now();
        let now_string = now.to_rfc3339_opts(SecondsFormat::Nanos, true);
        // The threshold cutoff timestamp is formatted the same way as
        // `prompts.updated_at` so the `<` comparison is exact at the
        // string level — every row writer goes through `current_timestamp`
        // which uses identical SecondsFormat::Nanos formatting.
        let threshold_chrono =
            chrono::Duration::from_std(threshold).map_err(|err| StackError::InvalidParam {
                field: "prompts.stale_threshold",
                reason: format!("threshold out of range: {err}"),
            })?;
        let cutoff = now
            .checked_sub_signed(threshold_chrono)
            .ok_or(StackError::InvalidParam {
                field: "prompts.stale_threshold",
                reason: "threshold subtraction underflowed the chrono range".to_owned(),
            })?;
        let cutoff_string = cutoff.to_rfc3339_opts(SecondsFormat::Nanos, true);

        if !self.external_logging_enabled() {
            let mut statement = self.connection().prepare(
                r#"
                UPDATE prompts
                SET status = 'stalled',
                    updated_at = ?1,
                    error_code = 'prompt.stalled',
                    error_message = ?2,
                    failure_class = 'stalled'
                WHERE status IN ('pending', 'running')
                  AND updated_at < ?3
                RETURNING id, session_id
                "#,
            )?;
            let rows = statement.query_map(params![now_string, reason, cutoff_string], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            return Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?);
        }
        // External logging path: run the UPDATE ... RETURNING inside an
        // IMMEDIATE transaction and enqueue an outbox row per flipped prompt
        // so the terminal status reaches Supabase atomically.
        let tx = rusqlite::Transaction::new_unchecked(
            self.connection(),
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let pairs: Vec<(String, String)> = {
            let mut statement = tx.prepare(
                r#"
                UPDATE prompts
                SET status = 'stalled',
                    updated_at = ?1,
                    error_code = 'prompt.stalled',
                    error_message = ?2,
                    failure_class = 'stalled'
                WHERE status IN ('pending', 'running')
                  AND updated_at < ?3
                RETURNING id, session_id
                "#,
            )?;
            let rows = statement.query_map(params![now_string, reason, cutoff_string], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        for (id, _session_id) in &pairs {
            sink_outbox::enqueue(&tx, "prompts", id, &now_string)?;
        }
        tx.commit()?;
        Ok(pairs)
    }

    /// Count of `pending`/`running` prompt rows older than `now - threshold`,
    /// plus the oldest such row's `updated_at`. Drives the `PromptsHealth`
    /// subsystem so `/v1/health/ready` and `acps status` can warn an
    /// operator that a row is stuck before the sweeper has a chance to
    /// flip it. The threshold matches the sweeper threshold so a single
    /// idle tick is normal and only persistent overrun shows up here.
    pub fn count_stuck_prompts(
        &self,
        threshold: std::time::Duration,
    ) -> Result<(i64, Option<String>)> {
        let now = Utc::now();
        let threshold_chrono =
            chrono::Duration::from_std(threshold).map_err(|err| StackError::InvalidParam {
                field: "prompts.stale_threshold",
                reason: format!("threshold out of range: {err}"),
            })?;
        let cutoff = now
            .checked_sub_signed(threshold_chrono)
            .ok_or(StackError::InvalidParam {
                field: "prompts.stale_threshold",
                reason: "threshold subtraction underflowed the chrono range".to_owned(),
            })?;
        let cutoff_string = cutoff.to_rfc3339_opts(SecondsFormat::Nanos, true);
        let row = self.connection().query_row(
            r#"
            SELECT COUNT(*), MIN(updated_at)
            FROM prompts
            WHERE status IN ('pending', 'running')
              AND updated_at < ?1
            "#,
            params![cutoff_string],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?)),
        )?;
        Ok(row)
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
