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
            bump_session_change(conn, session_id)?;
            Ok(seq)
        })?;
        event.seq = Some(seq);

        if let Some(hub) = self.event_hub() {
            // Both publishes are required: dropping the second strands
            // session-scoped events on the logs topic only, which breaks
            // reconnect/live-tail flows.
            hub.publish_log_event(&event);
            hub.publish_session_update(session_id, &event, &event.payload_json);
        }

        Ok(event)
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
