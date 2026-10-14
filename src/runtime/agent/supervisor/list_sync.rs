//! ACP `session/list` sync into local state, run in the background per running bridge so
//! `GET /v1/sessions` reads rows without an agent round trip or a commit.

use super::*;
use chrono::{SecondsFormat, Utc};
use std::sync::Weak;

/// Single-flight gate plus the last finished sync, shared by the background loop and
/// forced syncs.
#[derive(Default)]
pub(super) struct ListSyncShared {
    /// Held across one `session/list` call and its write, so syncs never overlap.
    gate: TokioMutex<()>,
    last: RwLock<Option<ListSyncReport>>,
}

#[derive(Clone)]
struct ListSyncReport {
    /// The bridge the sync listed, so a report from a replaced agent is never shown for
    /// the current one. Weak, so it neither keeps a stopped bridge alive nor lets its
    /// address be reused by the next one.
    bridge: Weak<AcpBridge>,
    upserted: u32,
    updated: u32,
    synced_at: String,
}

/// Where a sync records one bridge's sessions.
struct ListSyncTarget {
    target_id: String,
    workspace_root: String,
    state: StateHandle,
}

/// Sync the bridge's session list now, then every `list_sync_interval`, until the bridge
/// is no longer the supervisor's running one.
pub(super) fn spawn_list_sync_loop(
    shared: SupervisorShared,
    bridge: &Arc<AcpBridge>,
    context: &RestartContext,
) {
    let bridge = Arc::downgrade(bridge);
    let interval = context.list_sync_interval;
    let target = ListSyncTarget {
        target_id: context.target_id.clone(),
        workspace_root: context.workspace_root.clone(),
        state: context.state_store.clone(),
    };
    tokio::spawn(async move {
        run_list_sync_loop(shared, bridge, target, interval).await;
    });
}

async fn run_list_sync_loop(
    shared: SupervisorShared,
    bridge: Weak<AcpBridge>,
    target: ListSyncTarget,
    interval: Duration,
) {
    loop {
        let Some(bridge) = bridge.upgrade() else {
            return;
        };
        let current = matches!(
            &*shared.state.lock().await,
            AgentState::Running(running) if Arc::ptr_eq(running, &bridge)
        );
        if !current || !bridge.capabilities().supports_list_sessions() {
            return;
        }
        if let Err(error) = sync_bridge_sessions(&shared.list_sync, &bridge, &target).await {
            tracing::warn!(
                error = %crate::error::report(&error),
                target_id = %target.target_id,
                "background session list sync failed; keeping the previous result"
            );
        }
        drop(bridge);
        tokio::time::sleep(interval).await;
    }
}

/// List the bridge's sessions and record them, under the single-flight gate. Rows carry
/// the agent id the bridge was spawned with, so every sync of one bridge agrees.
async fn sync_bridge_sessions(
    list_sync: &ListSyncShared,
    bridge: &Arc<AcpBridge>,
    target: &ListSyncTarget,
) -> Result<ListSyncReport> {
    let _gate = list_sync.gate.lock().await;
    let sessions = bridge.list_sessions().await?;
    let agent_id = bridge.agent_id().to_owned();
    let mut skipped_invalid_cwd = 0_u32;
    let mut records = Vec::new();
    for session in sessions {
        let raw_cwd = session.cwd.to_string_lossy().into_owned();
        let cwd = match resolve_session_cwd(Some(raw_cwd.clone()), &target.workspace_root) {
            Ok(cwd) => cwd,
            Err(err) => {
                skipped_invalid_cwd += 1;
                tracing::warn!(
                    error = %err,
                    session_id = %session.session_id.0,
                    cwd = %raw_cwd,
                    "skipping ACP-listed session with invalid cwd"
                );
                continue;
            }
        };
        let agent_session_id = session.session_id.0.to_string();
        let updated_at = session.updated_at.clone();
        let metadata_json = serde_json::json!({
            "source": "agent_list",
            "agent_session_id": &agent_session_id,
            "agent_updated_at": &updated_at,
            "agent_meta": session.meta,
        })
        .to_string();
        records.push(ListedSessionRecord {
            id: next_session_id(),
            agent_session_id,
            agent_id: agent_id.clone(),
            cwd,
            title: session.title,
            updated_at: session.updated_at,
            metadata_json,
        });
    }
    let target_id = target.target_id.clone();
    let counts = target
        .state
        .write(move |store| {
            store.record_session_list_sync(&target_id, &agent_id, records, skipped_invalid_cwd)
        })
        .await?;
    let report = ListSyncReport {
        bridge: Arc::downgrade(bridge),
        upserted: counts.upserted,
        updated: counts.updated,
        synced_at: Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true),
    };
    *list_sync.last.write().await = Some(report.clone());
    Ok(report)
}

impl AgentSupervisor {
    /// The running bridge when it can list sessions, or the status that says why not.
    async fn listing_bridge(&self) -> std::result::Result<Arc<AcpBridge>, SessionListSyncResult> {
        let bridge = match &*self.state.lock().await {
            AgentState::Running(bridge) => Arc::clone(bridge),
            AgentState::Stopped
            | AgentState::Starting
            | AgentState::Stopping
            | AgentState::Updating => {
                return Err(SessionListSyncResult::without_sync(
                    SessionListSyncStatus::NotRunning,
                ));
            }
        };
        if !bridge.capabilities().supports_list_sessions() {
            return Err(SessionListSyncResult::without_sync(
                SessionListSyncStatus::Unsupported,
            ));
        }
        Ok(bridge)
    }

    /// Sync ACP `session/list` results into local state now; discovery only, rows land
    /// `available`. A sync already in flight finishes first, so the result always
    /// reflects a listing that started after this call.
    pub async fn sync_listed_sessions(
        &self,
        target_id: &str,
        workspace_root: &str,
        state: &StateHandle,
    ) -> Result<SessionListSyncResult> {
        let bridge = match self.listing_bridge().await {
            Ok(bridge) => bridge,
            Err(result) => return Ok(result),
        };
        let target = ListSyncTarget {
            target_id: target_id.to_owned(),
            workspace_root: workspace_root.to_owned(),
            state: state.clone(),
        };
        let report = sync_bridge_sessions(&self.list_sync, &bridge, &target).await?;
        Ok(SessionListSyncResult {
            attempted: true,
            status: SessionListSyncStatus::Synced,
            upserted: report.upserted,
            updated: report.updated,
            synced_at: Some(report.synced_at),
        })
    }

    /// The running agent's last successful sync, background or forced, without listing.
    pub async fn last_listed_sessions_sync(&self) -> SessionListSyncResult {
        let bridge = match self.listing_bridge().await {
            Ok(bridge) => bridge,
            Err(result) => return result,
        };
        match &*self.list_sync.last.read().await {
            Some(report) if std::ptr::eq(report.bridge.as_ptr(), Arc::as_ptr(&bridge)) => {
                SessionListSyncResult {
                    attempted: false,
                    status: SessionListSyncStatus::Cached,
                    upserted: report.upserted,
                    updated: report.updated,
                    synced_at: Some(report.synced_at.clone()),
                }
            }
            _ => SessionListSyncResult::without_sync(SessionListSyncStatus::Pending),
        }
    }
}
