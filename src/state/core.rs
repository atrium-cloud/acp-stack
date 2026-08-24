//! `StateStore` struct and opener: connection lifetime, default on-disk path,
//! and the `pub(super)` accessors the domain leaves query through.

use crate::error::Result;
use crate::events::EventHub;
use rusqlite::{Connection, Transaction, TransactionBehavior};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::sink_outbox;

/// Busy-timeout applied to every state connection so a contended writer waits
/// for the lock instead of failing immediately with `SQLITE_BUSY`.
const STATE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

pub struct StateStore {
    connection: Connection,
    path: PathBuf,
    /// Optional fan-out for every `append_event` write.
    event_hub: Option<EventHub>,
    /// When true, every persist call site enqueues into `sink_outbox` in the
    /// same transaction as the source write.
    external_logging_enabled: bool,
}

pub fn default_state_path(home: &Path) -> PathBuf {
    home.join(".local")
        .join("share")
        .join("acp-stack")
        .join("state.sqlite")
}

impl StateStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let connection = Connection::open(&path)?;
        // Set the busy timeout before switching journal modes so the WAL
        // transition itself can wait for any concurrent connection.
        connection.busy_timeout(STATE_BUSY_TIMEOUT)?;
        connection.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")?;
        Ok(Self {
            connection,
            path,
            event_hub: None,
            external_logging_enabled: false,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Attach a live `EventHub` so every `append_event` write also fans out on
    /// the `logs` topic.
    pub fn attach_event_hub(&mut self, hub: EventHub) {
        self.event_hub = Some(hub);
    }

    /// Enable transactional outbox writes alongside every persist call.
    pub fn set_external_logging_enabled(&mut self, enabled: bool) {
        self.external_logging_enabled = enabled;
    }

    pub(super) fn external_logging_enabled(&self) -> bool {
        self.external_logging_enabled
    }

    pub(super) fn connection(&self) -> &Connection {
        &self.connection
    }

    /// Integration-test hook for concurrent SQLite tests that need a
    /// non-default busy timeout.
    pub fn set_busy_timeout_for_test(&self, timeout: Duration) -> Result<()> {
        self.connection.busy_timeout(timeout)?;
        Ok(())
    }

    pub(super) fn event_hub(&self) -> Option<&EventHub> {
        self.event_hub.as_ref()
    }

    /// Write one row to `source_table`, atomically enqueueing an outbox row
    /// when external logging is enabled.
    pub(super) fn persist_with_outbox<F, R>(
        &self,
        source_table: &str,
        source_id: &str,
        created_at: &str,
        inner: F,
    ) -> Result<R>
    where
        F: FnOnce(&Connection) -> Result<R>,
    {
        if !self.external_logging_enabled {
            return inner(&self.connection);
        }
        let tx = Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let value = inner(&tx)?;
        sink_outbox::enqueue(&tx, source_table, source_id, created_at)?;
        tx.commit()?;
        Ok(value)
    }

    /// Like `persist_with_outbox`, but atomic across several rows: `inner`
    /// returns the source ids it changed and one outbox row is enqueued per id.
    pub(super) fn persist_many_with_outbox<F>(
        &self,
        source_table: &str,
        created_at: &str,
        inner: F,
    ) -> Result<Vec<String>>
    where
        F: FnOnce(&Connection) -> Result<Vec<String>>,
    {
        if !self.external_logging_enabled {
            return inner(&self.connection);
        }
        let tx = Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let ids = inner(&tx)?;
        for id in &ids {
            sink_outbox::enqueue(&tx, source_table, id, created_at)?;
        }
        tx.commit()?;
        Ok(ids)
    }
}
