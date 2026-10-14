//! Session row queries and mutations.

use std::collections::HashMap;

use super::*;

/// `query_session_status_window`'s statement, kept apart so its query plan is testable.
/// ?1 window start, ?2 user actor, ?3 agent actor, ?4 ACP event source, ?5 target id or
/// NULL, ?6 row limit, ?7 the demotion event kind.
pub(super) const SESSION_STATUS_WINDOW_SQL: &str = r#"
            WITH scoped_sessions AS (
                SELECT id, updated_at
                FROM sessions
                WHERE (?5 IS NULL OR target_id = ?5)
            ),
            -- Created prompts and events contribute only their newest in-window row per
            -- session, found by an index seek on that session, so a streaming turn's
            -- events are never read as a whole window.
            activity AS (
                SELECT id AS session_id,
                       updated_at AS activity_at,
                       ?2 AS actor,
                       0 AS priority
                FROM scoped_sessions
                WHERE updated_at >= ?1
                UNION ALL
                SELECT session_id, activity_at, ?2 AS actor, 1 AS priority
                FROM (
                    SELECT ss.id AS session_id,
                           (
                               SELECT MAX(p.created_at)
                               FROM prompts p
                               WHERE p.session_id = ss.id
                                 AND p.created_at >= ?1
                           ) AS activity_at
                    FROM scoped_sessions ss
                )
                WHERE activity_at IS NOT NULL
                UNION ALL
                -- No index leads with (session_id, updated_at), so this branch walks the
                -- window on prompts_updated_at_idx instead of every prompt per session.
                SELECT p.session_id,
                       p.updated_at AS activity_at,
                       ?3 AS actor,
                       2 AS priority
                FROM prompts p
                JOIN scoped_sessions ss ON ss.id = p.session_id
                WHERE p.status <> 'pending'
                  AND p.updated_at >= ?1
                UNION ALL
                SELECT ss.id AS session_id,
                       e.created_at AS activity_at,
                       CASE WHEN e.source = ?4 THEN ?3 ELSE ?2 END AS actor,
                       3 AS priority
                FROM scoped_sessions ss
                JOIN events e ON e.rowid = (
                    SELECT latest.rowid
                    FROM events latest INDEXED BY events_session_id_idx
                    WHERE latest.session_id = ss.id
                      AND latest.created_at >= ?1
                      -- A demotion is bookkeeping, not activity: without this
                      -- exclusion a bulk startup reconcile would stamp every
                      -- historical session into the window as user activity.
                      AND latest.kind <> ?7
                    ORDER BY latest.created_at DESC, latest.id DESC
                    LIMIT 1
                )
                UNION ALL
                SELECT pr.subject_id AS session_id,
                       pr.created_at AS activity_at,
                       ?3 AS actor,
                       4 AS priority
                FROM permission_requests pr
                JOIN scoped_sessions ss ON ss.id = pr.subject_id
                WHERE pr.status = 'pending'
                  AND pr.source = 'acp'
                  AND pr.subject_id IS NOT NULL
                  AND pr.created_at >= ?1
            ),
            ranked_activity AS (
                SELECT session_id,
                       activity_at,
                       actor,
                       ROW_NUMBER() OVER (
                           PARTITION BY session_id
                           ORDER BY activity_at DESC, priority DESC
                       ) AS row_number
                FROM activity
            ),
            window_sessions AS (
                SELECT session_id,
                       activity_at,
                       actor
                FROM ranked_activity
                WHERE row_number = 1
            ),
            latest_prompts AS (
                SELECT id, session_id, created_at, updated_at, status,
                       stop_reason, error_code, error_message, message_id,
                       message_id_acknowledged
                FROM (
                    SELECT p.id, p.session_id, p.created_at, p.updated_at, p.status,
                           p.stop_reason, p.error_code, p.error_message, p.message_id,
                           p.message_id_acknowledged,
                           ROW_NUMBER() OVER (
                               PARTITION BY p.session_id
                               ORDER BY
                                   CASE WHEN p.status IN ('pending', 'running') THEN 0 ELSE 1 END ASC,
                                   CASE WHEN p.status IN ('pending', 'running') THEN p.created_at END ASC,
                                   CASE WHEN p.status IN ('pending', 'running') THEN p.id END ASC,
                                   CASE WHEN p.status NOT IN ('pending', 'running') THEN p.created_at END DESC,
                                   CASE WHEN p.status NOT IN ('pending', 'running') THEN p.id END DESC
                           ) AS row_number
                    FROM prompts p
                    JOIN window_sessions ws ON ws.session_id = p.session_id
                    JOIN sessions owner ON owner.id = p.session_id
                    -- A fork child's inherited prompts predate the child; its
                    -- state starts from its own first prompt.
                    WHERE p.created_at >= owner.created_at
                )
                WHERE row_number = 1
            ),
            pending_acp_permissions AS (
                SELECT id, session_id, created_at, updated_at
                FROM (
                    SELECT pr.id, pr.subject_id AS session_id, pr.created_at, pr.updated_at,
                           ROW_NUMBER() OVER (
                               PARTITION BY pr.subject_id
                               ORDER BY pr.created_at ASC, pr.id ASC
                           ) AS row_number
                    FROM permission_requests pr
                    JOIN window_sessions ws ON ws.session_id = pr.subject_id
                    WHERE pr.status = 'pending'
                      AND pr.source = 'acp'
                      AND pr.subject_id IS NOT NULL
                )
                WHERE row_number = 1
            )
            SELECT s.id AS session_id,
                   s.target_id AS target_id,
                   s.agent_session_id,
                   s.created_at,
                   s.updated_at,
                   s.status,
                   s.agent_id,
                   s.cwd,
                   s.title,
                   r.activity_at,
                   r.actor,
                   lp.id,
                   lp.created_at,
                   lp.updated_at,
                   lp.status,
                   lp.stop_reason,
                   lp.error_code,
                   lp.error_message,
                   lp.message_id,
                   lp.message_id_acknowledged,
                   pp.id,
                   pp.created_at,
                   pp.updated_at,
                   (
                       SELECT e.created_at
                       FROM events e INDEXED BY events_session_id_idx
                       WHERE e.session_id = s.id
                         AND e.created_at >= lp.created_at
                         AND e.kind = 'session.update'
                         AND e.source = ?4
                       ORDER BY e.created_at ASC
                       LIMIT 1
                   ) AS prompt_stream_started_at,
                   s.change_seq,
                   (
                       SELECT COALESCE(MAX(e.seq), 0)
                       FROM events e
                       WHERE e.session_id = s.id
                   ) AS event_seq
            FROM sessions s
            JOIN window_sessions r ON r.session_id = s.id
            LEFT JOIN latest_prompts lp ON lp.session_id = s.id
            LEFT JOIN pending_acp_permissions pp ON pp.session_id = s.id
            ORDER BY r.activity_at DESC, s.id DESC
            LIMIT ?6
            "#;

