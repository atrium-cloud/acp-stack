use super::NotificationDrain;
use std::sync::Arc;
use std::time::Duration;

#[tokio::test]
async fn wait_idle_returns_immediately_when_no_guards_active() {
    let drain = Arc::new(NotificationDrain::default());
    tokio::time::timeout(Duration::from_secs(1), drain.wait_idle())
        .await
        .expect("wait_idle must not block with zero active guards");
}

#[tokio::test]
async fn wait_idle_completes_when_last_guard_drops() {
    let drain = Arc::new(NotificationDrain::default());
    let first = drain.enter();
    let second = drain.enter();

    let waiter = tokio::spawn({
        let drain = Arc::clone(&drain);
        async move { drain.wait_idle().await }
    });

    // An intermediate N->1 drop must not release the waiter.
    drop(first);
    tokio::task::yield_now().await;
    assert!(!waiter.is_finished());

    drop(second);
    tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .expect("wait_idle must observe the final guard drop")
        .expect("waiter task must not panic");
}

#[tokio::test]
async fn wait_idle_observes_notification_fired_after_registration() {
    let drain = Arc::new(NotificationDrain::default());
    let guard = drain.enter();

    let mut waiter = Box::pin(drain.wait_idle());
    // First poll registers the waiter while a guard is still active.
    assert!(
        futures::poll!(waiter.as_mut()).is_pending(),
        "wait_idle must be pending while a guard is active"
    );

    drop(guard);
    tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .expect("registered waiter must be woken by the final guard drop");
}
