//! Background task that flips in-flight prompts to terminal `Stalled`
//! state when no ACP `session/update` notification has touched the row
//! within the configured threshold.
//!
//! Without this sweep, an agent that crashes mid-stream (or one whose
//! upstream inference hangs without surfacing an error) would leave the
//! prompt row stuck in `running` forever — clients polling the row would
//! never see it settle. The sweeper is the supervisor-side guarantee
//! that every `prompts` row eventually reaches a terminal status, even
//! when the agent stops cooperating.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex as TokioMutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::state::{EVENT_KIND_PROMPT_STALLED, EVENT_SOURCE_SYSTEM, StateStore};

/// `error_message` written onto every `Stalled` prompt by the sweeper.
/// The constant lives at module scope so tests can match against it
/// without re-hardcoding the literal string.
pub const SWEEPER_STALL_REASON: &str = "no agent updates within threshold";

/// Background task that periodically scans the `prompts` table and
/// flips rows with no recent activity to `Stalled`. Constructed via
/// [`StalePromptSweeper::spawn`]; the returned handle owns the
/// background `tokio::task` and the cancellation token. Dropping the
/// handle cancels the task (defense in depth so a forgotten shutdown
/// path does not leave the sweeper orphaned).
pub struct StalePromptSweeper {
    handle: Option<JoinHandle<()>>,
    cancel: CancellationToken,
}

impl StalePromptSweeper {
    /// Start a sweeper bound to `state`. The first sweep happens after
    /// `sweep_interval` has elapsed (not immediately at boot) so that
    /// startup reconcile + initial prompt insertion settle before the
    /// first scan, and the cadence is steady from then on.
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

    /// Trigger cancellation and await the background task. Idempotent —
    /// callers may invoke it explicitly during graceful shutdown; the
    /// `Drop` impl falls back to the same cancellation if `shutdown` was
    /// not called.
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
        // The task may already be cancelled (via explicit `shutdown`) or
        // joined; cancelling again is a no-op. We don't await the handle
        // here because `Drop` is sync — explicit shutdown is preferred,
        // and this is defense in depth for forgotten paths.
        self.cancel.cancel();
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}
