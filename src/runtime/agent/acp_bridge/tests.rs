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

#[tokio::test]
async fn received_before_now_returns_immediately_when_nothing_in_flight() {
    let drain = Arc::new(NotificationDrain::default());
    tokio::time::timeout(Duration::from_secs(1), drain.wait_received_before_now())
        .await
        .expect("an empty drain has nothing received before now");
}

#[tokio::test]
async fn received_before_now_ignores_notifications_entered_after_the_call() {
    let drain = Arc::new(NotificationDrain::default());
    let earlier = drain.enter();

    let mut waiter = Box::pin(drain.wait_received_before_now());
    assert!(
        futures::poll!(waiter.as_mut()).is_pending(),
        "the barrier must wait for a notification entered before it"
    );
    let later = drain.enter();

    drop(earlier);
    tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .expect("a notification entered after the call must not hold the barrier");
    drop(later);
}

#[tokio::test]
async fn received_before_now_waits_for_an_earlier_notification_left_behind() {
    let drain = Arc::new(NotificationDrain::default());
    let first = drain.enter();
    let second = drain.enter();

    let mut waiter = Box::pin(drain.wait_received_before_now());
    assert!(futures::poll!(waiter.as_mut()).is_pending());

    // The later guard leaving first must not release the barrier.
    drop(second);
    assert!(
        futures::poll!(waiter.as_mut()).is_pending(),
        "the barrier must still wait for the earlier notification"
    );

    drop(first);
    tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .expect("the barrier must observe the earlier guard drop");
}
