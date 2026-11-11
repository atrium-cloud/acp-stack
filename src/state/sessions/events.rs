//! Session-scoped event persistence and queries.

use super::*;

/// Where a forward page of a session's events starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEventCursor {
    /// After the event with this id, which must belong to the session.
    EventId(String),
    /// After this position in the session's log; `0` reads from the start.
    Seq(u64),
}

/// The `seq` the next event appended to `session_id` takes. Callers run it
/// under the write lock of the transaction that inserts the row, which is
/// what keeps a session's sequence contiguous across processes; the unique
/// `(session_id, seq)` index backs that up.
pub(super) fn next_session_event_seq(conn: &rusqlite::Connection, session_id: &str) -> Result<u64> {
    Ok(conn.query_row(
        "SELECT COALESCE(MAX(seq), 0) + 1 FROM events WHERE session_id = ?1",
        params![session_id],
        |row| sequence_column(row, 0),
    )?)
}

/// `message` of every agent-streamed `session.update` row.
const SESSION_UPDATE_EVENT_MESSAGE: &str = "ACP session update";

/// A dotted lowercase name such as `thekit.schedule`: at least two segments of
/// ASCII lowercase letters, digits, and underscores.
fn validate_annotation_kind(kind: &str) -> Result<()> {
    let well_formed = kind.len() <= MAX_SESSION_ANNOTATION_KIND_BYTES
        && kind.split('.').count() >= MIN_SESSION_ANNOTATION_KIND_SEGMENTS
        && kind.split('.').all(|segment| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        });
    if well_formed {
        return Ok(());
    }
    Err(StackError::InvalidParam {
        field: "kind",
        reason: format!(
            "kind must be {MIN_SESSION_ANNOTATION_KIND_SEGMENTS} or more dotted segments of lowercase letters, digits, and underscores, at most {MAX_SESSION_ANNOTATION_KIND_BYTES} bytes"
        ),
    })
}

/// An agent-streamed row for `session_id`'s log, not yet written.
pub(super) fn session_update_event(
    session_id: &str,
    kind: &str,
    payload_json: &str,
) -> Result<Event> {
    serde_json::from_str::<serde::de::IgnoredAny>(payload_json)
        .map_err(|_| StackError::InvalidEventPayload)?;
    Ok(Event {
        id: next_event_id(),
        created_at: current_timestamp(),
        level: "info".to_owned(),
        kind: kind.to_owned(),
        message: SESSION_UPDATE_EVENT_MESSAGE.to_owned(),
        payload_json: payload_json.to_owned(),
        source: EVENT_SOURCE_ACP.to_owned(),
        session_id: Some(session_id.to_owned()),
        seq: None,
    })
}

/// Insert `event` as the next row of `session_id`'s log and return its `seq`. The caller owns
/// the transaction, the change-feed bump, the outbox row, and the publish after commit.
pub(super) fn insert_session_event_row(
    conn: &rusqlite::Connection,
    session_id: &str,
    event: &Event,
) -> Result<u64> {
    let seq = next_session_event_seq(conn, session_id)?;
    conn.execute(
        r#"
        INSERT INTO events (id, created_at, level, kind, message, payload_json, source, session_id, seq)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
        "#,
        params![
            event.id,
            event.created_at,
            event.level,
            event.kind,
            event.message,
            event.payload_json,
            event.source,
            session_id,
            sequence_param(seq)?,
        ],
    )?;
    Ok(seq)
}

impl StateStore {
    /// Append a session-scoped event with the default `system` source.
    pub fn append_session_event(
        &self,
        session_id: &str,
        level: &str,
        kind: &str,
        message: &str,
        payload_json: &str,
    ) -> Result<Event> {
        self.append_session_event_with_source(
            session_id,
            level,
            kind,
            EVENT_SOURCE_SYSTEM,
            message,
            payload_json,
        )
    }

