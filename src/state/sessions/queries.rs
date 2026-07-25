//! Session row queries and mutations.

use super::*;

impl StateStore {
    /// Apply a partial ACP `session_info_update` to an existing local session.
    /// The outer `Option` distinguishes an omitted field from an explicit
    /// `null`; all unrelated metadata keys are preserved.
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
        let mut metadata = serde_json::from_str::<serde_json::Value>(&record.metadata_json)
            .map_err(|err| StackError::StateInvalidJson {
                field: "sessions.metadata_json",
                reason: err.to_string(),
            })?
            .as_object()
            .cloned()
            .ok_or_else(|| StackError::StateInvalidJson {
                field: "sessions.metadata_json",
                reason: "expected a JSON object".to_owned(),
            })?;

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
            Ok(())
        })
    }

    pub fn query_sessions(&self, filter: SessionFilter<'_>) -> Result<Vec<SessionRecord>> {
        let mut sql = String::from(
            "SELECT id, target_id, agent_session_id, created_at, updated_at, status, agent_id, cwd, title, metadata_json \
             FROM sessions WHERE 1=1",
        );
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
                r#"
                SELECT id, target_id, agent_session_id, created_at, updated_at, status, agent_id, cwd, title, metadata_json
                FROM sessions
                WHERE id = ?1
                "#,
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
                r#"
                SELECT id, target_id, agent_session_id, created_at, updated_at, status, agent_id, cwd, title, metadata_json
                FROM sessions
                WHERE target_id = ?1 AND agent_session_id = ?2
                "#,
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
        let mut statement = self.connection().prepare(
            r#"
            WITH scoped_sessions AS (
                SELECT id
                FROM sessions
                WHERE (?5 IS NULL OR target_id = ?5)
            ),
            activity AS (
                SELECT s.id AS session_id,
                       s.updated_at AS activity_at,
                       ?2 AS actor,
                       0 AS priority
                FROM sessions s
                JOIN scoped_sessions ss ON ss.id = s.id
                WHERE s.updated_at >= ?1
                UNION ALL
                SELECT p.session_id,
                       p.created_at AS activity_at,
                       ?2 AS actor,
                       1 AS priority
                FROM prompts p
                JOIN scoped_sessions ss ON ss.id = p.session_id
                WHERE p.created_at >= ?1
                UNION ALL
                SELECT p.session_id,
                       p.updated_at AS activity_at,
                       ?3 AS actor,
                       2 AS priority
                FROM prompts p
                JOIN scoped_sessions ss ON ss.id = p.session_id
                WHERE p.status <> 'pending'
                  AND p.updated_at >= ?1
                UNION ALL
                SELECT e.session_id,
                       e.created_at AS activity_at,
                       CASE WHEN e.source = ?4 THEN ?3 ELSE ?2 END AS actor,
                       3 AS priority
                FROM events e
                JOIN scoped_sessions ss ON ss.id = e.session_id
                WHERE e.session_id IS NOT NULL
                  AND e.created_at >= ?1
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
                       SELECT MIN(e.created_at)
                       FROM events e
                       WHERE e.session_id = s.id
                         AND e.kind = 'session.update'
                         AND e.source = ?4
                         AND lp.id IS NOT NULL
                         AND e.created_at >= lp.created_at
                   ) AS prompt_stream_started_at
            FROM sessions s
            JOIN window_sessions r ON r.session_id = s.id
            LEFT JOIN latest_prompts lp ON lp.session_id = s.id
            LEFT JOIN pending_acp_permissions pp ON pp.session_id = s.id
            ORDER BY r.activity_at DESC, s.id DESC
            LIMIT ?6
            "#,
        )?;
        let rows = statement.query_map(
            params![
                since,
                SESSION_ACTIVITY_ACTOR_USER,
                SESSION_ACTIVITY_ACTOR_AGENT,
                EVENT_SOURCE_ACP,
                target_id,
                i64::from(limit),
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
        let now = current_timestamp();
        let row = SessionRecord {
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
        };
        self.persist_with_outbox("sessions", &row.id, &row.created_at, |conn| {
            conn.execute(
                r#"
                INSERT INTO sessions
                    (id, target_id, agent_session_id, created_at, updated_at, status, agent_id, cwd, title, metadata_json)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
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
                ],
            )?;
            Ok(())
        })?;
        Ok(row)
    }

    /// Convenience wrapper that derives each record's target from its
    /// `agent_id` and upserts one record at a time. Unlike a primary-key upsert
    /// this dedups on `(target_id, agent_session_id)` — the agent's session id
    /// is the stable external identity, not the internal row `id`. Used by
    /// tests and any caller that has no explicit per-target grouping; the
    /// daemon sync path calls `upsert_listed_sessions_for_target` directly.
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

    pub fn upsert_listed_sessions_for_target(
        &self,
        target_id: &str,
        records: Vec<ListedSessionRecord>,
    ) -> Result<ListedSessionUpsertCounts> {
        let mut counts = ListedSessionUpsertCounts::default();
        for record in records {
            let existing =
                self.get_session_by_target_agent_session_id(target_id, &record.agent_session_id)?;
            validate_json_payload(self.connection(), &record.metadata_json)?;
            let updated_at = record
                .updated_at
                .as_deref()
                .map(normalize_listed_session_timestamp)
                .transpose()?
                .unwrap_or_else(current_timestamp);
            match existing {
                Some(existing) => {
                    self.persist_with_outbox("sessions", &existing.id, &updated_at, |conn| {
                        conn.execute(
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
                                metadata_json = ?8,
                                target_id = ?9,
                                agent_session_id = ?10
                            WHERE id = ?11
                            "#,
                            params![
                                updated_at,
                                SESSION_STATUS_ACTIVE,
                                SESSION_STATUS_CLOSED,
                                SESSION_STATUS_AVAILABLE,
                                record.agent_id,
                                record.cwd,
                                record.title,
                                record.metadata_json,
                                target_id,
                                record.agent_session_id,
                                existing.id,
                            ],
                        )?;
                        Ok(())
                    })?;
                    counts.updated += 1;
                }
                None => {
                    let created_at = current_timestamp();
                    let id = record.id;
                    self.persist_with_outbox("sessions", &id, &updated_at, |conn| {
                        conn.execute(
                            r#"
                            INSERT INTO sessions
                                (id, target_id, agent_session_id, created_at, updated_at, status, agent_id, cwd, title, metadata_json)
                            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                            "#,
                            params![
                                id,
                                target_id,
                                record.agent_session_id,
                                created_at,
                                updated_at,
                                SESSION_STATUS_AVAILABLE,
                                record.agent_id,
                                record.cwd,
                                record.title,
                                record.metadata_json,
                            ],
                        )?;
                        Ok(())
                    })?;
                    counts.upserted += 1;
                }
            }
        }
        Ok(counts)
    }

    pub fn rename_session_target_id(
        &self,
        old_target_id: &str,
        new_target_id: &str,
    ) -> Result<usize> {
        if old_target_id == new_target_id {
            return Ok(0);
        }
        // The UNIQUE(target_id, agent_session_id) index would reject moving a
        // row whose agent_session_id already exists under new_target_id. Detect
        // it up front and fail with a subsystem-identifying error instead of
        // surfacing a raw SQLite UNIQUE violation partway through the move.
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
            Ok(ids)
        })?;
        Ok(ids.len())
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
            Ok(())
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
            Ok(())
        })
    }
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
