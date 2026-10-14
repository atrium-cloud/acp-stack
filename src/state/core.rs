//! `StateStore` struct and opener: connection lifetime, default on-disk path,
//! and the `pub(super)` accessors the domain leaves query through.

use crate::error::Result;
use crate::events::EventHub;
use crate::tracing_init::LATENCY_TARGET;
use rusqlite::{Connection, Transaction, TransactionBehavior};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::ids::next_feed_epoch;
use super::sink_outbox;

/// Busy-timeout applied to every state connection so a contended writer waits
/// for the lock instead of failing immediately with `SQLITE_BUSY`.
const STATE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Pragmas every state connection runs at open. `synchronous = NORMAL` under WAL
/// stays corruption-safe and loses no commit on a process crash; only a power loss
/// or OS crash can drop the newest commits, and commits stop paying an fsync each.
const STATE_CONNECTION_PRAGMAS: &str =
    "PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = ON;";

/// A write transaction slower than this, from `BEGIN IMMEDIATE` to `COMMIT`, logs a
/// warning on the latency target.
const SLOW_STATE_TRANSACTION_THRESHOLD: Duration = Duration::from_millis(200);

pub struct StateStore {
    connection: Connection,
    path: PathBuf,
    /// Optional fan-out for every `append_event` write.
    event_hub: Option<EventHub>,
    /// When true, every persist call site enqueues into `sink_outbox` in the
    /// same transaction as the source write.
    external_logging_enabled: bool,
    /// Change-feed epoch, minted per open rather than stored: a checkpoint
    /// restore brings back every stored value, so only a value from outside
    /// the database tells a feed reader its cursor may name reissued values.
    feed_epoch: String,
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
        connection.execute_batch(STATE_CONNECTION_PRAGMAS)?;
        Ok(Self {
            connection,
            path,
            event_hub: None,
            external_logging_enabled: false,
            feed_epoch: next_feed_epoch(),
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

    /// The change-feed epoch every session-feed reader compares its cursor's
    /// epoch against.
    pub fn feed_epoch(&self) -> &str {
        &self.feed_epoch
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

    /// Open an IMMEDIATE write transaction whose commit is timed. `scope` names
    /// the write in the slow-transaction log line.
    pub(super) fn begin_immediate(&self, scope: &'static str) -> Result<TimedTransaction<'_>> {
        let began_at = Instant::now();
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        Ok(TimedTransaction {
            transaction,
            scope,
            began_at,
            lock_wait: began_at.elapsed(),
        })
    }

    /// Write one row to `source_table` in one IMMEDIATE transaction, together
    /// with an outbox row when external logging is enabled. The transaction is
    /// unconditional because `inner` may read a sequence and write it back
    /// (event `seq`, session `change_seq`), which is only correct under the
    /// write lock.
    pub(super) fn persist_with_outbox<F, R>(
        &self,
        source_table: &'static str,
        source_id: &str,
        created_at: &str,
        inner: F,
    ) -> Result<R>
    where
        F: FnOnce(&Connection) -> Result<R>,
    {
        let tx = self.begin_immediate(source_table)?;
        let value = inner(&tx)?;
        if self.external_logging_enabled {
            sink_outbox::enqueue(&tx, source_table, source_id, created_at)?;
        }
        tx.commit()?;
        Ok(value)
    }

    /// Like `persist_with_outbox`, but atomic across several rows: `inner`
    /// returns the source ids it changed and one outbox row is enqueued per id.
    pub(super) fn persist_many_with_outbox<F>(
        &self,
        source_table: &'static str,
        created_at: &str,
        inner: F,
    ) -> Result<Vec<String>>
    where
        F: FnOnce(&Connection) -> Result<Vec<String>>,
    {
        let tx = self.begin_immediate(source_table)?;
        let ids = inner(&tx)?;
        if self.external_logging_enabled {
            for id in &ids {
                sink_outbox::enqueue(&tx, source_table, id, created_at)?;
            }
        }
        tx.commit()?;
        Ok(ids)
    }
}

/// An IMMEDIATE transaction that logs its timing when it runs slow. Dropping it
/// without `commit` rolls back, as a plain `Transaction` does.
pub(super) struct TimedTransaction<'connection> {
    transaction: Transaction<'connection>,
    scope: &'static str,
    began_at: Instant,
    /// Time `BEGIN IMMEDIATE` waited for SQLite's write lock.
    lock_wait: Duration,
}

impl<'connection> std::ops::Deref for TimedTransaction<'connection> {
    type Target = Transaction<'connection>;

    fn deref(&self) -> &Self::Target {
        &self.transaction
    }
}

impl TimedTransaction<'_> {
    pub(super) fn commit(self) -> Result<()> {
        let commit_started_at = Instant::now();
        self.transaction.commit()?;
        let commit = commit_started_at.elapsed();
        let total = self.began_at.elapsed();
        if total >= SLOW_STATE_TRANSACTION_THRESHOLD {
            tracing::warn!(
                target: LATENCY_TARGET,
                scope = self.scope,
                transaction_ms = duration_millis(total),
                lock_wait_ms = duration_millis(self.lock_wait),
                commit_ms = duration_millis(commit),
                "slow state transaction"
            );
        }
        Ok(())
    }
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
