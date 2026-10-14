//! `StateStore` struct and opener: connection lifetime, default on-disk path,
//! and the `pub(super)` accessors the domain leaves query through.

use crate::error::{Result, StackError};
use crate::events::EventHub;
use crate::tracing_init::LATENCY_TARGET;
use rusqlite::{Connection, Transaction, TransactionBehavior};
use std::cell::{Cell, RefCell};
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

/// A state write or wait slower than this logs a warning on the latency target.
pub(super) const SLOW_STATE_TRANSACTION_THRESHOLD: Duration = Duration::from_millis(200);

/// Savepoint a write scope opens when its connection is already inside a transaction,
/// such as a writer batch. Scopes nest strictly, so one name serves every level.
const NESTED_WRITE_SAVEPOINT: &str = "acps_write";

type PendingPublish = Box<dyn FnOnce(&EventHub) + Send>;

pub struct StateStore {
    connection: Connection,
    path: PathBuf,
    /// Optional fan-out for every `append_event` write.
    event_hub: Option<EventHub>,
    /// When true, every persist call site enqueues into `sink_outbox` in the
    /// same transaction as the source write.
    external_logging_enabled: Cell<bool>,
    /// Change-feed epoch, minted per open rather than stored: a checkpoint
    /// restore brings back every stored value, so only a value from outside
    /// the database tells a feed reader its cursor may name reissued values.
    feed_epoch: String,
    /// Hub publishes of writes made inside an enclosing transaction, held until that
    /// transaction commits so a live subscriber never sees a row that rolls back.
    pending_publishes: RefCell<Vec<PendingPublish>>,
    /// Set while the `StateHandle` writer runs a batch, so a batch SQLite rolled back on
    /// its own is never mistaken for a connection with no transaction open.
    in_writer_batch: Cell<bool>,
}

pub fn default_state_path(home: &Path) -> PathBuf {
    home.join(".local")
        .join("share")
        .join("acp-stack")
        .join("state.sqlite")
}

