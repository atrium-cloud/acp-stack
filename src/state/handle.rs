//! `StateHandle`: the daemon's shared access to the state database. One writer thread
//! owns the write connection and group-commits queued write jobs, each under its own
//! savepoint; reads run on a small pool of read-only connections on the blocking pool,
//! so a read never waits on a commit.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rusqlite::Connection;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{Semaphore, mpsc, oneshot};

use crate::error::{Result, StackError, report};
use crate::tracing_init::LATENCY_TARGET;

use super::core::{SLOW_STATE_TRANSACTION_THRESHOLD, StateStore, duration_millis};
use super::ids::next_feed_epoch;

/// Write jobs that may wait for the writer before `write` callers wait to enqueue and
/// detached writes start dropping.
const STATE_WRITE_QUEUE_CAPACITY: usize = 1024;

/// Most write jobs one batch commits together.
const STATE_WRITE_BATCH_MAX: usize = 64;

/// Read-only connections serving concurrent reads.
const STATE_READER_CONNECTIONS: usize = 4;

/// How often the checkpointer folds the WAL back into the database file.
const STATE_CHECKPOINT_INTERVAL: Duration = Duration::from_secs(10);

/// WAL pages after which the writer checkpoints inline, a backstop for a checkpointer
/// that keeps losing to readers.
const WRITER_WAL_AUTOCHECKPOINT_PAGES: u32 = 10_000;

/// Size the WAL file is truncated back to after a checkpoint resets it.
const STATE_JOURNAL_SIZE_LIMIT_BYTES: u64 = 64 * 1024 * 1024;

/// Savepoint each job of a batch runs under, so a failing job rolls back alone.
const WRITE_JOB_SAVEPOINT: &str = "acps_job";

/// Rows `ANALYZE` samples per index, SQLite's recommended bound for `PRAGMA optimize`.
const STATE_ANALYSIS_LIMIT: u32 = 400;

/// `0x10002` analyzes every table whose statistics are missing or stale, not only the
/// tables this connection has queried, since a fresh connection has queried none.
/// Readers open after it runs, so they plan with the same statistics.
const STATE_OPTIMIZE_PRAGMA: &str = "PRAGMA optimize = 0x10002;";

const WRITER_THREAD_NAME: &str = "acps-state-writer";
const CHECKPOINTER_THREAD_NAME: &str = "acps-state-checkpoint";

/// A queued write: runs against the writer's store, or is told why it cannot, and
/// returns what to do once the batch's commit outcome is known.
type WriteJobRun = Box<dyn FnOnce(std::result::Result<&StateStore, &str>) -> JobOutcome + Send>;

/// Called with the batch's commit failure, `None` when it committed.
type SettleWrite = Box<dyn FnOnce(Option<&str>) + Send>;

struct WriteJob {
    enqueued_at: Instant,
    run: WriteJobRun,
}

struct JobOutcome {
    succeeded: bool,
    settle: SettleWrite,
}

enum WriterMessage {
    Job(WriteJob),
    /// Run everything queued before this, then stop taking jobs.
    Shutdown(oneshot::Sender<()>),
}

/// Cloneable handle to the state database, shared by every daemon component.
#[derive(Clone)]
pub struct StateHandle {
    inner: Arc<HandleInner>,
}

struct HandleInner {
    path: PathBuf,
    feed_epoch: String,
    sender: mpsc::Sender<WriterMessage>,
    readers: Arc<Semaphore>,
    idle_readers: Mutex<Vec<StateStore>>,
    threads: Mutex<Option<HandleThreads>>,
}

struct HandleThreads {
    writer: JoinHandle<()>,
    checkpointer: JoinHandle<()>,
    stop_checkpointer: std::sync::mpsc::Sender<()>,
}

