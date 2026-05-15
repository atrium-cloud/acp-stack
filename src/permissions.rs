//! Durable permission pipeline.
//!
//! `PermissionService` is the single funnel for permission requests originating
//! from either the Command Gateway (`commands.rs`) or the ACP bridge
//! (`acp_bridge.rs`). It:
//!
//!   * Persists each request as a `permission_requests` row in the `pending`
//!     state.
//!   * Holds an in-memory waiter (`tokio::sync::oneshot`) keyed by request id,
//!     plus any source-specific sensitive payload (e.g. raw command env
//!     values) that must NOT enter the durable row.
//!   * Resolves the waiter when the request is approved, denied, canceled, or
//!     expires after `[permissions].request_timeout`.
//!   * Records the decision as a `permission_decisions` row and publishes a
//!     `permission.*` event on both the `permissions` and `logs` WebSocket
//!     topics.
//!
//! Tier: pending reads + approve/deny decisions are session-tier per
//! `docs/specs/security.md:20`. The durability is in SQLite; the in-memory
//! state is purely the waiter map. On daemon restart `reconcile_orphaned_permissions`
//! marks pending command rows `expired` and pending ACP rows `canceled` so
//! clients never see them stay pending forever.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::{Mutex as TokioMutex, oneshot};

use crate::config::PermissionTimeoutAction;
use crate::error::{Result, StackError};
use crate::events::EventHub;
use crate::state::{NewPermissionRequest, PermissionRequestRecord, PermissionStatus, StateStore};

/// Source of a permission request. ACP-source requests originate from a
/// pass-through `session/request_permission`; command-source requests come
/// from the Command Gateway's `review` / `locked` policy hits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionSource {
    Command,
    Acp,
}

impl PermissionSource {
    pub fn as_str(self) -> &'static str {
        match self {
            PermissionSource::Command => "command",
            PermissionSource::Acp => "acp",
        }
    }
}

/// Inputs the caller supplies to `request`. `detail` MUST be redacted: secret
/// values, raw env values, and other sensitive material should be kept in
/// `sensitive_payload` instead.
#[derive(Debug, Clone)]
pub struct NewPermission {
    pub source: PermissionSource,
    pub requester: Option<String>,
    pub subject_id: Option<String>,
    pub detail: Value,
}

/// Outcome the waiter receives. `option_id` mirrors the ACP request envelope:
/// approval can select a specific option (e.g. for tool-use prompts), and
/// `PermissionService` does not interpret it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionOutcome {
    Approved {
        option_id: Option<String>,
        reason: Option<String>,
    },
    Denied {
        reason: Option<String>,
    },
    Canceled {
        reason: String,
    },
    Expired,
}

impl PermissionOutcome {
    pub fn as_status(&self) -> PermissionStatus {
        match self {
            PermissionOutcome::Approved { .. } => PermissionStatus::Approved,
            PermissionOutcome::Denied { .. } => PermissionStatus::Denied,
            PermissionOutcome::Canceled { .. } => PermissionStatus::Canceled,
            PermissionOutcome::Expired => PermissionStatus::Expired,
        }
    }
}

/// Public view of a permission request, suitable for the HTTP API. The
/// `detail` field is the parsed JSON of the durable `detail_json` column.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PermissionRequestView {
    pub id: String,
    pub created_at: String,
    pub updated_at: String,
    pub status: String,
    pub source: String,
    pub requester: Option<String>,
    pub subject_id: Option<String>,
    pub detail: Value,
    pub expires_at: Option<String>,
}

