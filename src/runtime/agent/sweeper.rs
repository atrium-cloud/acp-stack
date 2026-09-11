//! Background state sweeper. Guarantees every `prompts` row reaches a
//! terminal status (flips in-flight prompts to `Stalled` when no ACP
//! `session/update` has touched the row within the configured threshold)
//! and demotes idle `active` sessions to `available`.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex as TokioMutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::state::{
    EVENT_KIND_PROMPT_STALLED, EVENT_KIND_SESSION_AVAILABLE, EVENT_SOURCE_SYSTEM, StateStore,
};

/// `error_message` written onto every `Stalled` prompt by the sweeper.
pub const SWEEPER_STALL_REASON: &str = "no agent updates within threshold";

/// Handle owning the background sweep task and its cancellation token;
/// dropping it cancels the task.
pub struct StateSweeper {
    handle: Option<JoinHandle<()>>,
    cancel: CancellationToken,
}

impl StateSweeper {
    /// Start a sweeper bound to `state`. The first sweep waits one full
    /// `sweep_interval` so startup reconcile settles before any scan.
    pub fn spawn(
        state: Arc<TokioMutex<StateStore>>,
        threshold: Duration,
        sweep_interval: Duration,
        session_idle_threshold: Duration,
    ) -> Self {
        let cancel = CancellationToken::new();
        let cancel_inner = cancel.clone();
        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(sweep_interval) => {}
                    _ = cancel_inner.cancelled() => return,
                }
                sweep_stalled_prompts(&state, threshold).await;
                sweep_idle_sessions(&state, session_idle_threshold).await;
            }
        });
        Self {
            handle: Some(handle),
            cancel,
        }
    }

    /// Trigger cancellation and await the background task; idempotent.
    pub async fn shutdown(mut self) {
        self.cancel.cancel();
        if let Some(handle) = self.handle.take()
            && let Err(err) = handle.await
        {
            tracing::warn!(error = ?err, "state sweeper task did not exit cleanly");
        }
    }
}

async fn sweep_stalled_prompts(state: &Arc<TokioMutex<StateStore>>, threshold: Duration) {
    let pairs = {
        let guard = state.lock().await;
        match guard.mark_stalled_prompts(threshold, SWEEPER_STALL_REASON) {
            Ok(pairs) => pairs,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "state sweeper: mark_stalled_prompts failed"
                );
                return;
            }
        }
    };
    if pairs.is_empty() {
        return;
    }
    let threshold_secs = threshold.as_secs();
    let guard = state.lock().await;
    for (prompt_id, session_id) in pairs {
        let payload = serde_json::json!({
            "prompt_id": prompt_id,
            "threshold_secs": threshold_secs,
        })
        .to_string();
        if let Err(err) = guard.append_session_event_with_source(
            &session_id,
            "warn",
            EVENT_KIND_PROMPT_STALLED,
            EVENT_SOURCE_SYSTEM,
            "prompt stalled",
            &payload,
        ) {
            tracing::warn!(
                error = %err,
                prompt_id = %prompt_id,
                session_id = %session_id,
                "state sweeper: failed to append prompt.stalled event"
            );
        }
    }
}

async fn sweep_idle_sessions(state: &Arc<TokioMutex<StateStore>>, idle_threshold: Duration) {
    let ids = {
        let guard = state.lock().await;
        match guard.mark_idle_sessions(idle_threshold) {
            Ok(ids) => ids,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "state sweeper: mark_idle_sessions failed"
                );
                return;
            }
        }
    };
    if ids.is_empty() {
        return;
    }
    let payload = serde_json::json!({
        "reason": "idle",
        "threshold_secs": idle_threshold.as_secs(),
    })
    .to_string();
    let guard = state.lock().await;
    for session_id in ids {
        if let Err(err) = guard.append_session_event_with_source(
            &session_id,
            "info",
            EVENT_KIND_SESSION_AVAILABLE,
            EVENT_SOURCE_SYSTEM,
            "session available",
            &payload,
        ) {
            tracing::warn!(
                error = %err,
                session_id = %session_id,
                "state sweeper: failed to append session.available event"
            );
        }
    }
}

impl Drop for StateSweeper {
    fn drop(&mut self) {
        // `Drop` is sync so the handle cannot be awaited here; explicit
        // `shutdown` is preferred and this only covers forgotten paths.
        self.cancel.cancel();
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}