impl StateHandle {
    /// Take ownership of a migrated store as the writer and start the writer and
    /// checkpointer threads. The store's event hub and external-logging flag stay with
    /// the writer; readers open lazily on first use.
    pub fn start(store: StateStore) -> Result<Self> {
        Self::start_with_queue_capacity(store, STATE_WRITE_QUEUE_CAPACITY)
    }

    fn start_with_queue_capacity(mut store: StateStore, queue_capacity: usize) -> Result<Self> {
        let feed_epoch = next_feed_epoch();
        store.set_feed_epoch(feed_epoch.clone());
        store.connection().execute_batch(&format!(
            "PRAGMA wal_autocheckpoint = {WRITER_WAL_AUTOCHECKPOINT_PAGES}; \
             PRAGMA journal_size_limit = {STATE_JOURNAL_SIZE_LIMIT_BYTES}; \
             PRAGMA analysis_limit = {STATE_ANALYSIS_LIMIT};"
        ))?;
        // Takes the write lock, so another process holding it past the busy timeout
        // costs only stale planner statistics, not the daemon start.
        if let Err(error) = store.connection().execute_batch(STATE_OPTIMIZE_PRAGMA) {
            tracing::warn!(%error, "state planner statistics refresh failed");
        }
        let path = store.path().to_path_buf();
        let checkpoint_connection = open_checkpoint_connection(&path)?;
        let (sender, receiver) = mpsc::channel(queue_capacity);
        let writer = spawn_thread(WRITER_THREAD_NAME, move || run_writer(store, receiver))?;
        let (stop_checkpointer, stop_receiver) = std::sync::mpsc::channel();
        let checkpointer = spawn_thread(CHECKPOINTER_THREAD_NAME, move || {
            run_checkpointer(checkpoint_connection, stop_receiver)
        })?;
        Ok(Self {
            inner: Arc::new(HandleInner {
                path,
                feed_epoch,
                sender,
                readers: Arc::new(Semaphore::new(STATE_READER_CONNECTIONS)),
                idle_readers: Mutex::new(Vec::with_capacity(STATE_READER_CONNECTIONS)),
                threads: Mutex::new(Some(HandleThreads {
                    writer,
                    checkpointer,
                    stop_checkpointer,
                })),
            }),
        })
    }

    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// The change-feed epoch every connection of this handle reports.
    pub fn feed_epoch(&self) -> &str {
        &self.inner.feed_epoch
    }