impl StateStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_feed_epoch(path, next_feed_epoch())
    }

    /// Open with a caller-minted feed epoch, so every connection of one `StateHandle`
    /// reports the same epoch.
    pub(super) fn open_with_feed_epoch(path: impl AsRef<Path>, feed_epoch: String) -> Result<Self> {
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
            external_logging_enabled: Cell::new(false),
            feed_epoch,
            pending_publishes: RefCell::new(Vec::new()),
            in_writer_batch: Cell::new(false),
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
    pub fn set_external_logging_enabled(&self, enabled: bool) {
        self.external_logging_enabled.set(enabled);
    }

    pub(super) fn external_logging_enabled(&self) -> bool {
        self.external_logging_enabled.get()
    }

    pub(super) fn connection(&self) -> &Connection {
        &self.connection
    }

    /// The change-feed epoch every session-feed reader compares its cursor's
    /// epoch against.
    pub fn feed_epoch(&self) -> &str {
        &self.feed_epoch
    }

    pub(super) fn set_feed_epoch(&mut self, feed_epoch: String) {
        self.feed_epoch = feed_epoch;
    }

    /// Integration-test hook for concurrent SQLite tests that need a
    /// non-default busy timeout.
    pub fn set_busy_timeout_for_test(&self, timeout: Duration) -> Result<()> {
        self.connection.busy_timeout(timeout)?;
        Ok(())
    }

    /// Publish to the attached hub once the write is durable: now when no transaction
    /// is open, otherwise when the enclosing transaction commits.
    pub(super) fn publish(&self, publish: impl FnOnce(&EventHub) + Send + 'static) {
        let Some(hub) = &self.event_hub else {
            return;
        };
        if self.connection.is_autocommit() && !self.in_writer_batch.get() {
            publish(hub);
        } else {
            self.pending_publishes.borrow_mut().push(Box::new(publish));
        }
    }

    pub(super) fn set_in_writer_batch(&self, in_batch: bool) {
        self.in_writer_batch.set(in_batch);
    }

    pub(super) fn pending_publish_count(&self) -> usize {
        self.pending_publishes.borrow().len()
    }

    /// Drop the publishes queued after the first `keep`, for writes that rolled back.
    pub(super) fn discard_publishes_after(&self, keep: usize) {
        self.pending_publishes.borrow_mut().truncate(keep);
    }

    /// Run every queued publish, after the enclosing transaction committed.
    pub(super) fn flush_publishes(&self) {
        let pending = self.pending_publishes.take();
        if let Some(hub) = &self.event_hub {
            for publish in pending {
                publish(hub);
            }
        }
    }

    /// Open a write scope whose commit is timed. On a connection with no open transaction
    /// this is `BEGIN IMMEDIATE`; inside one, such as a writer batch, it is a savepoint that
    /// commits with the enclosing transaction. `scope` names the write in the slow-write
    /// log line.
    pub(super) fn begin_immediate(&self, scope: &'static str) -> Result<TimedTransaction<'_>> {
        let began_at = Instant::now();
        let nested = !self.connection.is_autocommit();
        if !nested && self.in_writer_batch.get() {
            return Err(StackError::StateAccessFailed {
                reason: "state write batch was rolled back by SQLite".to_owned(),
            });
        }
        if nested {
            self.connection
                .execute_batch(&format!("SAVEPOINT {NESTED_WRITE_SAVEPOINT}"))?;
        } else {
            self.connection.execute_batch("BEGIN IMMEDIATE")?;
        }
        Ok(TimedTransaction {
            connection: &self.connection,
            nested,
            finished: false,
            scope,
            began_at,
            lock_wait: began_at.elapsed(),
        })
    }

    /// Run `read` on one consistent snapshot: its own deferred transaction, or the
    /// enclosing one when a transaction is already open.
    pub(super) fn with_read_snapshot<R>(
        &self,
        read: impl FnOnce(&Connection) -> Result<R>,
    ) -> Result<R> {
        if !self.connection.is_autocommit() {
            return read(&self.connection);
        }
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Deferred)?;
        let value = read(&transaction)?;
        transaction.commit()?;
        Ok(value)
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
        if self.external_logging_enabled() {
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
        if self.external_logging_enabled() {
            for id in &ids {
                sink_outbox::enqueue(&tx, source_table, id, created_at)?;
            }
        }
        tx.commit()?;
        Ok(ids)
    }
}

/// A write scope that logs its timing when it runs slow. Dropping it without `commit`
/// rolls back the scope's own writes.
pub(super) struct TimedTransaction<'connection> {
    connection: &'connection Connection,
    /// A savepoint inside an enclosing transaction rather than a transaction of its own.
    nested: bool,
    finished: bool,
    scope: &'static str,
    began_at: Instant,
    /// Time `BEGIN IMMEDIATE` waited for SQLite's write lock.
    lock_wait: Duration,
}

impl std::ops::Deref for TimedTransaction<'_> {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        self.connection
    }
}

impl Drop for TimedTransaction<'_> {
    fn drop(&mut self) {
        // SQLite already ended a transaction that some errors abort on their own.
        if self.finished || self.connection.is_autocommit() {
            return;
        }
        let rollback = if self.nested {
            format!("ROLLBACK TO {NESTED_WRITE_SAVEPOINT}; RELEASE {NESTED_WRITE_SAVEPOINT}")
        } else {
            "ROLLBACK".to_owned()
        };
        if let Err(error) = self.connection.execute_batch(&rollback) {
            tracing::warn!(scope = self.scope, %error, "state write rollback failed");
        }
    }
}

impl TimedTransaction<'_> {
    pub(super) fn commit(mut self) -> Result<()> {
        let commit_started_at = Instant::now();
        let commit_sql = if self.nested {
            format!("RELEASE {NESTED_WRITE_SAVEPOINT}")
        } else {
            "COMMIT".to_owned()
        };
        self.connection.execute_batch(&commit_sql)?;
        self.finished = true;
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

pub(super) fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
