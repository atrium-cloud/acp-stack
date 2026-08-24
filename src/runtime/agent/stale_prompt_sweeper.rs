//! Supervisor-side guarantee that every `prompts` row reaches a terminal
//! status: flips in-flight prompts to `Stalled` when no ACP `session/update`
//! has touched the row within the configured threshold.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex as TokioMutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::state::{EVENT_KIND_PROMPT_STALLED, EVENT_SOURCE_SYSTEM, StateStore};

/// `error_message` written onto every `Stalled` prompt by the sweeper.
pub const SWEEPER_STALL_REASON: &str = "no agent updates within threshold";

/// Handle owning the background sweep task and its cancellation token;
/// dropping it cancels the task.
pub struct StalePromptSweeper {
    handle: Option<JoinHandle<()>>,
    cancel: CancellationToken,
}

impl StalePromptSweeper {
    /// Start a sweeper bound to `state`. The first sweep waits one full
    /// `sweep_interval` so startup reconcile settles before any scan.
    pub fn spawn(
        state: Arc<TokioMutex<StateStore>>,
        threshold: Duration,
        sweep_interval: Duration,
    ) -> Self {
        let cancel = CancellationToken::new();
        let cancel_inner = cancel.clone();
        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(sweep_interval) => {}
                    _ = cancel_inner.cancelled() => return,
                }
                let pairs = {
                    let guard = state.lock().await;
                    match guard.mark_stalled_prompts(threshold, SWEEPER_STALL_REASON) {
                        Ok(pairs) => pairs,
                        Err(err) => {
                            tracing::warn!(
                                error = %err,
                                "stale prompt sweeper: mark_stalled_prompts failed"
                            );
                            continue;
                        }
                    }
                };
                if pairs.is_empty() {
                    continue;
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
                            "stale prompt sweeper: failed to append prompt.stalled event"
                        );
                    }
                }
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
            tracing::warn!(error = ?err, "stale prompt sweeper task did not exit cleanly");
        }
    }
}

impl Drop for StalePromptSweeper {
    fn drop(&mut self) {
        // `Drop` is sync so the handle cannot be awaited here; explicit
        // `shutdown` is preferred and this only covers forgotten paths.
        self.cancel.cancel();
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}