    /// Run `job` on a read-only connection. It never waits on a commit; a job that
    /// writes fails with `SQLITE_READONLY`.
    pub async fn read<R, F>(&self, job: F) -> Result<R>
    where
        F: FnOnce(&StateStore) -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let wait_started_at = Instant::now();
        let permit = self
            .inner
            .readers
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| StackError::StateAccessFailed {
                reason: "state reader pool closed".to_owned(),
            })?;
        let permit_wait = wait_started_at.elapsed();
        if permit_wait >= SLOW_STATE_TRANSACTION_THRESHOLD {
            tracing::warn!(
                target: LATENCY_TARGET,
                permit_wait_ms = duration_millis(permit_wait),
                "slow state reader permit"
            );
        }
        let task_inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            // Held by the task, so a caller that stops waiting cannot free the
            // permit while the job still runs.
            let _permit = permit;
            let store = match task_inner.take_idle_reader() {
                Some(store) => store,
                None => open_reader(&task_inner.path, &task_inner.feed_epoch)?,
            };
            let result = job(&store);
            // A store whose job panicked is dropped with the unwinding task and
            // reopened on a later read.
            task_inner.return_idle_reader(store);
            result
        })
        .await
        .map_err(|join_error| StackError::StateAccessFailed {
            reason: format!("state read job failed: {join_error}"),
        })?
    }

    /// Run `job` on the writer and return once its writes are committed. The job shares
    /// one transaction with the rest of its batch but rolls back alone when it fails.
    pub async fn write<R, F>(&self, job: F) -> Result<R>
    where
        F: FnOnce(&StateStore) -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let (reply, reply_receiver) = oneshot::channel();
        self.inner
            .sender
            .send(WriterMessage::Job(replying_job(job, reply)))
            .await
            .map_err(|_| StackError::StateWriterStopped)?;
        receive_reply(reply_receiver.await)
    }

    /// `write` for callers already on a blocking thread; panics on an async worker. A
    /// write job calling it gets an error, since the writer would wait on itself.
    pub fn blocking_write<R, F>(&self, job: F) -> Result<R>
    where
        F: FnOnce(&StateStore) -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        if std::thread::current().name() == Some(WRITER_THREAD_NAME) {
            return Err(StackError::StateAccessFailed {
                reason: "a state write job cannot wait on another state write".to_owned(),
            });
        }
        let (reply, reply_receiver) = oneshot::channel();
        self.inner
            .sender
            .blocking_send(WriterMessage::Job(replying_job(job, reply)))
            .map_err(|_| StackError::StateWriterStopped)?;
        receive_reply(reply_receiver.blocking_recv())
    }

    /// Queue a write nobody waits for, such as an audit row. A full queue drops it with
    /// a warning rather than stalling the caller; a failure is logged by the writer.
    pub fn write_detached<F>(&self, label: &'static str, job: F)
    where
        F: FnOnce(&StateStore) -> Result<()> + Send + 'static,
    {
        let job = WriteJob {
            enqueued_at: Instant::now(),
            run: Box::new(move |store| {
                let result = match store {
                    Ok(store) => job(store),
                    Err(reason) => Err(StackError::StateAccessFailed {
                        reason: reason.to_owned(),
                    }),
                };
                let succeeded = result.is_ok();
                JobOutcome {
                    succeeded,
                    settle: Box::new(move |commit_failure| match (result, commit_failure) {
                        (Err(error), _) => tracing::warn!(
                            label,
                            error = %report(&error),
                            "detached state write failed"
                        ),
                        (Ok(()), Some(reason)) => tracing::warn!(
                            label,
                            reason,
                            "detached state write lost to a failed commit"
                        ),
                        (Ok(()), None) => {}
                    }),
                }
            }),
        };
        match self.inner.sender.try_send(WriterMessage::Job(job)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                tracing::warn!(label, "state write queue full; dropped detached write");
            }
            Err(TrySendError::Closed(_)) => {
                tracing::warn!(label, "state writer stopped; dropped detached write");
            }
        }
    }

    /// Commit everything already queued, then stop the writer and checkpointer threads.
    /// Writes sent afterwards fail with `StateWriterStopped`. Later calls are no-ops.
    pub async fn shutdown(&self) {
        let (ack, ack_receiver) = oneshot::channel();
        if self
            .inner
            .sender
            .send(WriterMessage::Shutdown(ack))
            .await
            .is_ok()
            && ack_receiver.await.is_err()
        {
            tracing::warn!("state writer exited before acknowledging shutdown");
        }
        let threads = self
            .inner
            .threads
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let Some(threads) = threads else {
            return;
        };
        let joined = tokio::task::spawn_blocking(move || {
            // The checkpointer also stops once this sender drops, so a failed send needs
            // no handling.
            if threads.stop_checkpointer.send(()).is_err() {
                tracing::debug!("state checkpointer already stopped");
            }
            if threads.writer.join().is_err() {
                tracing::error!("state writer thread panicked");
            }
            if threads.checkpointer.join().is_err() {
                tracing::error!("state checkpointer thread panicked");
            }
        })
        .await;
        if let Err(error) = joined {
            tracing::error!(%error, "joining the state threads failed");
        }
    }
}

impl HandleInner {
    fn take_idle_reader(&self) -> Option<StateStore> {
        self.idle_readers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop()
    }

    fn return_idle_reader(&self, store: StateStore) {
        self.idle_readers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(store);
    }
}

