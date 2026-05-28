//! `StateStore` wiring: connection lifetime, default on-disk path, and the
//! `pub(super)` accessors that the domain leaves use to reach the underlying
//! `rusqlite::Connection`. Each domain table's persistence logic lives in the
//! sibling leaf (`sessions`, `events`, `commands`, ...); this file is just
//! the store struct + opener.

use crate::error::Result;
use crate::events::EventHub;
use rusqlite::{Connection, Transaction, TransactionBehavior};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::sink_outbox;

pub struct StateStore {
    connection: Connection,
    path: PathBuf,
    /// Optional fan-out for every `append_event` write. Set via
    /// `attach_event_hub` from `acps serve`; CLI tools that open the store
    /// read-only leave it `None`.
    event_hub: Option<EventHub>,
    /// When true, every persist call site enqueues into `sink_outbox` in the
    /// same transaction as the source write. Set by `acps serve` only when
    /// `[logging.supabase].enabled = true`; CLI tools and acpctl leave it
    /// off so they don't write an outbox row the daemon will then re-send.
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
        connection.execute_batch("PRAGMA foreign_keys = ON;")?;
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
    /// the `logs` topic. The daemon (`acps serve`) calls this once at startup;
    /// CLI tools that open the store for ad-hoc queries leave it unset.
    pub fn attach_event_hub(&mut self, hub: EventHub) {
        self.event_hub = Some(hub);
    }

    /// Enable transactional outbox writes alongside every persist call.
    /// Caller must already have validated `[logging.supabase].enabled` and
    /// the matching API key secret; this flag is the on/off switch the
    /// persist leaves consult.
    pub fn set_external_logging_enabled(&mut self, enabled: bool) {
        self.external_logging_enabled = enabled;
    }

    pub(super) fn external_logging_enabled(&self) -> bool {
        self.external_logging_enabled
    }

    /// `pub(super)` accessor so the domain leaves can issue queries against
    /// the shared connection without exposing the field publicly. Kept off
    /// the public API: external callers reach the store through the typed
    /// `query_*` / `append_*` methods, not the raw `Connection`.
    pub(super) fn connection(&self) -> &Connection {
        &self.connection
    }

    /// Narrow integration-test hook for concurrent SQLite tests that need a
    /// non-default busy timeout. Keep this typed instead of exposing the raw
    /// `Connection`, because rusqlite mutates through `&self`.
    pub fn set_busy_timeout_for_test(&self, timeout: Duration) -> Result<()> {
        self.connection.busy_timeout(timeout)?;
        Ok(())
    }

    pub(super) fn event_hub(&self) -> Option<&EventHub> {
        self.event_hub.as_ref()
    }

    /// Run a single persistence operation that writes one row to
    /// `source_table` and, when external logging is enabled, atomically
    /// enqueues an outbox row for delivery. When external logging is off
    /// the closure runs directly on the connection (the cheap path used by
    /// every non-Supabase deployment); otherwise the closure runs inside an
    /// IMMEDIATE transaction and the outbox enqueue happens before commit.
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
}
