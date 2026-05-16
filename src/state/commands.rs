//! `commands` table persistence + per-command output chunks.

use crate::error::{Result, StackError};
use rusqlite::{OptionalExtension, params};

use super::core::StateStore;
use super::events::{EVENT_SOURCE_COMMAND, Event};
use super::ids::{current_timestamp, next_command_id};
use super::records::CommandFilter;
use super::rows::validate_json_payload;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRecord {
    pub id: String,
    pub created_at: String,
    pub updated_at: String,
    pub status: String,
    pub command: String,
    pub exit_status: Option<i64>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub cwd: Option<String>,
    pub env_json: Option<String>,
    pub duration_ms: Option<i64>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewCommandRecord<'a> {
    pub command: &'a str,
    pub cwd: Option<&'a str>,
    pub env_json: Option<&'a str>,
}

/// Lifecycle status of a `commands` row. The string form goes to SQLite and
/// out over the API; `CommandStatus::as_str` is the single source of truth so
/// the gateway and tests do not drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandStatus {
    Pending,
    Running,
    Exited,
    Failed,
    Canceled,
}

impl CommandStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            CommandStatus::Pending => "pending",
            CommandStatus::Running => "running",
            CommandStatus::Exited => "exited",
            CommandStatus::Failed => "failed",
            CommandStatus::Canceled => "canceled",
        }
    }
}

pub(super) fn row_to_command(row: &rusqlite::Row<'_>) -> rusqlite::Result<CommandRecord> {
    let truncated: i64 = row.get(11)?;
    Ok(CommandRecord {
        id: row.get(0)?,
        created_at: row.get(1)?,
        updated_at: row.get(2)?,
        status: row.get(3)?,
        command: row.get(4)?,
        exit_status: row.get(5)?,
        started_at: row.get(6)?,
        finished_at: row.get(7)?,
        cwd: row.get(8)?,
        env_json: row.get(9)?,
        duration_ms: row.get(10)?,
        truncated: truncated != 0,
    })
}

