use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use acp_stack::runtime::agent::acp_bridge::{AcpBridge, AcpPermissionPolicy, SessionEventSink};

use crate::support::{fake_agent_config, fake_env, null_sink};

#[derive(Default)]
struct BlockingSink {
    append_started: tokio::sync::Notify,
    allow_append_finish: tokio::sync::Notify,
    flush_started: tokio::sync::Notify,
    first_append_seen: AtomicBool,
    append_done: AtomicBool,
}

impl SessionEventSink for BlockingSink {
    fn append<'a>(
        &'a self,
        _session_id: &'a str,
        _kind: &'a str,
        _payload_json: &'a str,
    ) -> futures::future::BoxFuture<'a, ()> {
        Box::pin(async move {
            if !self.first_append_seen.swap(true, Ordering::SeqCst) {
                self.append_started.notify_one();
                self.allow_append_finish.notified().await;
            }
            self.append_done.store(true, Ordering::SeqCst);
        })
    }

    fn flush<'a>(&'a self) -> futures::future::BoxFuture<'a, ()> {
        Box::pin(async move {
            assert!(
                self.append_done.load(Ordering::SeqCst),
                "shutdown flushed the session event sink before the connection task drained"
            );
            self.flush_started.notify_waiters();
        })
    }
}

#[tokio::test]
async fn shutdown_waits_for_connection_task_before_flushing_sink() {
    use agent_client_protocol::schema::v1::{ContentBlock, PromptRequest, TextContent};

    let sink = Arc::new(BlockingSink::default());
    let sink_dyn: Arc<dyn SessionEventSink> = sink.clone();
    let bridge = Arc::new(
        AcpBridge::spawn(
            &std::env::temp_dir(),
            &fake_agent_config(),
            fake_env(),
            std::env::temp_dir(),
            sink_dyn,
            AcpPermissionPolicy::Cancel,
            &Default::default(),
            "/bin/sh",
            None,
            None,
        )
        .await
        .expect("spawn"),
    );
    let response = bridge
        .new_session(std::env::temp_dir(), vec![])
        .await
        .expect("new session");
    let prompt = PromptRequest::new(
        response.session_id,
        vec![ContentBlock::Text(TextContent::new("block until shutdown"))],
    );
    let prompt_bridge = Arc::clone(&bridge);
    let prompt_task = tokio::spawn(async move { prompt_bridge.prompt_session(prompt).await });

    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        sink.append_started.notified(),
    )
    .await
    .expect("append started");

    let shutdown_bridge = Arc::clone(&bridge);
    let shutdown_task = tokio::spawn(async move { shutdown_bridge.shutdown().await });
    let early_flush = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        sink.flush_started.notified(),
    )
    .await;
    assert!(
        early_flush.is_err(),
        "sink flushed while a notification handler was still appending"
    );

    sink.allow_append_finish.notify_waiters();
    shutdown_task
        .await
        .expect("shutdown task joins")
        .expect("shutdown ok");
    let _ = prompt_task.await.expect("prompt task joins");
}

#[derive(Default)]
struct BlockingCaptureSink {
    capture_started: tokio::sync::Notify,
    allow_capture_finish: tokio::sync::Notify,
    flush_started: tokio::sync::Notify,
    first_capture_seen: AtomicBool,
    append_done: AtomicBool,
}

impl SessionEventSink for BlockingCaptureSink {
    fn capture_session_update<'a>(
        &'a self,
        _agent_session_id: &'a str,
        _update: &'a agent_client_protocol::schema::v1::SessionUpdate,
    ) -> futures::future::BoxFuture<'a, bool> {
        Box::pin(async move {
            if !self.first_capture_seen.swap(true, Ordering::SeqCst) {
                self.capture_started.notify_one();
                self.allow_capture_finish.notified().await;
            }
            true
        })
    }

    fn append<'a>(
        &'a self,
        _session_id: &'a str,
        _kind: &'a str,
        _payload_json: &'a str,
    ) -> futures::future::BoxFuture<'a, ()> {
        Box::pin(async move {
            self.append_done.store(true, Ordering::SeqCst);
        })
    }

    fn flush<'a>(&'a self) -> futures::future::BoxFuture<'a, ()> {
        Box::pin(async move {
            assert!(
                self.append_done.load(Ordering::SeqCst),
                "shutdown flushed before blocked capture reached the raw append"
            );
            self.flush_started.notify_waiters();
        })
    }
}

#[tokio::test]
async fn shutdown_drains_notification_queued_before_capture_blocks() {
    use agent_client_protocol::schema::v1::{ContentBlock, PromptRequest, TextContent};

    let sink = Arc::new(BlockingCaptureSink::default());
    let sink_dyn: Arc<dyn SessionEventSink> = sink.clone();
    let bridge = Arc::new(
        AcpBridge::spawn(
            &std::env::temp_dir(),
            &fake_agent_config(),
            fake_env(),
            std::env::temp_dir(),
            sink_dyn,
            AcpPermissionPolicy::Cancel,
            &Default::default(),
            "/bin/sh",
            None,
            None,
        )
        .await
        .expect("spawn"),
    );
    let response = bridge
        .new_session(std::env::temp_dir(), vec![])
        .await
        .expect("new session");
    let prompt = PromptRequest::new(
        response.session_id,
        vec![ContentBlock::Text(TextContent::new(
            "block capture until shutdown",
        ))],
    );
    let prompt_bridge = Arc::clone(&bridge);
    let prompt_task = tokio::spawn(async move { prompt_bridge.prompt_session(prompt).await });

    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        sink.capture_started.notified(),
    )
    .await
    .expect("capture started");

    let shutdown_bridge = Arc::clone(&bridge);
    let shutdown_task = tokio::spawn(async move { shutdown_bridge.shutdown().await });
    let early_flush = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        sink.flush_started.notified(),
    )
    .await;
    assert!(
        early_flush.is_err(),
        "sink flushed while capture was blocked"
    );

    sink.allow_capture_finish.notify_waiters();
    shutdown_task
        .await
        .expect("shutdown task joins")
        .expect("shutdown ok");
    let _ = prompt_task.await.expect("prompt task joins");
}

#[tokio::test]
async fn cancel_session_settles_prompt_with_cancelled_stop_reason() {
    use agent_client_protocol::schema::v1::{ContentBlock, PromptRequest, StopReason, TextContent};
    let bridge = AcpBridge::spawn(
        &std::env::temp_dir(),
        &fake_agent_config(),
        fake_env(),
        std::env::temp_dir(),
        null_sink(),
        AcpPermissionPolicy::Cancel,
        &Default::default(),
        "/bin/sh",
        None,
        None,
    )
    .await
    .expect("spawn");
    let response = bridge
        .new_session(std::env::temp_dir(), vec![])
        .await
        .expect("new");
    let session_id = response.session_id;

    // The fake agent checks the cancel flag only after emitting notifications,
    // so notify-then-prompt deterministically returns `cancelled`.
    bridge
        .cancel_session(session_id.clone())
        .await
        .expect("cancel");
    let prompt = PromptRequest::new(
        session_id,
        vec![ContentBlock::Text(TextContent::new("ignored"))],
    );
    let stop = bridge
        .prompt_session(prompt)
        .await
        .expect("prompt")
        .stop_reason;
    assert!(matches!(stop, StopReason::Cancelled));
    bridge.shutdown().await.expect("shutdown ok");
}