/// The sync event [`StateStore::record_session_list_sync`] writes with the rows it changes.
struct ListSyncEvent<'a> {
    agent_id: &'a str,
    skipped_invalid_cwd: u32,
}

/// One row write a session-list sync planned.
enum ListedSessionWrite {
    Insert(SessionRecord),
    Update(SessionRecord),
}

impl StateStore {
    /// Apply a partial ACP `session_info_update`. The outer `Option`
    /// distinguishes an omitted field from an explicit `null`.
    pub fn update_session_info(
        &self,
        id: &str,
        title: Option<Option<&str>>,
        agent_updated_at: Option<Option<&str>>,
        agent_meta: Option<&serde_json::Map<String, serde_json::Value>>,
    ) -> Result<()> {
        let record = self
            .get_session(id)?
            .ok_or_else(|| StackError::SessionNotFound { id: id.to_owned() })?;
        let mut metadata = metadata_object(&record.metadata_json)?;

        if let Some(agent_updated_at) = agent_updated_at {
            metadata.insert(
                "agent_updated_at".to_owned(),
                agent_updated_at
                    .map(|value| serde_json::Value::String(value.to_owned()))
                    .unwrap_or(serde_json::Value::Null),
            );
        }
        if let Some(agent_meta) = agent_meta {
            metadata.insert(
                "agent_meta".to_owned(),
                serde_json::Value::Object(agent_meta.clone()),
            );
        }

        let title = title
            .map(|value| value.map(str::to_owned))
            .unwrap_or(record.title);
        let metadata_json = serde_json::Value::Object(metadata).to_string();
        let now = current_timestamp();
        self.persist_with_outbox("sessions", id, &now, |conn| {
            let affected = conn.execute(
                r#"
                UPDATE sessions
                SET title = ?1, metadata_json = ?2, updated_at = ?3
                WHERE id = ?4
                "#,
                params![title, metadata_json, now, id],
            )?;
            if affected == 0 {
                return Err(StackError::SessionNotFound { id: id.to_owned() });
            }
            bump_session_change(conn, id)
        })
    }

    /// Replace the stored slash-command list. ACP `available_commands_update`
    /// is latest-wins, so an empty list legitimately overwrites. Returns
    /// whether a write happened.
    pub fn replace_session_available_commands(
        &self,
        id: &str,
        commands: &[SessionAvailableCommand],
    ) -> Result<bool> {
        let record = self
            .get_session(id)?
            .ok_or_else(|| StackError::SessionNotFound { id: id.to_owned() })?;
        let mut metadata = metadata_object(&record.metadata_json)?;

        let commands_value =
            serde_json::to_value(commands).map_err(|err| StackError::StateInvalidJson {
                field: "sessions.metadata_json",
                reason: err.to_string(),
            })?;
        // Agents may re-advertise an identical list on every turn.
        if metadata.get(SESSION_METADATA_AVAILABLE_COMMANDS) == Some(&commands_value) {
            return Ok(false);
        }
        let now = current_timestamp();
        metadata.insert(
            SESSION_METADATA_AVAILABLE_COMMANDS.to_owned(),
            commands_value,
        );
        metadata.insert(
            SESSION_METADATA_AVAILABLE_COMMANDS_UPDATED_AT.to_owned(),
            serde_json::Value::String(now.clone()),
        );

        let metadata_json = serde_json::Value::Object(metadata).to_string();
        self.persist_with_outbox("sessions", id, &now, |conn| {
            let affected = conn.execute(
                r#"
                UPDATE sessions
                SET metadata_json = ?1, updated_at = ?2
                WHERE id = ?3
                "#,
                params![metadata_json, now, id],
            )?;
            if affected == 0 {
                return Err(StackError::SessionNotFound { id: id.to_owned() });
            }
            bump_session_change(conn, id)
        })?;
        Ok(true)
    }

