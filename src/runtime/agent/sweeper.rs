//! Background state sweeper. Guarantees every `prompts` row reaches a
//! terminal status by flipping in-flight prompts to `Stalled` once no ACP
//! `session/update` has touched the row for `stale_threshold`, or for
//! `tool_call_stale_threshold` while the turn has a tool call open. It also
//! demotes idle `active` sessions to `available` and prunes deleted-session
//! tombstones past their retention window.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex as TokioMutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::PromptsConfig;
use crate::state::{
    EVENT_KIND_PROMPT_STALLED, EVENT_KIND_SESSION_AVAILABLE, EVENT_SOURCE_SYSTEM,
    PromptStaleThresholds, SESSION_TOMBSTONE_RETENTION, StateStore,
};

/// `error_message` written onto every `Stalled` prompt by the sweeper.
pub const SWEEPER_STALL_REASON: &str = "no agent updates within threshold";

/// The stale thresholds `[prompts]` configures, shared by the sweep and every
/// stuck-prompt probe so they agree on what counts as stuck.
pub fn prompt_stale_thresholds(prompts: &PromptsConfig) -> PromptStaleThresholds {
    PromptStaleThresholds {
        quiet: prompts.effective_stale_threshold(),
        open_tool_call: prompts.effective_tool_call_stale_threshold(),
    }
}

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
        thresholds: PromptStaleThresholds,
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
                sweep_stalled_prompts(&state, thresholds).await;
                sweep_idle_sessions(&state, session_idle_threshold).await;
                sweep_session_tombstones(&state).await;
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

async fn sweep_stalled_prompts(
    state: &Arc<TokioMutex<StateStore>>,
    thresholds: PromptStaleThresholds,
) {
    // One guard covers the flip and its events: a prompt task waiting on the
    // lock could otherwise replace the stall and log `prompt.stall_resolved`
    // before the `prompt.stalled` row it resolves.
    let guard = state.lock().await;
    let stalled = match guard.mark_stalled_prompts(thresholds, SWEEPER_STALL_REASON) {
        Ok(stalled) => stalled,
        Err(err) => {
            tracing::warn!(
                error = %err,
                "state sweeper: mark_stalled_prompts failed"
            );
            return;
        }
    };
    for prompt in stalled {
        let payload = stalled_event_payload(&prompt.prompt_id, prompt.threshold.as_secs());
        if let Err(err) = guard.append_session_event_with_source(
            &prompt.session_id,
            "warn",
            EVENT_KIND_PROMPT_STALLED,
            EVENT_SOURCE_SYSTEM,
            "prompt stalled",
            &payload,
        ) {
            tracing::warn!(
                error = %err,
                prompt_id = %prompt.prompt_id,
                session_id = %prompt.session_id,
                "state sweeper: failed to append prompt.stalled event"
            );
        }
    }
}

/// `cause` mirrors the `error_message` the sweep wrote onto the prompt row, so
/// a transcript reads the stall reason off the event alone.
fn stalled_event_payload(prompt_id: &str, threshold_secs: u64) -> String {
    serde_json::json!({
        "prompt_id": prompt_id,
        "threshold_secs": threshold_secs,
        "cause": SWEEPER_STALL_REASON,
    })
    .to_string()
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

async fn sweep_session_tombstones(state: &Arc<TokioMutex<StateStore>>) {
    let guard = state.lock().await;
    match guard.prune_session_tombstones(SESSION_TOMBSTONE_RETENTION) {
        Ok(0) => {}
        Ok(pruned) => tracing::info!(pruned, "state sweeper: pruned expired session tombstones"),
        Err(err) => tracing::warn!(
            error = %err,
            "state sweeper: prune_session_tombstones failed"
        ),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stalled_event_payload_names_the_stall_cause() {
        let payload: serde_json::Value =
            serde_json::from_str(&stalled_event_payload("prm_1", 900)).expect("payload is json");

        assert_eq!(payload["prompt_id"], "prm_1");
        assert_eq!(payload["threshold_secs"], 900);
        assert_eq!(payload["cause"], SWEEPER_STALL_REASON);
    }
}