    pub fn append_session_event_with_source(
        &self,
        session_id: &str,
        level: &str,
        kind: &str,
        source: &str,
        message: &str,
        payload_json: &str,
    ) -> Result<Event> {
        validate_json_payload(self.connection(), payload_json)?;
        let mut event = Event {
            id: next_event_id(),
            created_at: current_timestamp(),
            level: level.to_owned(),
            kind: kind.to_owned(),
            message: message.to_owned(),
            payload_json: payload_json.to_owned(),
            source: source.to_owned(),
            session_id: Some(session_id.to_owned()),
            seq: None,
        };

        let seq = self.persist_with_outbox("events", &event.id, &event.created_at, |conn| {
            let seq = insert_session_event_row(conn, session_id, &event)?;
            bump_session_change(conn, session_id)?;
            Ok(seq)
        })?;
        event.seq = Some(seq);
        self.publish_session_event(session_id, &event);
        Ok(event)
    }

    /// Append a `session.annotation` row, `{ "kind", "data" }`, to the one session whose
    /// agent session id is `agent_session_id` and whose own turn is in flight: the turn a
    /// tool the agent ran reports into. The caller chooses the payload, never the row's kind.
    pub fn append_session_annotation(
        &self,
        agent_session_id: &str,
        kind: &str,
        data: serde_json::Value,
    ) -> Result<Event> {
        validate_annotation_kind(kind)?;
        if !data.is_object() {
            return Err(StackError::InvalidParam {
                field: "data",
                reason: "data must be a JSON object".to_owned(),
            });
        }
        let data_bytes = data.to_string().len();
        if data_bytes > MAX_SESSION_ANNOTATION_DATA_BYTES {
            return Err(StackError::InvalidParam {
                field: "data",
                reason: format!(
                    "data is {data_bytes} bytes serialized, over the {MAX_SESSION_ANNOTATION_DATA_BYTES}-byte cap"
                ),
            });
        }
        let session_id = self.annotated_session_id(agent_session_id)?;
        let payload = serde_json::json!({ "kind": kind, "data": data }).to_string();
        self.append_session_event_with_source(
            &session_id,
            "info",
            EVENT_KIND_SESSION_ANNOTATION,
            EVENT_SOURCE_API,
            SESSION_ANNOTATION_EVENT_MESSAGE,
            &payload,
        )
    }

