//! Runtime supervisor lifecycle hooks.
//!
//! In 0.0.1 the supervisor only records its own start/stop transitions into
//! `agent_lifecycle`. The agent process supervision lives in a later batch;
//! this module is the named home it will land in.
//!
//! Events use `event_kind`:
//! - `server.starting` — config + state opened, about to bind.
//! - `server.started`  — listener is bound; the bound address is in payload.
//! - `server.stopped`  — graceful shutdown completed; elapsed wall time is in payload.
//!
//! All three are tied to one `acps serve` process. Payloads are small JSON
//! objects so operators can grep on bind addresses or restart loops.
//!
//! `record_*` helpers come in two flavors: sync (`&StateStore`) for use before
//! the store is moved into `AppState`, and async (`&Arc<Mutex<StateStore>>`)
//! for use after, where a brief lock acquires the connection.

use std::sync::Arc;
use std::time::Instant;

use serde_json::json;
use tokio::sync::Mutex as TokioMutex;

use crate::error::Result;
use crate::state::StateStore;

pub struct ServerLifecycle {
    started_at: Instant,
}

impl ServerLifecycle {
    /// Record `server.starting` while the store is still a direct handle, then
    /// hand back a lifecycle handle that tracks elapsed wall time for the
    /// `server.stopped` payload.
    pub fn starting(state: &StateStore, bind: &str) -> Result<Self> {
        let payload = json!({ "bind": bind }).to_string();
        state.append_agent_lifecycle("server.starting", "acps serve starting", &payload)?;
        Ok(Self {
            started_at: Instant::now(),
        })
    }

    /// Record `server.started` after the listener is bound. Async-aware so the
    /// caller can hold the same `Arc<Mutex<StateStore>>` it later hands to
    /// axum handlers.
    pub async fn started(&self, state: &Arc<TokioMutex<StateStore>>, bind: &str) -> Result<()> {
        let payload = json!({ "bind": bind }).to_string();
        let guard = state.lock().await;
        guard.append_agent_lifecycle("server.started", "acps serve listening", &payload)?;
        Ok(())
    }

    /// Record `server.stopped` with elapsed wall time. Called from the shutdown
    /// arm after axum's graceful-shutdown future resolves.
    pub async fn stopped(&self, state: &Arc<TokioMutex<StateStore>>, reason: &str) -> Result<()> {
        let elapsed_ms = u64::try_from(self.started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
        let payload = json!({ "reason": reason, "elapsed_ms": elapsed_ms }).to_string();
        let guard = state.lock().await;
        guard.append_agent_lifecycle("server.stopped", "acps serve stopped", &payload)?;
        Ok(())
    }
}