fn replying_job<R, F>(job: F, reply: oneshot::Sender<Result<R>>) -> WriteJob
where
    F: FnOnce(&StateStore) -> Result<R> + Send + 'static,
    R: Send + 'static,
{
    WriteJob {
        enqueued_at: Instant::now(),
        run: Box::new(move |store| {
            let result = match store {
                Ok(store) => job(store),
                Err(reason) => Err(StackError::StateAccessFailed {
                    reason: reason.to_owned(),
                }),
            };
            JobOutcome {
                succeeded: result.is_ok(),
                settle: Box::new(move |commit_failure| {
                    let result = match (result, commit_failure) {
                        (Ok(_), Some(reason)) => Err(StackError::StateAccessFailed {
                            reason: reason.to_owned(),
                        }),
                        (result, _) => result,
                    };
                    if reply.send(result).is_err() {
                        tracing::debug!("state write caller stopped waiting for its reply");
                    }
                }),
            }
        }),
    }
}

fn receive_reply<R>(reply: std::result::Result<Result<R>, oneshot::error::RecvError>) -> Result<R> {
    reply.map_err(|_| StackError::StateAccessFailed {
        reason: "state writer dropped the job before replying".to_owned(),
    })?
}

fn spawn_thread(name: &str, body: impl FnOnce() + Send + 'static) -> Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(body)
        .map_err(|error| StackError::StateAccessFailed {
            reason: format!("failed to start the {name} thread: {error}"),
        })
}

fn open_reader(path: &Path, feed_epoch: &str) -> Result<StateStore> {
    let store = StateStore::open_with_feed_epoch(path, feed_epoch.to_owned())?;
    store.connection().execute_batch("PRAGMA query_only = ON")?;
    Ok(store)
}

fn open_checkpoint_connection(path: &Path) -> Result<Connection> {
    let connection = Connection::open(path)?;
    connection.execute_batch(&format!(
        "PRAGMA journal_size_limit = {STATE_JOURNAL_SIZE_LIMIT_BYTES};"
    ))?;
    Ok(connection)
}

fn run_writer(store: StateStore, mut receiver: mpsc::Receiver<WriterMessage>) {
    let mut shutdown_acks = Vec::new();
    // After a shutdown closes the channel, `blocking_recv` still yields every message
    // queued before it, so the loop drains the queue before it ends.
    while let Some(first) = receiver.blocking_recv() {
        let mut batch = Vec::new();
        let mut next = Some(first);
        while let Some(message) = next.take() {
            match message {
                WriterMessage::Job(job) => batch.push(job),
                WriterMessage::Shutdown(ack) => {
                    receiver.close();
                    shutdown_acks.push(ack);
                }
            }
            if batch.len() < STATE_WRITE_BATCH_MAX {
                next = receiver.try_recv().ok();
            }
        }
        if !batch.is_empty() {
            run_batch(&store, batch);
        }
    }
    for ack in shutdown_acks {
        if ack.send(()).is_err() {
            tracing::debug!("state shutdown caller stopped waiting");
        }
    }
}