    /// Replace the stored session config-option snapshot (latest-wins).
    /// Returns whether a write happened.
    pub fn replace_session_config_options(
        &self,
        id: &str,
        options_value: serde_json::Value,
    ) -> Result<bool> {
        let record = self
            .get_session(id)?
            .ok_or_else(|| StackError::SessionNotFound { id: id.to_owned() })?;
        let mut metadata = metadata_object(&record.metadata_json)?;

        if metadata.get(SESSION_METADATA_CONFIG_OPTIONS) == Some(&options_value) {
            return Ok(false);
        }
        let now = current_timestamp();
        metadata.insert(SESSION_METADATA_CONFIG_OPTIONS.to_owned(), options_value);
        metadata.insert(
            SESSION_METADATA_CONFIG_OPTIONS_UPDATED_AT.to_owned(),
            serde_json::Value::String(now.clone()),
        );

        let metadata_json = serde_json::Value::Object(metadata).to_string();
        self.persist_with_outbox("sessions", id, &now, |conn| {
            let affected = conn.execute(
                r#"
                UPDATE sessions
                SET metadata_json = ?1, updated_at = ?2
                WHERE id = ?3
                "#,
                params![metadata_json, now, id],
            )?;
            if affected == 0 {
                return Err(StackError::SessionNotFound { id: id.to_owned() });
            }
            bump_session_change(conn, id)
        })?;
        Ok(true)
    }