impl PermissionRequestView {
    /// `detail_json` is guarded by a `json_valid()` CHECK constraint
    /// (migration 006), so a parse failure here means the row was either
    /// written outside our codepath or is corrupted on disk. We surface
    /// that as a typed error rather than masking it with `Value::Null`,
    /// which would silently feed an empty detail to operators approving
    /// the request.
    pub fn from_record(record: PermissionRequestRecord) -> Result<Self> {
        let detail = serde_json::from_str(&record.detail_json).map_err(|err| {
            tracing::warn!(error = %err, perm_id = %record.id, "permission detail_json is not valid JSON");
            StackError::StateInvalidJson {
                field: "permission_requests.detail_json",
                reason: err.to_string(),
            }
        })?;
        Ok(Self {
            id: record.id,
            created_at: record.created_at,
            updated_at: record.updated_at,
            status: record.status,
            source: record.source,
            requester: record.requester,
            subject_id: record.subject_id,
            detail,
            expires_at: record.expires_at,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PermissionDecisionView {
    pub id: String,
    pub request_id: String,
    pub created_at: String,
    pub decision: String,
    pub deciding_principal: Option<String>,
    pub reason: Option<String>,
}

struct PendingOp {
    waiter: oneshot::Sender<PermissionOutcome>,
}

#[derive(Clone)]
pub struct PermissionService {
    state: Arc<TokioMutex<StateStore>>,
    events: EventHub,
    pending: Arc<TokioMutex<HashMap<String, PendingOp>>>,
    timeout: Duration,
    timeout_action: PermissionTimeoutAction,
}

impl PermissionService {
    pub fn new(
        state: Arc<TokioMutex<StateStore>>,
        events: EventHub,
        timeout: Duration,
        timeout_action: PermissionTimeoutAction,
    ) -> Self {
        Self {
            state,
            events,
            pending: Arc::new(TokioMutex::new(HashMap::new())),
            timeout,
            timeout_action,
        }
    }

    /// Create a new permission row, register a waiter, and schedule the timer.
    /// Returns the freshly-inserted record and a receiver that resolves when
    /// the request is decided, canceled, or times out.
    pub async fn request(
        &self,
        input: NewPermission,
    ) -> Result<(
        PermissionRequestRecord,
        oneshot::Receiver<PermissionOutcome>,
    )> {
        let expires_at = compute_expiry(self.timeout);
        let detail_json = serde_json::to_string(&input.detail).map_err(|err| {
            tracing::error!(error = %err, "failed to serialize permission detail JSON");
            StackError::StateInvalidJson {
                field: "permission_requests.detail_json",
                reason: err.to_string(),
            }
        })?;
        let record = {
            let state = self.state.lock().await;
            state.append_permission_request(NewPermissionRequest {
                source: input.source.as_str(),
                requester: input.requester.as_deref(),
                subject_id: input.subject_id.as_deref(),
                detail_json: &detail_json,
                expires_at: expires_at.as_deref(),
            })?
        };

        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            pending.insert(record.id.clone(), PendingOp { waiter: tx });
        }

        self.publish_event(
            &record.id,
            &record.created_at,
            "permission.created",
            json!({
                "id": record.id,
                "source": record.source,
                "subject_id": record.subject_id,
                "expires_at": record.expires_at,
            }),
        )
        .await;

        self.spawn_timer(record.id.clone());

        Ok((record, rx))
    }

    /// Approve a pending request. Returns the persisted decision view.
    pub async fn approve(
        &self,
        id: &str,
        option_id: Option<String>,
        reason: Option<String>,
        deciding_principal: &str,
    ) -> Result<PermissionDecisionView> {
        let outcome = PermissionOutcome::Approved {
            option_id,
            reason: reason.clone(),
        };
        self.resolve(id, outcome, deciding_principal, reason).await
    }

    pub async fn deny(
        &self,
        id: &str,
        reason: Option<String>,
        deciding_principal: &str,
    ) -> Result<PermissionDecisionView> {
        let outcome = PermissionOutcome::Denied {
            reason: reason.clone(),
        };
        self.resolve(id, outcome, deciding_principal, reason).await
    }

    pub async fn cancel(&self, id: &str, reason: &str) -> Result<()> {
        let outcome = PermissionOutcome::Canceled {
            reason: reason.to_owned(),
        };
        self.resolve(id, outcome, "system", Some(reason.to_owned()))
            .await
            .map(|_| ())
    }

    pub async fn pending(&self, limit: u32) -> Result<Vec<PermissionRequestView>> {
        let state = self.state.lock().await;
        let rows = state.query_pending_permissions(limit)?;
        rows.into_iter()
            .map(PermissionRequestView::from_record)
            .collect()
    }

    pub async fn get(&self, id: &str) -> Result<PermissionRequestView> {
        let state = self.state.lock().await;
        let record = state
            .get_permission_request(id)?
            .ok_or_else(|| StackError::PermissionNotFound { id: id.to_owned() })?;
        PermissionRequestView::from_record(record)
    }

    async fn resolve(
        &self,
        id: &str,
        outcome: PermissionOutcome,
        deciding_principal: &str,
        reason: Option<String>,
    ) -> Result<PermissionDecisionView> {
        let new_status = outcome.as_status();
        let decision = {
            let state = self.state.lock().await;
            state.decide_permission(id, new_status, Some(deciding_principal), reason.as_deref())?
        };

        // Take the waiter (if any) and fire it. A missing waiter means the
        // timer already fired or the daemon was restarted between request and
        // decision — still record the durable decision row so the audit trail
        // is complete.
        if let Some(op) = self.pending.lock().await.remove(id) {
            let _ = op.waiter.send(outcome.clone());
        }

        let kind = match outcome {
            PermissionOutcome::Approved { .. } => "permission.approved",
            PermissionOutcome::Denied { .. } => "permission.denied",
            PermissionOutcome::Canceled { .. } => "permission.canceled",
            PermissionOutcome::Expired => "permission.expired",
        };
        self.publish_event(
            id,
            &decision.created_at,
            kind,
            json!({
                "id": id,
                "decision": decision.decision,
                "deciding_principal": decision.deciding_principal,
                "reason": decision.reason,
            }),
        )
        .await;

        Ok(PermissionDecisionView {
            id: decision.id,
            request_id: decision.request_id,
            created_at: decision.created_at,
            decision: decision.decision,
            deciding_principal: decision.deciding_principal,
            reason: decision.reason,
        })
    }

    fn spawn_timer(&self, id: String) {
        let timeout = self.timeout;
        let action = self.timeout_action;
        let state = Arc::clone(&self.state);
        let pending = Arc::clone(&self.pending);
        let events = self.events.clone();
        tokio::spawn(async move {
            tokio::time::sleep(timeout).await;

            let (new_status, outcome, kind) = match action {
                PermissionTimeoutAction::Deny => (
                    PermissionStatus::Expired,
                    PermissionOutcome::Expired,
                    "permission.expired",
                ),
                PermissionTimeoutAction::Approve => (
                    PermissionStatus::Approved,
                    PermissionOutcome::Approved {
                        option_id: None,
                        reason: Some("auto-approved on timeout".to_owned()),
                    },
                    "permission.approved",
                ),
            };

            // Use the atomic decide_permission so a concurrent approve/deny
            // cannot land between the transition and the decision row. If the
            // row was already decided by another caller, that caller's `resolve`
            // has fired the waiter; we exit without writing.
            let outcome_for_waiter = outcome.clone();
            let result = {
                let store = state.lock().await;
                match store.decide_permission(&id, new_status, Some("system"), Some("timeout")) {
                    Ok(decision) => Ok(Some(decision.created_at)),
                    Err(StackError::InvalidPermissionTransition { .. }) => Ok(None),
                    Err(err) => Err(err),
                }
            };

            let now = match result {
                Ok(Some(created_at)) => created_at,
                Ok(None) => {
                    // Concurrent decision settled the row first. The decider
                    // has already fired the waiter; we have nothing to do.
                    return;
                }
                Err(err) => {
                    // A real state error: drop the waiter from the map and
                    // fire it with Expired so the caller doesn't hang. The
                    // row stays pending in SQLite; reconcile_orphaned_permissions
                    // will sweep it on next daemon restart.
                    tracing::warn!(error = %err, perm_id = %id, "timer transition/decision failed");
                    if let Some(op) = pending.lock().await.remove(&id) {
                        let _ = op.waiter.send(PermissionOutcome::Expired);
                    }
                    return;
                }
            };

            if let Some(op) = pending.lock().await.remove(&id) {
                let _ = op.waiter.send(outcome_for_waiter);
            }

            persist_and_publish_permission_event(
                &state,
                &events,
                &id,
                &now,
                kind,
                json!({
                    "id": id,
                    "decision": new_status.as_str(),
                    "deciding_principal": "system",
                    "reason": "timeout",
                }),
            )
            .await;
        });
    }

    async fn publish_event(&self, id: &str, created_at: &str, kind: &str, data: Value) {
        persist_and_publish_permission_event(&self.state, &self.events, id, created_at, kind, data)
            .await;
    }
}

/// Append a durable `events` row AND publish the live `permissions` topic
/// envelope for a permission lifecycle event. Two side effects so callers
/// don't drift apart: every WS-visible event lands in `events` (so
/// `GET /v1/logs/permissions` returns it), and every durable event is
/// fanned out live (so subscribers see it immediately). The append_event
/// helper already fans out to the `logs` topic, so each lifecycle event
/// reaches `logs` AND `permissions` subscribers.
async fn persist_and_publish_permission_event(
    state: &Arc<TokioMutex<StateStore>>,
    events: &EventHub,
    id: &str,
    created_at: &str,
    kind: &str,
    data: Value,
) {
    let payload_text = match serde_json::to_string(&data) {
        Ok(text) => text,
        Err(err) => {
            tracing::warn!(
                error = %err,
                perm_id = id,
                kind,
                "failed to serialize permission event payload",
            );
            return;
        }
    };
    let message = match kind {
        "permission.created" => "permission requested",
        "permission.approved" => "permission approved",
        "permission.denied" => "permission denied",
        "permission.canceled" => "permission canceled",
        "permission.expired" => "permission expired",
        _ => "permission event",
    };
    {
        let store = state.lock().await;
        if let Err(err) = store.append_event("info", kind, message, &payload_text) {
            tracing::warn!(
                error = %err,
                perm_id = id,
                kind,
                "failed to append permission event to events table",
            );
        }
    }
    events.publish_permission_event(id, created_at, kind, data);
}

fn compute_expiry(timeout: Duration) -> Option<String> {
    if timeout.is_zero() {
        return None;
    }
    // Use millisecond precision so sub-second timeouts (test fixtures, future
    // fast-cycle UI) still write a non-NULL expires_at. `as_millis()` returns
    // u128 but real-world timeouts fit in i64 ms easily.
    let millis = i64::try_from(timeout.as_millis()).unwrap_or(i64::MAX);
    let now = chrono::Utc::now();
    let expires = now + chrono::Duration::milliseconds(millis);
    Some(expires.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_service(action: PermissionTimeoutAction) -> (tempfile::TempDir, PermissionService) {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = tempdir.path().join("state.sqlite");
        let store = StateStore::open(&path).expect("open");
        store.migrate().expect("migrate");
        let state = Arc::new(TokioMutex::new(store));
        let events = EventHub::new();
        let service = PermissionService::new(state, events, Duration::from_millis(60), action);
        (tempdir, service)
    }

    #[tokio::test]
    async fn request_then_approve_resolves_waiter() {
        let (_dir, service) = fresh_service(PermissionTimeoutAction::Deny);
        let (record, rx) = service
            .request(NewPermission {
                source: PermissionSource::Command,
                requester: Some("cmd_a".to_owned()),
                subject_id: Some("cmd_a".to_owned()),
                detail: json!({ "command": "echo hi" }),
            })
            .await
            .expect("request");
        assert_eq!(record.status, "pending");

        service
            .approve(&record.id, Some("ok".to_owned()), None, "session-key")
            .await
            .expect("approve");

        let outcome = rx.await.expect("recv");
        assert!(matches!(
            outcome,
            PermissionOutcome::Approved { option_id: Some(opt), .. } if opt == "ok"
        ));
    }

    #[tokio::test]
    async fn request_then_deny_resolves_waiter() {
        let (_dir, service) = fresh_service(PermissionTimeoutAction::Deny);
        let (record, rx) = service
            .request(NewPermission {
                source: PermissionSource::Acp,
                requester: Some("sess_a".to_owned()),
                subject_id: Some("sess_a".to_owned()),
                detail: json!({}),
            })
            .await
            .expect("request");

        service
            .deny(&record.id, Some("no".to_owned()), "session-key")
            .await
            .expect("deny");

        let outcome = rx.await.expect("recv");
        assert!(matches!(outcome, PermissionOutcome::Denied { reason: Some(r) } if r == "no"));
    }

    #[tokio::test]
    async fn timer_expires_pending_request_when_action_is_deny() {
        let (_dir, service) = fresh_service(PermissionTimeoutAction::Deny);
        let (record, rx) = service
            .request(NewPermission {
                source: PermissionSource::Command,
                requester: None,
                subject_id: None,
                detail: json!({}),
            })
            .await
            .expect("request");
        let outcome = tokio::time::timeout(Duration::from_secs(2), rx)
            .await
            .expect("must fire")
            .expect("recv");
        assert!(matches!(outcome, PermissionOutcome::Expired));

        let view = service.get(&record.id).await.expect("get");
        assert_eq!(view.status, "expired");
    }

    #[tokio::test]
    async fn double_approve_rejected_with_transition_error() {
        let (_dir, service) = fresh_service(PermissionTimeoutAction::Deny);
        let (record, _rx) = service
            .request(NewPermission {
                source: PermissionSource::Command,
                requester: None,
                subject_id: None,
                detail: json!({}),
            })
            .await
            .expect("request");
        service
            .approve(&record.id, None, None, "session-key")
            .await
            .expect("first");
        let error = service
            .approve(&record.id, None, None, "session-key")
            .await
            .expect_err("second must fail");
        assert!(error.to_string().contains("cannot transition"), "{error}");
    }

    #[tokio::test]
    async fn cancel_settles_waiter() {
        let (_dir, service) = fresh_service(PermissionTimeoutAction::Deny);
        let (record, rx) = service
            .request(NewPermission {
                source: PermissionSource::Acp,
                requester: None,
                subject_id: None,
                detail: json!({}),
            })
            .await
            .expect("request");
        service
            .cancel(&record.id, "session-closed")
            .await
            .expect("cancel");
        let outcome = rx.await.expect("recv");
        assert!(matches!(outcome, PermissionOutcome::Canceled { .. }));
    }
}
