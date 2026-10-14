use super::*;
use crate::events::EventHub;
use std::sync::mpsc as std_mpsc;
use tokio::sync::broadcast::error::TryRecvError;

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

fn migrated_store(dir: &tempfile::TempDir) -> StateStore {
    let store = StateStore::open(dir.path().join("state.sqlite")).expect("open state");
    store.migrate().expect("migrate");
    store
}

fn event_count(store: &StateStore, kind: &str) -> Result<i64> {
    Ok(store.connection().query_row(
        "SELECT COUNT(*) FROM events WHERE kind = ?1",
        [kind],
        |row| row.get(0),
    )?)
}

async fn count_events(handle: &StateHandle, kind: &'static str) -> i64 {
    handle
        .read(move |store| event_count(store, kind))
        .await
        .expect("count events")
}

fn append(store: &StateStore, kind: &str) -> Result<()> {
    store.append_event("info", kind, "test", "{}")?;
    Ok(())
}

/// Occupy the writer with a job that blocks until the returned sender fires or drops,
/// and wait until the writer has started it.
async fn hold_writer(handle: &StateHandle) -> std_mpsc::Sender<()> {
    let (release, released) = std_mpsc::channel::<()>();
    let (started, started_receiver) = oneshot::channel();
    handle.write_detached("hold", move |_| {
        started.send(()).expect("hold start observed");
        // A release message and a dropped sender both end the hold.
        let _released = released.recv();
        Ok(())
    });
    tokio::time::timeout(TEST_TIMEOUT, started_receiver)
        .await
        .expect("writer started the hold job")
        .expect("hold job reported start");
    release
}