    pub fn query_sessions(&self, filter: SessionFilter<'_>) -> Result<Vec<SessionRecord>> {
        let mut sql = format!("SELECT {SESSION_COLUMNS} FROM sessions WHERE 1=1");
        let mut bindings: Vec<rusqlite::types::Value> = Vec::new();
        if let Some(since) = filter.since {
            sql.push_str(" AND updated_at >= ?");
            bindings.push(rusqlite::types::Value::Text(since.to_owned()));
        }
        if let Some(until) = filter.until {
            sql.push_str(" AND updated_at < ?");
            bindings.push(rusqlite::types::Value::Text(until.to_owned()));
        }
        if let Some(status) = filter.status {
            sql.push_str(" AND status = ?");
            bindings.push(rusqlite::types::Value::Text(status.to_owned()));
        }
        if let Some(target_id) = filter.target_id {
            sql.push_str(" AND target_id = ?");
            bindings.push(rusqlite::types::Value::Text(target_id.to_owned()));
        }
        if let Some(after) = filter.after_id {
            match filter.order {
                LogOrder::Desc => sql.push_str(
                    " AND (updated_at, id) < (SELECT updated_at, id FROM sessions WHERE id = ?)",
                ),
                LogOrder::Asc => sql.push_str(
                    " AND (updated_at, id) > (SELECT updated_at, id FROM sessions WHERE id = ?)",
                ),
            }
            bindings.push(rusqlite::types::Value::Text(after.to_owned()));
        }
        let direction = filter.order.sql_keyword();
        sql.push_str(&format!(
            " ORDER BY updated_at {direction}, id {direction} LIMIT ?"
        ));
        bindings.push(rusqlite::types::Value::Integer(i64::from(filter.limit)));
        let mut statement = self.connection().prepare(&sql)?;
        let rows =
            statement.query_map(rusqlite::params_from_iter(bindings.iter()), row_to_session)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn get_session(&self, id: &str) -> Result<Option<SessionRecord>> {
        Ok(self
            .connection()
            .query_row(
                &format!("SELECT {SESSION_COLUMNS} FROM sessions WHERE id = ?1"),
                params![id],
                row_to_session,
            )
            .optional()?)
    }

    pub fn get_session_by_target_agent_session_id(
        &self,
        target_id: &str,
        agent_session_id: &str,
    ) -> Result<Option<SessionRecord>> {
        Ok(self
            .connection()
            .query_row(
                &format!(
                    "SELECT {SESSION_COLUMNS} FROM sessions \
                     WHERE target_id = ?1 AND agent_session_id = ?2"
                ),
                params![target_id, agent_session_id],
                row_to_session,
            )
            .optional()?)
    }

    pub fn session_update_bounds(&self) -> Result<Option<SessionUpdateBounds>> {
        let first = self
            .connection()
            .query_row(
                r#"
                SELECT updated_at
                FROM sessions
                ORDER BY updated_at ASC, id ASC
                LIMIT 1
                "#,
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let Some(first_updated_at) = first else {
            return Ok(None);
        };
        let (latest_updated_at, latest_status) = self.connection().query_row(
            r#"
            SELECT updated_at, status
            FROM sessions
            ORDER BY updated_at DESC, id DESC
            LIMIT 1
            "#,
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?;
        Ok(Some(SessionUpdateBounds {
            first_updated_at,
            latest_updated_at,
            latest_status,
        }))
    }

    pub fn query_active_session_activity(&self, limit: u32) -> Result<Vec<SessionActivityRecord>> {
        let mut statement = self.connection().prepare(
            r#"
            WITH active_sessions AS (
                SELECT id, target_id, agent_session_id, created_at, updated_at, status, agent_id, cwd, title
                FROM sessions
                WHERE status = ?1
            ),
            activity AS (
                SELECT e.session_id,
                       e.created_at AS activity_at,
                       CASE WHEN e.source = ?2 THEN ?3 ELSE ?4 END AS actor,
                       3 AS priority
                FROM events e
                JOIN active_sessions s ON s.id = e.session_id
                UNION ALL
                SELECT p.session_id,
                       p.created_at AS activity_at,
                       ?4 AS actor,
                       1 AS priority
                FROM prompts p
                JOIN active_sessions s ON s.id = p.session_id
                UNION ALL
                SELECT p.session_id,
                       p.updated_at AS activity_at,
                       ?3 AS actor,
                       2 AS priority
                FROM prompts p
                JOIN active_sessions s ON s.id = p.session_id
                WHERE p.status <> 'pending'
                UNION ALL
                SELECT s.id,
                       s.updated_at AS activity_at,
                       ?4 AS actor,
                       0 AS priority
                FROM active_sessions s
            ),
            ranked_activity AS (
                SELECT session_id,
                       activity_at,
                       actor,
                       ROW_NUMBER() OVER (
                           PARTITION BY session_id
                           ORDER BY activity_at DESC, priority DESC
                       ) AS row_number
                FROM activity
            )
            SELECT s.id AS session_id,
                   s.target_id AS target_id,
                   s.agent_session_id,
                   s.created_at,
                   s.updated_at,
                   s.status,
                   s.agent_id,
                   s.cwd,
                   s.title,
                   r.activity_at,
                   r.actor
            FROM active_sessions s
            JOIN ranked_activity r ON r.session_id = s.id AND r.row_number = 1
            ORDER BY r.activity_at DESC, s.id DESC
            LIMIT ?5
            "#,
        )?;
        let rows = statement.query_map(
            params![
                SESSION_STATUS_ACTIVE,
                EVENT_SOURCE_ACP,
                SESSION_ACTIVITY_ACTOR_AGENT,
                SESSION_ACTIVITY_ACTOR_USER,
                i64::from(limit),
            ],
            |row| {
                Ok(SessionActivityRecord {
                    id: row.get(0)?,
                    target_id: row.get(1)?,
                    agent_session_id: row.get(2)?,
                    created_at: row.get(3)?,
                    updated_at: row.get(4)?,
                    status: row.get(5)?,
                    agent_id: row.get(6)?,
                    cwd: row.get(7)?,
                    title: row.get(8)?,
                    last_activity_at: row.get(9)?,
                    last_activity_from: row.get(10)?,
                })
            },
        )?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn query_session_status_window(
        &self,
        since: &str,
        target_id: Option<&str>,
        limit: u32,
    ) -> Result<Vec<SessionStatusRecord>> {
        let mut statement = self.connection().prepare(SESSION_STATUS_WINDOW_SQL)?;
        let rows = statement.query_map(
            params![
                since,
                SESSION_ACTIVITY_ACTOR_USER,
                SESSION_ACTIVITY_ACTOR_AGENT,
                EVENT_SOURCE_ACP,
                target_id,
                i64::from(limit),
                EVENT_KIND_SESSION_AVAILABLE,
            ],
            |row| {
                let prompt_id: Option<String> = row.get(11)?;
                let latest_prompt = match prompt_id {
                    Some(id) => Some(SessionStatusPromptRecord {
                        id,
                        created_at: row.get(12)?,
                        updated_at: row.get(13)?,
                        status: row.get(14)?,
                        stop_reason: row.get(15)?,
                        error_code: row.get(16)?,
                        error_message: row.get(17)?,
                        message_id: row.get(18)?,
                        message_id_acknowledged: row.get::<_, Option<i64>>(19)?.unwrap_or(0) != 0,
                    }),
                    None => None,
                };
                let permission_id: Option<String> = row.get(20)?;
                let pending_permission = match permission_id {
                    Some(id) => Some(SessionStatusPermissionRecord {
                        id,
                        created_at: row.get(21)?,
                        updated_at: row.get(22)?,
                    }),
                    None => None,
                };
                Ok(SessionStatusRecord {
                    id: row.get(0)?,
                    target_id: row.get(1)?,
                    agent_session_id: row.get(2)?,
                    created_at: row.get(3)?,
                    updated_at: row.get(4)?,
                    status: row.get(5)?,
                    agent_id: row.get(6)?,
                    cwd: row.get(7)?,
                    title: row.get(8)?,
                    last_activity_at: row.get(9)?,
                    last_activity_from: row.get(10)?,
                    latest_prompt,
                    pending_permission,
                    prompt_stream_started_at: row.get(23)?,
                    change_seq: sequence_column(row, 24)?,
                    event_seq: sequence_column(row, 25)?,
                })
            },
        )?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn query_restart_blockers(
        &self,
        target_id: Option<&str>,
    ) -> Result<Vec<RestartBlockerRecord>> {
        let mut statement = self.connection().prepare(
            r#"
            WITH active_sessions AS (
                SELECT id, target_id, updated_at
                FROM sessions
                WHERE status = ?1
                  AND (?2 IS NULL OR target_id = ?2)
            )
            SELECT s.id,
                   s.target_id,
                   CASE p.status
                       WHEN 'pending' THEN 'prompt_sent'
                       WHEN 'running' THEN 'working'
                       ELSE 'blocked'
                   END AS state,
                   p.id AS prompt_id,
                   p.status AS prompt_status,
                   p.stop_reason AS prompt_stop_reason,
                   NULL AS permission_id,
                   p.created_at AS blocker_created_at,
                   0 AS blocker_priority
            FROM active_sessions s
            JOIN prompts p ON p.session_id = s.id
            WHERE p.status IN ('pending', 'running')
            UNION ALL
            SELECT s.id,
                   s.target_id,
                   'permission_required' AS state,
                   NULL AS prompt_id,
                   NULL AS prompt_status,
                   NULL AS prompt_stop_reason,
                   pr.id AS permission_id,
                   pr.created_at AS blocker_created_at,
                   1 AS blocker_priority
            FROM active_sessions s
            JOIN permission_requests pr ON pr.subject_id = s.id
            WHERE pr.status = 'pending'
              AND pr.source = 'acp'
              AND pr.subject_id IS NOT NULL
            ORDER BY 8 DESC, 9 DESC, 1 ASC
            "#,
        )?;
        let rows = statement.query_map(params![SESSION_STATUS_ACTIVE, target_id], |row| {
            Ok(RestartBlockerRecord {
                session_id: row.get(0)?,
                target_id: row.get(1)?,
                state: row.get(2)?,
                prompt_id: row.get(3)?,
                prompt_status: row.get(4)?,
                prompt_stop_reason: row.get(5)?,
                permission_id: row.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn query_pending_acp_permission_ids_for_target(
        &self,
        target_id: &str,
    ) -> Result<Vec<String>> {
        let mut statement = self.connection().prepare(
            r#"
            SELECT pr.id
            FROM permission_requests pr
            JOIN sessions s ON s.id = pr.subject_id
            WHERE pr.status = 'pending'
              AND pr.source = 'acp'
              AND s.target_id = ?1
            ORDER BY pr.created_at ASC, pr.id ASC
            "#,
        )?;
        let rows = statement.query_map(params![target_id], |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn insert_session(&self, record: NewSessionRecord) -> Result<SessionRecord> {
        let target_id = record.agent_id.clone();
        self.insert_session_for_target(&target_id, record.id.clone(), record)
    }

    pub fn insert_session_for_target(
        &self,
        target_id: &str,
        agent_session_id: String,
        record: NewSessionRecord,
    ) -> Result<SessionRecord> {
        validate_json_payload(self.connection(), &record.metadata_json)?;
        let mut row = new_active_session_row(target_id, agent_session_id, record);
        row.change_seq =
            self.persist_with_outbox("sessions", &row.id, &row.created_at, |conn| {
                let change_seq = next_change_seq(conn)?;
                insert_session_row(conn, &row, change_seq)?;
                Ok(change_seq)
            })?;
        Ok(row)
    }

    /// Derive each record's target from its `agent_id` and upsert one at a
    /// time, deduping on `(target_id, agent_session_id)` because the agent's
    /// session id is the stable external identity, not the row `id`.
    pub fn upsert_listed_sessions(
        &self,
        records: Vec<ListedSessionRecord>,
    ) -> Result<ListedSessionUpsertCounts> {
        let mut counts = ListedSessionUpsertCounts::default();
        for record in records {
            let target_id = record.agent_id.clone();
            let record_counts = self.upsert_listed_sessions_for_target(&target_id, vec![record])?;
            counts.upserted += record_counts.upserted;
            counts.updated += record_counts.updated;
        }
        Ok(counts)
    }

    /// Apply one listing's rows for `target_id`; see [`Self::record_session_list_sync`], which
    /// also records the sync event.
    pub fn upsert_listed_sessions_for_target(
        &self,
        target_id: &str,
        records: Vec<ListedSessionRecord>,
    ) -> Result<ListedSessionUpsertCounts> {
        self.apply_listed_sessions(target_id, records, None)
    }

    /// Apply one ACP `session/list` result for `target_id`: new rows are inserted `available`,
    /// rows whose stored values the listing moves are updated, and the rest are not written.
    /// Every write, plus a `session.list_synced` event naming the counts, lands in one
    /// transaction; a listing that changes nothing opens no transaction and records no event.
    pub fn record_session_list_sync(
        &self,
        target_id: &str,
        agent_id: &str,
        records: Vec<ListedSessionRecord>,
        skipped_invalid_cwd: u32,
    ) -> Result<ListedSessionUpsertCounts> {
        self.apply_listed_sessions(
            target_id,
            records,
            Some(ListSyncEvent {
                agent_id,
                skipped_invalid_cwd,
            }),
        )
    }

    fn apply_listed_sessions(
        &self,
        target_id: &str,
        records: Vec<ListedSessionRecord>,
        sync_event: Option<ListSyncEvent<'_>>,
    ) -> Result<ListedSessionUpsertCounts> {
        // Dedupe on `(target_id, agent_session_id)`: the agent's session id is the stable
        // external identity, not the row `id`. A planned row replaces its entry, so a listing
        // naming one session twice updates the row its first mention planned.
        let mut stored: HashMap<String, SessionRecord> = self
            .query_sessions_for_target(target_id)?
            .into_iter()
            .map(|row| (row.agent_session_id.clone(), row))
            .collect();
        let mut writes: Vec<ListedSessionWrite> = Vec::new();
        let mut counts = ListedSessionUpsertCounts::default();
        for record in records {
            let listed_metadata = metadata_object(&record.metadata_json)?;
            let listed_updated_at = record
                .updated_at
                .as_deref()
                .map(normalize_listed_session_timestamp)
                .transpose()?;
            let planned = match stored.get(&record.agent_session_id) {
                Some(existing) => {
                    // A listing without `updated_at` reports no activity, so
                    // the stored timestamp stands.
                    let updated_at =
                        listed_updated_at.unwrap_or_else(|| existing.updated_at.clone());
                    let status = if existing.status == SESSION_STATUS_ACTIVE
                        || existing.status == SESSION_STATUS_CLOSED
                    {
                        existing.status.clone()
                    } else {
                        SESSION_STATUS_AVAILABLE.to_owned()
                    };
                    // The listing's keys layer over what the runtime stored
                    // for the session (config options, commands, fork lineage).
                    let existing_metadata = metadata_object(&existing.metadata_json)?;
                    let mut metadata = existing_metadata.clone();
                    metadata.extend(listed_metadata);
                    let changed = existing.updated_at != updated_at
                        || existing.status != status
                        || existing.agent_id != record.agent_id
                        || existing.cwd != record.cwd
                        || existing.title != record.title
                        || metadata != existing_metadata;
                    if !changed {
                        continue;
                    }
                    counts.updated += 1;
                    let row = SessionRecord {
                        updated_at,
                        status,
                        agent_id: record.agent_id,
                        cwd: record.cwd,
                        title: record.title,
                        metadata_json: serde_json::Value::Object(metadata).to_string(),
                        ..existing.clone()
                    };
                    writes.push(ListedSessionWrite::Update(row.clone()));
                    row
                }
                None => {
                    let created_at = current_timestamp();
                    let updated_at = listed_updated_at.unwrap_or_else(|| created_at.clone());
                    counts.upserted += 1;
                    let row = SessionRecord {
                        id: record.id,
                        target_id: target_id.to_owned(),
                        agent_session_id: record.agent_session_id,
                        created_at,
                        updated_at,
                        status: SESSION_STATUS_AVAILABLE.to_owned(),
                        agent_id: record.agent_id,
                        cwd: record.cwd,
                        title: record.title,
                        metadata_json: record.metadata_json,
                        change_seq: 0,
                        event_seq: 0,
                    };
                    writes.push(ListedSessionWrite::Insert(row.clone()));
                    row
                }
            };
            stored.insert(planned.agent_session_id.clone(), planned);
        }
        if writes.is_empty() {
            return Ok(counts);
        }

        let event = sync_event.map(|sync| Event {
            id: next_event_id(),
            created_at: current_timestamp(),
            level: "info".to_owned(),
            kind: EVENT_KIND_SESSION_LIST_SYNCED.to_owned(),
            message: "ACP session list synced".to_owned(),
            payload_json: serde_json::json!({
                "target_id": target_id,
                "agent_id": sync.agent_id,
                "upserted": counts.upserted,
                "updated": counts.updated,
                "skipped_invalid_cwd": sync.skipped_invalid_cwd,
            })
            .to_string(),
            source: EVENT_SOURCE_SYSTEM.to_owned(),
            session_id: None,
            seq: None,
        });
        let tx = self.begin_immediate("sessions")?;
        for write in &writes {
            let row = match write {
                ListedSessionWrite::Insert(row) => {
                    insert_session_row(&tx, row, next_change_seq(&tx)?)?;
                    row
                }
                ListedSessionWrite::Update(row) => {
                    // Status is re-guarded in SQL: a session attached or closed since the
                    // planning read keeps that status.
                    tx.execute(
                        r#"
                        UPDATE sessions
                        SET updated_at = ?1,
                            status = CASE
                                WHEN status IN (?2, ?3) THEN status
                                ELSE ?4
                            END,
                            agent_id = ?5,
                            cwd = ?6,
                            title = ?7,
                            metadata_json = ?8
                        WHERE id = ?9
                        "#,
                        params![
                            row.updated_at,
                            SESSION_STATUS_ACTIVE,
                            SESSION_STATUS_CLOSED,
                            SESSION_STATUS_AVAILABLE,
                            row.agent_id,
                            row.cwd,
                            row.title,
                            row.metadata_json,
                            row.id,
                        ],
                    )?;
                    bump_session_change(&tx, &row.id)?;
                    row
                }
            };
            if self.external_logging_enabled() {
                sink_outbox::enqueue(&tx, "sessions", &row.id, &row.updated_at)?;
            }
        }
        if let Some(event) = &event {
            insert_unscoped_event_row(&tx, event)?;
            if self.external_logging_enabled() {
                sink_outbox::enqueue(&tx, "events", &event.id, &event.created_at)?;
            }
        }
        tx.commit()?;
        if let Some(event) = event {
            self.publish(move |hub| hub.publish_log_event(&event));
        }
        Ok(counts)
    }

    /// Every session row of one target.
    fn query_sessions_for_target(&self, target_id: &str) -> Result<Vec<SessionRecord>> {
        let mut statement = self.connection().prepare(&format!(
            "SELECT {SESSION_COLUMNS} FROM sessions WHERE target_id = ?1"
        ))?;
        let rows = statement.query_map(params![target_id], row_to_session)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn rename_session_target_id(
        &self,
        old_target_id: &str,
        new_target_id: &str,
    ) -> Result<usize> {
        if old_target_id == new_target_id {
            return Ok(0);
        }
        // Detect UNIQUE(target_id, agent_session_id) collisions up front,
        // rather than surfacing a raw SQLite violation partway through.
        let collisions = self.connection().query_row(
            r#"
            SELECT COUNT(*)
            FROM sessions AS moving
            WHERE moving.target_id = ?1
              AND EXISTS (
                  SELECT 1 FROM sessions AS existing
                  WHERE existing.target_id = ?2
                    AND existing.agent_session_id = moving.agent_session_id
              )
            "#,
            params![old_target_id, new_target_id],
            |row| row.get::<_, i64>(0),
        )?;
        if collisions > 0 {
            return Err(StackError::SessionTargetRenameConflict {
                old_target_id: old_target_id.to_owned(),
                new_target_id: new_target_id.to_owned(),
                count: usize::try_from(collisions).unwrap_or(usize::MAX),
            });
        }
        let updated_at = current_timestamp();
        // Move every row in one transaction so a failure can never leave the
        // sessions table split across the old and new target ids.
        let ids = self.persist_many_with_outbox("sessions", &updated_at, |conn| {
            let ids = {
                let mut statement =
                    conn.prepare("SELECT id FROM sessions WHERE target_id = ?1 ORDER BY id")?;
                let rows =
                    statement.query_map(params![old_target_id], |row| row.get::<_, String>(0))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            conn.execute(
                r#"
                UPDATE sessions
                SET target_id = ?1, updated_at = ?2
                WHERE target_id = ?3
                "#,
                params![new_target_id, updated_at, old_target_id],
            )?;
            for id in &ids {
                bump_session_change(conn, id)?;
            }
            Ok(ids)
        })?;
        Ok(ids.len())
    }

    /// Hard-delete the session row with its prompts and events, leaving a
    /// tombstone that reports the deletion on the change feed. Returns `None`
    /// for an unknown id (repeat deletes succeed silently per ACP
    /// `session/delete`). Permission rows stay: they are the durable security
    /// log, not session history.
    pub fn delete_session(&self, id: &str) -> Result<Option<SessionRecord>> {
        let Some(record) = self.get_session(id)? else {
            return Ok(None);
        };
        let now = current_timestamp();
        self.persist_with_outbox("sessions", id, &now, |conn| {
            conn.execute("DELETE FROM prompts WHERE session_id = ?1", params![id])?;
            conn.execute("DELETE FROM events WHERE session_id = ?1", params![id])?;
            conn.execute("DELETE FROM sessions WHERE id = ?1", params![id])?;
            insert_session_tombstone(conn, id, &now)
        })?;
        Ok(Some(record))
    }

    pub fn update_session_status(&self, id: &str, status: &str) -> Result<()> {
        let now = current_timestamp();
        self.persist_with_outbox("sessions", id, &now, |conn| {
            let affected = conn.execute(
                r#"
                UPDATE sessions
                SET status = ?1, updated_at = ?2
                WHERE id = ?3
                "#,
                params![status, now, id],
            )?;
            if affected == 0 {
                return Err(StackError::SessionNotFound { id: id.to_owned() });
            }
            bump_session_change(conn, id)
        })
    }

    pub fn update_session_status_and_cwd(&self, id: &str, status: &str, cwd: &str) -> Result<()> {
        let now = current_timestamp();
        self.persist_with_outbox("sessions", id, &now, |conn| {
            let affected = conn.execute(
                r#"
                UPDATE sessions
                SET status = ?1, cwd = ?2, updated_at = ?3
                WHERE id = ?4
                "#,
                params![status, cwd, now, id],
            )?;
            if affected == 0 {
                return Err(StackError::SessionNotFound { id: id.to_owned() });
            }
            bump_session_change(conn, id)
        })
    }

    /// Demote every `active` session (of one target, or all) to `available`,
    /// returning the demoted ids. Runs at serve startup (all targets) and on
    /// agent teardown (that target only), where no session can still be
    /// attached to a live adapter.
    ///
    /// `updated_at` is deliberately left untouched: it doubles as the
    /// last-activity timestamp for list ranges, `since` pagination, and the
    /// status window, and a demotion is not activity.
    pub fn reconcile_orphaned_sessions(&self, target_id: Option<&str>) -> Result<Vec<String>> {
        const SQL: &str = r#"
            UPDATE sessions
            SET status = ?1
            WHERE status = ?2
              AND (?3 IS NULL OR target_id = ?3)
            RETURNING id
        "#;
        let now = current_timestamp();
        self.persist_many_with_outbox("sessions", &now, |conn| {
            let ids = {
                let mut statement = conn.prepare(SQL)?;
                let rows = statement.query_map(
                    params![SESSION_STATUS_AVAILABLE, SESSION_STATUS_ACTIVE, target_id],
                    |row| row.get::<_, String>(0),
                )?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            for id in &ids {
                bump_session_change(conn, id)?;
            }
            Ok(ids)
        })
    }

    /// Demote `active` sessions with no in-flight prompt, no pending ACP
    /// permission request, and no prompt/session activity within `threshold`
    /// to `available`, returning the demoted ids. Like the orphan reconcile,
    /// the sweep never touches `updated_at`.
    pub fn mark_idle_sessions(&self, threshold: std::time::Duration) -> Result<Vec<String>> {
        const SQL: &str = r#"
            UPDATE sessions
            SET status = ?1
            WHERE status = ?2
              AND NOT EXISTS (
                  SELECT 1 FROM prompts p
                  WHERE p.session_id = sessions.id
                    AND p.status IN ('pending', 'running')
              )
              AND NOT EXISTS (
                  SELECT 1 FROM permission_requests pr
                  WHERE pr.subject_id = sessions.id
                    AND pr.source = 'acp'
                    AND pr.status = 'pending'
              )
              AND max(
                  COALESCE(
                      (SELECT MAX(p.updated_at) FROM prompts p WHERE p.session_id = sessions.id),
                      updated_at
                  ),
                  updated_at
              ) < ?3
            RETURNING id
        "#;
        let now = Utc::now();
        let now_string = now.to_rfc3339_opts(SecondsFormat::Nanos, true);
        // The cutoff MUST use the same SecondsFormat::Nanos formatting as
        // `current_timestamp`, or the string-level `<` comparison is wrong.
        let threshold_chrono =
            chrono::Duration::from_std(threshold).map_err(|err| StackError::InvalidParam {
                field: "sessions.idle_threshold",
                reason: format!("threshold out of range: {err}"),
            })?;
        let cutoff = now
            .checked_sub_signed(threshold_chrono)
            .ok_or(StackError::InvalidParam {
                field: "sessions.idle_threshold",
                reason: "threshold subtraction underflowed the chrono range".to_owned(),
            })?;
        let cutoff_string = cutoff.to_rfc3339_opts(SecondsFormat::Nanos, true);
        self.persist_many_with_outbox("sessions", &now_string, |conn| {
            let ids = {
                let mut statement = conn.prepare(SQL)?;
                let rows = statement.query_map(
                    params![
                        SESSION_STATUS_AVAILABLE,
                        SESSION_STATUS_ACTIVE,
                        cutoff_string
                    ],
                    |row| row.get::<_, String>(0),
                )?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            for id in &ids {
                bump_session_change(conn, id)?;
            }
            Ok(ids)
        })
    }
}

/// The row a created or forked session starts as.
pub(super) fn new_active_session_row(
    target_id: &str,
    agent_session_id: String,
    record: NewSessionRecord,
) -> SessionRecord {
    let now = current_timestamp();
    SessionRecord {
        id: record.id,
        target_id: target_id.to_owned(),
        agent_session_id,
        created_at: now.clone(),
        updated_at: now,
        status: SESSION_STATUS_ACTIVE.to_owned(),
        agent_id: record.agent_id,
        cwd: record.cwd,
        title: record.title,
        metadata_json: record.metadata_json,
        change_seq: 0,
        event_seq: 0,
    }
}

/// Insert `row` stamped with `change_seq`, a value the caller issued in the
/// same transaction.
pub(super) fn insert_session_row(
    conn: &rusqlite::Connection,
    row: &SessionRecord,
    change_seq: u64,
) -> Result<()> {
    conn.execute(
        r#"
        INSERT INTO sessions
            (id, target_id, agent_session_id, created_at, updated_at, status, agent_id, cwd, title, metadata_json, change_seq)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
        "#,
        params![
            row.id,
            row.target_id,
            row.agent_session_id,
            row.created_at,
            row.updated_at,
            row.status,
            row.agent_id,
            row.cwd,
            row.title,
            row.metadata_json,
            sequence_param(change_seq)?,
        ],
    )?;
    clear_session_tombstone(conn, &row.id)
}

/// The JSON object a `sessions.metadata_json` value holds.
fn metadata_object(metadata_json: &str) -> Result<serde_json::Map<String, serde_json::Value>> {
    serde_json::from_str::<serde_json::Value>(metadata_json)
        .map_err(|err| StackError::StateInvalidJson {
            field: "sessions.metadata_json",
            reason: err.to_string(),
        })?
        .as_object()
        .cloned()
        .ok_or_else(|| StackError::StateInvalidJson {
            field: "sessions.metadata_json",
            reason: "expected a JSON object".to_owned(),
        })
}

fn normalize_listed_session_timestamp(raw: &str) -> Result<String> {
    let parsed =
        chrono::DateTime::parse_from_rfc3339(raw).map_err(|err| StackError::InvalidParam {
            field: "updated_at",
            reason: format!("listed session timestamp is not valid RFC3339: {err}"),
        })?;
    Ok(parsed
        .with_timezone(&Utc)
        .to_rfc3339_opts(SecondsFormat::Nanos, true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::events::LATEST_EVENT_TIMESTAMP_SQL;

    fn query_plan(store: &StateStore, sql: &str, bindings: &[&dyn rusqlite::ToSql]) -> Vec<String> {
        let mut statement = store
            .connection()
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .expect("prepare plan");
        statement
            .query_map(bindings, |row| row.get::<_, String>(3))
            .expect("query plan")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("plan rows")
    }

    fn migrated_store(dir: &tempfile::TempDir) -> StateStore {
        let store = StateStore::open(dir.path().join("state.sqlite")).expect("open state");
        store.migrate().expect("migrate");
        store
    }

    #[test]
    fn the_status_window_seeks_events_and_prompts_per_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = migrated_store(&dir);
        for target_id in [None, Some("codex")] {
            let plan = query_plan(
                &store,
                SESSION_STATUS_WINDOW_SQL,
                rusqlite::params![
                    "2026-01-01T00:00:00.000000000Z",
                    SESSION_ACTIVITY_ACTOR_USER,
                    SESSION_ACTIVITY_ACTOR_AGENT,
                    EVENT_SOURCE_ACP,
                    target_id,
                    10_i64,
                    EVENT_KIND_SESSION_AVAILABLE,
                ],
            );
            for step in &plan {
                // A full scan prints as `SCAN <alias>`, with or without a `USING` suffix.
                let mut tokens = step.split(' ');
                if tokens.next() == Some("SCAN") {
                    assert!(
                        !matches!(tokens.next(), Some("e" | "latest" | "p")),
                        "full scan in {plan:#?}"
                    );
                }
                // Both indexes lead with columns that span every session, so using
                // either walks the whole window instead of one session's rows.
                for cross_session_index in ["idx_events_created_kind", "idx_events_kind_created"] {
                    assert!(
                        !step.contains(cross_session_index),
                        "cross-session walk in {plan:#?}"
                    );
                }
                // A session-only prefix reads that session's whole prompt history.
                assert!(
                    !step.ends_with("prompts_session_idx (session_id=?)"),
                    "unbounded per-session prompt read in {plan:#?}"
                );
            }
        }
    }

    #[test]
    fn the_latest_event_timestamp_reads_one_index_entry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = migrated_store(&dir);
        let plan = query_plan(&store, LATEST_EVENT_TIMESTAMP_SQL, &[]);
        assert!(
            plan.iter()
                .any(|step| step.contains("idx_events_created_kind"))
                && !plan.iter().any(|step| step.contains("TEMP B-TREE")),
            "latest event read sorts instead of seeking: {plan:#?}"
        );
        assert_eq!(store.latest_event_timestamp().expect("empty"), None);
        let event = store
            .append_event("info", "test.event", "test", "{}")
            .expect("append");
        assert_eq!(
            store.latest_event_timestamp().expect("latest"),
            Some(event.created_at)
        );
    }
}