impl StateStore {
    pub fn query_commands(&self, filter: CommandFilter<'_>) -> Result<Vec<CommandRecord>> {
        let mut sql = String::from(
            "SELECT id, created_at, updated_at, status, command, exit_status, \
                    started_at, finished_at, cwd, env_json, duration_ms, truncated \
             FROM commands WHERE 1=1",
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
        if let Some(after) = filter.after_id {
            sql.push_str(
                " AND (updated_at, id) < (SELECT updated_at, id FROM commands WHERE id = ?)",
            );
            bindings.push(rusqlite::types::Value::Text(after.to_owned()));
        }
        sql.push_str(" ORDER BY updated_at DESC, id DESC LIMIT ?");
        bindings.push(rusqlite::types::Value::Integer(i64::from(filter.limit)));
        let mut statement = self.connection().prepare(&sql)?;
        let rows =
            statement.query_map(rusqlite::params_from_iter(bindings.iter()), row_to_command)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn get_command(&self, id: &str) -> Result<Option<CommandRecord>> {
        Ok(self
            .connection()
            .query_row(
                r#"
                SELECT id, created_at, updated_at, status, command, exit_status,
                       started_at, finished_at, cwd, env_json, duration_ms, truncated
                FROM commands
                WHERE id = ?1
                "#,
                params![id],
                row_to_command,
            )
            .optional()?)
    }

    /// Insert a `commands` row in the `pending` state. The gateway transitions
    /// it to `running` via `start_command` once the subprocess has been
    /// spawned, so an inserted row that never starts (e.g. a crash between
    /// INSERT and spawn) is recoverable from history.
    pub fn append_command(&self, input: NewCommandRecord<'_>) -> Result<CommandRecord> {
        if let Some(payload) = input.env_json {
            validate_json_payload(self.connection(), payload)?;
        }
        let now = current_timestamp();
        let record = CommandRecord {
            id: next_command_id(),
            created_at: now.clone(),
            updated_at: now,
            status: CommandStatus::Pending.as_str().to_owned(),
            command: input.command.to_owned(),
            exit_status: None,
            started_at: None,
            finished_at: None,
            cwd: input.cwd.map(str::to_owned),
            env_json: input.env_json.map(str::to_owned),
            duration_ms: None,
            truncated: false,
        };

        self.persist_with_outbox("commands", &record.id, &record.created_at, |conn| {
            conn.execute(
                r#"
                INSERT INTO commands
                    (id, created_at, updated_at, status, command, exit_status,
                     started_at, finished_at, cwd, env_json, duration_ms, truncated)
                VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL, NULL, ?6, ?7, NULL, 0)
                "#,
                params![
                    record.id,
                    record.created_at,
                    record.updated_at,
                    record.status,
                    record.command,
                    record.cwd,
                    record.env_json,
                ],
            )?;
            Ok(())
        })?;

        Ok(record)
    }

    /// Move a command from `pending` to `running` and stamp `started_at`. The
    /// caller is responsible for ensuring the subprocess has actually been
    /// spawned; this only records the transition.
    pub fn start_command(&self, id: &str) -> Result<()> {
        let now = current_timestamp();
        self.persist_with_outbox("commands", id, &now, |conn| {
            let rows_affected = conn.execute(
                r#"
                UPDATE commands
                SET status = ?1, started_at = ?2, updated_at = ?2
                WHERE id = ?3
                "#,
                params![CommandStatus::Running.as_str(), now, id],
            )?;
            if rows_affected == 0 {
                return Err(StackError::CommandNotFound { id: id.to_owned() });
            }
            Ok(())
        })
    }

    /// Record the terminal state of a command run. `status` should be one of
    /// the non-pending/non-running variants of `CommandStatus`; the caller
    /// supplies the resolved exit status (or `None` if killed by signal).
    pub fn finish_command(
        &self,
        id: &str,
        status: CommandStatus,
        exit_status: Option<i32>,
        duration_ms: Option<i64>,
    ) -> Result<()> {
        let now = current_timestamp();
        self.persist_with_outbox("commands", id, &now, |conn| {
            let rows_affected = conn.execute(
                r#"
                UPDATE commands
                SET status = ?1,
                    exit_status = ?2,
                    finished_at = ?3,
                    updated_at = ?3,
                    duration_ms = ?4
                WHERE id = ?5
                "#,
                params![status.as_str(), exit_status, now, duration_ms, id],
            )?;
            if rows_affected == 0 {
                return Err(StackError::CommandNotFound { id: id.to_owned() });
            }
            Ok(())
        })
    }

    /// Flip the `truncated` flag on a command row. Idempotent; called when the
    /// gateway hits its per-command output cap.
    pub fn mark_command_truncated(&self, id: &str) -> Result<()> {
        let now = current_timestamp();
        self.persist_with_outbox("commands", id, &now, |conn| {
            let rows_affected = conn.execute(
                "UPDATE commands SET truncated = 1, updated_at = ?1 WHERE id = ?2",
                params![now, id],
            )?;
            if rows_affected == 0 {
                return Err(StackError::CommandNotFound { id: id.to_owned() });
            }
            Ok(())
        })
    }

    /// Append a single stdout/stderr chunk as a durable event. The `events`
    /// row carries the bytes; `commands.{id}` WebSocket subscribers receive
    /// the same payload via `EventHub::publish_command_event`. `seq` lets
    /// consumers reassemble interleaved streams in original write order.
    pub fn append_command_output(
        &self,
        command_id: &str,
        stream: &str,
        seq: u64,
        chunk: &str,
    ) -> Result<Event> {
        let payload = serde_json::json!({
            "command_id": command_id,
            "stream": stream,
            "seq": seq,
            "data": chunk,
        });
        let payload_json =
            serde_json::to_string(&payload).map_err(|_| StackError::InvalidEventPayload)?;
        let kind = format!("command.{stream}");
        self.append_event_with_source("info", &kind, EVENT_SOURCE_COMMAND, "", &payload_json)
    }

    /// Same idea for `commands` as `reconcile_orphaned_prompts`: a daemon
    /// restart kills any subprocesses (`kill_on_drop` plus tokio runtime
    /// teardown), but the SQLite rows are not finalized in that path. Without
    /// this sweep, every `running` / `pending` row from the previous run is
    /// permanently stuck and a CLI/HTTP poll would never see them settle.
    /// Returns the number of rows transitioned to `failed`.
    pub fn reconcile_orphaned_commands(&self, reason: &str) -> Result<usize> {
        let now = current_timestamp();
        let _ = reason; // recorded via finished_at + a synthetic event below
        if !self.external_logging_enabled() {
            let affected = self.connection().execute(
                r#"
                UPDATE commands
                SET status = 'failed',
                    updated_at = ?1,
                    finished_at = COALESCE(finished_at, ?1)
                WHERE status IN ('pending', 'running')
                "#,
                params![now],
            )?;
            return Ok(affected);
        }
        let tx = rusqlite::Transaction::new_unchecked(
            self.connection(),
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let ids: Vec<String> = {
            let mut statement =
                tx.prepare("SELECT id FROM commands WHERE status IN ('pending', 'running')")?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let affected = tx.execute(
            r#"
            UPDATE commands
            SET status = 'failed',
                updated_at = ?1,
                finished_at = COALESCE(finished_at, ?1)
            WHERE status IN ('pending', 'running')
            "#,
            params![now],
        )?;
        for id in &ids {
            super::sink_outbox::enqueue(&tx, "commands", id, &now)?;
        }
        tx.commit()?;
        Ok(affected)
    }
}