async fn wait_for_queued(handle: &StateHandle, queued: usize) {
    let sender = &handle.inner.sender;
    tokio::time::timeout(TEST_TIMEOUT, async {
        while sender.max_capacity() - sender.capacity() < queued {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("jobs queued");
}

#[tokio::test]
async fn failing_job_rolls_back_alone_within_its_batch() {
    let dir = tempfile::tempdir().expect("tempdir");
    let handle = StateHandle::start(migrated_store(&dir)).expect("start handle");
    let release = hold_writer(&handle).await;
    let first = tokio::spawn({
        let handle = handle.clone();
        async move { handle.write(|store| append(store, "test.first")).await }
    });
    wait_for_queued(&handle, 1).await;
    let failing = tokio::spawn({
        let handle = handle.clone();
        async move {
            handle
                .write(|store| {
                    append(store, "test.failing")?;
                    Err::<(), _>(StackError::InvalidEventPayload)
                })
                .await
        }
    });
    wait_for_queued(&handle, 2).await;
    let last = tokio::spawn({
        let handle = handle.clone();
        async move { handle.write(|store| append(store, "test.last")).await }
    });
    wait_for_queued(&handle, 3).await;
    release.send(()).expect("release writer");

    first.await.expect("join").expect("first write");
    assert!(matches!(
        failing.await.expect("join"),
        Err(StackError::InvalidEventPayload)
    ));
    last.await.expect("join").expect("last write");
    assert_eq!(count_events(&handle, "test.first").await, 1);
    assert_eq!(count_events(&handle, "test.failing").await, 0);
    assert_eq!(count_events(&handle, "test.last").await, 1);
}

#[tokio::test]
async fn publishes_follow_the_commit_and_skip_rolled_back_jobs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut store = migrated_store(&dir);
    let hub = EventHub::new();
    store.attach_event_hub(hub.clone());
    let mut frames = hub.subscribe();
    let handle = StateHandle::start(store).expect("start handle");

    let failed = handle
        .write(|store| {
            append(store, "test.rolled_back")?;
            Err::<(), _>(StackError::InvalidEventPayload)
        })
        .await;
    assert!(failed.is_err());
    assert!(matches!(frames.try_recv(), Err(TryRecvError::Empty)));

    handle
        .write(|store| append(store, "test.committed"))
        .await
        .expect("committed write");
    let frame = frames
        .try_recv()
        .expect("frame published by the time write returns");
    assert_eq!(frame.payload["kind"], "test.committed");
    assert_eq!(count_events(&handle, "test.committed").await, 1);
}

#[tokio::test]
async fn reads_run_while_the_writer_is_held() {
    let dir = tempfile::tempdir().expect("tempdir");
    let handle = StateHandle::start(migrated_store(&dir)).expect("start handle");
    handle
        .write(|store| append(store, "test.seed"))
        .await
        .expect("seed");
    let release = hold_writer(&handle).await;
    let count = tokio::time::timeout(TEST_TIMEOUT, count_events(&handle, "test.seed"))
        .await
        .expect("read finished while the writer was held");
    assert_eq!(count, 1);
    release.send(()).expect("release writer");
}

#[tokio::test]
async fn readers_reject_writes_and_share_the_feed_epoch() {
    let dir = tempfile::tempdir().expect("tempdir");
    let handle = StateHandle::start(migrated_store(&dir)).expect("start handle");
    let write_on_reader = handle.read(|store| append(store, "test.reader")).await;
    assert!(write_on_reader.is_err());
    let reader_epoch = handle
        .read(|store| Ok(store.feed_epoch().to_owned()))
        .await
        .expect("read epoch");
    let writer_epoch = handle
        .write(|store| Ok(store.feed_epoch().to_owned()))
        .await
        .expect("write epoch");
    assert_eq!(reader_epoch, handle.feed_epoch());
    assert_eq!(writer_epoch, handle.feed_epoch());
}

#[tokio::test]
async fn detached_write_drops_when_the_queue_is_full() {
    let dir = tempfile::tempdir().expect("tempdir");
    let handle = StateHandle::start_for_test(migrated_store(&dir), 1).expect("start handle");
    let release = hold_writer(&handle).await;
    handle.write_detached("kept", |store| append(store, "test.kept"));
    handle.write_detached("dropped", |store| append(store, "test.dropped"));
    release.send(()).expect("release writer");
    handle.shutdown().await;
    let store = StateStore::open(handle.path()).expect("reopen");
    assert_eq!(event_count(&store, "test.kept").expect("count"), 1);
    assert_eq!(event_count(&store, "test.dropped").expect("count"), 0);
}

#[tokio::test]
async fn shutdown_commits_queued_writes_and_rejects_later_ones() {
    let dir = tempfile::tempdir().expect("tempdir");
    let handle = StateHandle::start(migrated_store(&dir)).expect("start handle");
    let release = hold_writer(&handle).await;
    handle.write_detached("queued", |store| append(store, "test.queued"));
    let shutdown = tokio::spawn({
        let handle = handle.clone();
        async move { handle.shutdown().await }
    });
    wait_for_queued(&handle, 2).await;
    release.send(()).expect("release writer");
    tokio::time::timeout(TEST_TIMEOUT, shutdown)
        .await
        .expect("shutdown finished")
        .expect("join");

    let store = StateStore::open(handle.path()).expect("reopen");
    assert_eq!(event_count(&store, "test.queued").expect("count"), 1);
    let late = handle.write(|store| append(store, "test.late")).await;
    assert!(matches!(late, Err(StackError::StateWriterStopped)));
}

#[tokio::test]
async fn panicking_job_fails_alone_and_the_writer_keeps_running() {
    let dir = tempfile::tempdir().expect("tempdir");
    let handle = StateHandle::start(migrated_store(&dir)).expect("start handle");
    let panicked = handle
        .write(|store| -> Result<()> {
            append(store, "test.panicked")?;
            panic!("job panic under test");
        })
        .await;
    assert!(matches!(
        panicked,
        Err(StackError::StateAccessFailed { .. })
    ));
    handle
        .write(|store| append(store, "test.after_panic"))
        .await
        .expect("writer still running");
    assert_eq!(count_events(&handle, "test.panicked").await, 0);
    assert_eq!(count_events(&handle, "test.after_panic").await, 1);
}

#[tokio::test]
async fn blocking_write_from_a_write_job_fails_instead_of_deadlocking() {
    let dir = tempfile::tempdir().expect("tempdir");
    let handle = StateHandle::start(migrated_store(&dir)).expect("start handle");
    let inner_handle = handle.clone();
    let nested = tokio::time::timeout(
        TEST_TIMEOUT,
        handle
            .write(move |_| Ok(inner_handle.blocking_write(|store| append(store, "test.nested")))),
    )
    .await
    .expect("nested write returned")
    .expect("outer write");
    assert!(matches!(nested, Err(StackError::StateAccessFailed { .. })));
    handle
        .write(|store| append(store, "test.after_nested"))
        .await
        .expect("writer still running");
    assert_eq!(count_events(&handle, "test.nested").await, 0);
    assert_eq!(count_events(&handle, "test.after_nested").await, 1);
}

#[tokio::test]
async fn batch_rolled_back_by_sqlite_fails_every_job_and_publishes_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut store = migrated_store(&dir);
    let hub = EventHub::new();
    store.attach_event_hub(hub.clone());
    let mut frames = hub.subscribe();
    let handle = StateHandle::start(store).expect("start handle");
    let release = hold_writer(&handle).await;
    let before = tokio::spawn({
        let handle = handle.clone();
        async move {
            handle
                .write(|store| append(store, "test.before_abort"))
                .await
        }
    });
    wait_for_queued(&handle, 1).await;
    let aborting = tokio::spawn({
        let handle = handle.clone();
        async move {
            handle
                .write(|store| {
                    append(store, "test.aborting")?;
                    // Stands in for a disk-full or IO error that ends the transaction.
                    store.connection().execute_batch("ROLLBACK")?;
                    Ok(append(store, "test.after_abort").is_err())
                })
                .await
        }
    });
    wait_for_queued(&handle, 2).await;
    let after = tokio::spawn({
        let handle = handle.clone();
        async move {
            handle
                .write(|store| append(store, "test.queued_after"))
                .await
        }
    });
    wait_for_queued(&handle, 3).await;
    release.send(()).expect("release writer");

    assert!(before.await.expect("join").is_err());
    assert!(aborting.await.expect("join").is_err());
    assert!(after.await.expect("join").is_err());
    for kind in [
        "test.before_abort",
        "test.aborting",
        "test.after_abort",
        "test.queued_after",
    ] {
        assert_eq!(count_events(&handle, kind).await, 0, "{kind}");
    }
    assert!(matches!(frames.try_recv(), Err(TryRecvError::Empty)));
    handle
        .write(|store| append(store, "test.next_batch"))
        .await
        .expect("next batch commits");
    assert_eq!(count_events(&handle, "test.next_batch").await, 1);
}