    /// The session an annotation lands on. Uniqueness of an agent session id holds per
    /// target only, so a match on two targets with a turn in flight each is refused.
    fn annotated_session_id(&self, agent_session_id: &str) -> Result<String> {
        let mut statement = self.connection().prepare(
            r#"
            SELECT id,
                   EXISTS (
                       SELECT 1 FROM prompts
                       WHERE prompts.session_id = sessions.id
                         AND prompts.status IN ('pending', 'running')
                   )
            FROM sessions
            WHERE agent_session_id = ?1
            "#,
        )?;
        let rows = statement.query_map(params![agent_session_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?))
        })?;
        let matched = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        if matched.is_empty() {
            return Err(StackError::SessionNotFound {
                id: agent_session_id.to_owned(),
            });
        }
        let in_flight: Vec<String> = matched
            .into_iter()
            .filter_map(|(session_id, turn_in_flight)| turn_in_flight.then_some(session_id))
            .collect();
        match in_flight.as_slice() {
            [] => Err(StackError::SessionNoTurnInFlight {
                agent_session_id: agent_session_id.to_owned(),
            }),
            [session_id] => Ok(session_id.clone()),
            _ => Err(StackError::SessionAgentSessionAmbiguous {
                agent_session_id: agent_session_id.to_owned(),
                count: in_flight.len(),
            }),
        }
    }

    /// Append one agent-streamed ACP `session/update` row and touch the in-flight prompt of
    /// the session's tree in one transaction, so a streamed chunk costs one commit.
    pub fn append_session_update(
        &self,
        session_id: &str,
        kind: &str,
        payload_json: &str,
    ) -> Result<Event> {
        let mut event = session_update_event(session_id, kind, payload_json)?;
        let tx = self.begin_immediate("events")?;
        event.seq = Some(self.insert_session_update_row(&tx, session_id, &event)?);
        tx.commit()?;
        self.publish_session_event(session_id, &event);
        Ok(event)
    }

    /// Write `event` as the next row of `session_id`'s log inside the caller's transaction and
    /// touch an in-flight prompt, which keeps the stale-prompt sweeper off a turn that is still
    /// streaming. ACP updates carry no prompt id, so the touch lands on the oldest in-flight
    /// prompt of the tree's root: a subagent's traffic is work of the root's turn.
    pub(super) fn insert_session_update_row(
        &self,
        conn: &rusqlite::Connection,
        session_id: &str,
        event: &Event,
    ) -> Result<u64> {
        let seq = insert_session_event_row(conn, session_id, event)?;
        bump_session_change(conn, session_id)?;
        // Only `updated_at` moves: an in-flight prompt has no stop reason or error to keep.
        let touched_prompt_id: Option<String> = conn
            .query_row(
                r#"
                UPDATE prompts
                SET updated_at = ?1
                WHERE id = (
                    SELECT id FROM prompts
                    WHERE session_id = COALESCE(
                            (SELECT root_session_id FROM session_subagents WHERE session_id = ?2),
                            ?2
                        )
                      AND status IN ('pending', 'running')
                    ORDER BY created_at ASC, id ASC
                    LIMIT 1
                )
                RETURNING id
                "#,
                params![event.created_at, session_id],
                |row| row.get(0),
            )
            .optional()?;
        if self.external_logging_enabled() {
            sink_outbox::enqueue(conn, "events", &event.id, &event.created_at)?;
            if let Some(prompt_id) = &touched_prompt_id {
                sink_outbox::enqueue(conn, "prompts", prompt_id, &event.created_at)?;
            }
        }
        Ok(seq)
    }

    pub(super) fn publish_session_event(&self, session_id: &str, event: &Event) {
        let session_id = session_id.to_owned();
        let event = event.clone();
        self.publish(move |hub| {
            // Both publishes are required: dropping the second strands
            // session-scoped events on the logs topic only, which breaks
            // reconnect/live-tail flows.
            hub.publish_log_event(&event);
            hub.publish_session_update(&session_id, &event, &event.payload_json);
        });
    }

    /// Forward page of session events in log (`seq`) order.
    ///
    /// A cursor that does not resolve to a position in this session's log is
    /// an error rather than an empty page: after a checkpoint restore rolls
    /// the database back, an empty page would read as "you are at the head"
    /// and a client would treat a stale prefix as complete.
    pub fn query_session_events(
        &self,
        session_id: &str,
        after: Option<&SessionEventCursor>,
        limit: u32,
    ) -> Result<Vec<Event>> {
        let after_seq = match after {
            None => 0,
            Some(SessionEventCursor::EventId(event_id)) => self
                .session_event_seq(session_id, event_id)?
                .ok_or_else(|| StackError::SessionEventCursorUnknown {
                    session_id: session_id.to_owned(),
                    cursor_id: event_id.clone(),
                })?,
            Some(SessionEventCursor::Seq(seq)) => {
                if *seq > self.session_event_head(session_id)? {
                    return Err(StackError::SessionEventCursorUnknown {
                        session_id: session_id.to_owned(),
                        cursor_id: seq.to_string(),
                    });
                }
                *seq
            }
        };
        let mut statement = self.connection().prepare(&format!(
            r#"
            SELECT {EVENT_COLUMNS}
            FROM events
            WHERE session_id = ?1 AND seq > ?2
            ORDER BY seq ASC
            LIMIT ?3
            "#
        ))?;
        let rows = statement.query_map(
            params![session_id, sequence_param(after_seq)?, i64::from(limit)],
            row_to_event,
        )?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn session_event_seq(&self, session_id: &str, event_id: &str) -> Result<Option<u64>> {
        Ok(self
            .connection()
            .query_row(
                "SELECT seq FROM events WHERE id = ?1 AND session_id = ?2 AND seq IS NOT NULL",
                params![event_id, session_id],
                |row| sequence_column(row, 0),
            )
            .optional()?)
    }

    /// The `seq` of the session's newest event, `0` when it has none.
    pub fn session_event_head(&self, session_id: &str) -> Result<u64> {
        Ok(self.connection().query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM events WHERE session_id = ?1",
            params![session_id],
            |row| sequence_column(row, 0),
        )?)
    }

    /// Newest-first window of session-scoped events, so a reconnecting client
    /// gets the most-recent slice without paging from the start of the table.
    pub fn latest_session_events(&self, session_id: &str, limit: u32) -> Result<Vec<Event>> {
        let mut statement = self.connection().prepare(&format!(
            r#"
            SELECT {EVENT_COLUMNS}
            FROM events
            WHERE session_id = ?1
            ORDER BY seq DESC
            LIMIT ?2
            "#
        ))?;
        let rows = statement.query_map(params![session_id, i64::from(limit)], row_to_event)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}