/// Run `jobs` in one transaction, each under its own savepoint, then publish and reply
/// once the commit outcome is known.
fn run_batch(store: &StateStore, jobs: Vec<WriteJob>) {
    let batch_started_at = Instant::now();
    let batch_size = jobs.len();
    let queue_wait = jobs
        .iter()
        .map(|job| batch_started_at.duration_since(job.enqueued_at))
        .max()
        .unwrap_or_default();
    let connection = store.connection();
    if let Err(error) = connection.execute_batch("BEGIN IMMEDIATE") {
        let reason = format!("state write batch could not begin: {error}");
        tracing::warn!(%reason, batch_size, "state write batch failed");
        for job in jobs {
            ((job.run)(Err(&reason)).settle)(None);
        }
        return;
    }
    store.set_in_writer_batch(true);
    let mut outcomes = Vec::with_capacity(batch_size);
    let mut aborted: Option<String> = None;
    for job in jobs {
        if let Some(reason) = &aborted {
            outcomes.push((job.run)(Err(reason.as_str())));
            continue;
        }
        let publishes_before = store.pending_publish_count();
        if let Err(error) = connection.execute_batch(&format!("SAVEPOINT {WRITE_JOB_SAVEPOINT}")) {
            let reason = format!("state write job could not open its savepoint: {error}");
            outcomes.push((job.run)(Err(&reason)));
            continue;
        }
        let outcome = catch_unwind(AssertUnwindSafe(|| (job.run)(Ok(store))));
        let keep = matches!(&outcome, Ok(outcome) if outcome.succeeded);
        let end_savepoint = if keep {
            format!("RELEASE {WRITE_JOB_SAVEPOINT}")
        } else {
            store.discard_publishes_after(publishes_before);
            format!("ROLLBACK TO {WRITE_JOB_SAVEPOINT}; RELEASE {WRITE_JOB_SAVEPOINT}")
        };
        if let Err(error) = connection.execute_batch(&end_savepoint) {
            tracing::error!(%error, keep, "state write job savepoint did not close");
        }
        match outcome {
            Ok(outcome) => outcomes.push(outcome),
            // The job's reply sender unwound with it, so its caller sees a dropped job.
            Err(_) => tracing::error!("state write job panicked; its writes rolled back"),
        }
        // Disk-full, IO and interrupt errors make SQLite roll back the whole transaction on
        // its own, taking every earlier job's writes with it.
        if connection.is_autocommit() {
            aborted = Some("state write batch was rolled back by SQLite".to_owned());
        }
    }
    store.set_in_writer_batch(false);
    let commit_started_at = Instant::now();
    let commit_result = match aborted {
        Some(reason) => Err(reason),
        None => connection
            .execute_batch("COMMIT")
            .map_err(|error| format!("state write batch commit failed: {error}")),
    };
    let commit_failure = match commit_result {
        Ok(()) => None,
        Err(reason) => {
            tracing::warn!(%reason, batch_size, "state write batch failed");
            if !connection.is_autocommit()
                && let Err(rollback_error) = connection.execute_batch("ROLLBACK")
            {
                tracing::error!(error = %rollback_error, "state write batch rollback failed");
            }
            store.discard_publishes_after(0);
            Some(reason)
        }
    };
    let commit = commit_started_at.elapsed();
    if commit_failure.is_none() {
        store.flush_publishes();
    }
    for outcome in outcomes {
        (outcome.settle)(commit_failure.as_deref());
    }
    let batch = batch_started_at.elapsed();
    if queue_wait >= SLOW_STATE_TRANSACTION_THRESHOLD || batch >= SLOW_STATE_TRANSACTION_THRESHOLD {
        tracing::warn!(
            target: LATENCY_TARGET,
            batch_size,
            queue_wait_ms = duration_millis(queue_wait),
            batch_ms = duration_millis(batch),
            commit_ms = duration_millis(commit),
            "slow state write batch"
        );
    }
}

fn run_checkpointer(connection: Connection, stop: std::sync::mpsc::Receiver<()>) {
    // Any message or a dropped sender stops the loop; only the timeout runs a checkpoint.
    while let Err(std::sync::mpsc::RecvTimeoutError::Timeout) =
        stop.recv_timeout(STATE_CHECKPOINT_INTERVAL)
    {
        // PASSIVE never waits on readers or the writer; frames it cannot copy yet wait
        // for the next tick.
        if let Err(error) = connection.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(())) {
            tracing::warn!(%error, "state WAL checkpoint failed");
        }
    }
}

#[cfg(test)]
impl StateHandle {
    pub(super) fn start_for_test(store: StateStore, queue_capacity: usize) -> Result<Self> {
        Self::start_with_queue_capacity(store, queue_capacity)
    }
}

#[cfg(test)]
mod tests;
