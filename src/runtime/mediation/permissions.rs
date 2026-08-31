//! Durable permission pipeline: `PermissionService` is the single funnel for
//! Command Gateway and ACP bridge permission requests, persisting each as a
//! `permission_requests` row backed by an in-memory oneshot waiter.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::{Mutex as TokioMutex, oneshot};

use crate::config::{AcpPromptAction, PermissionTimeoutAction};
use crate::error::{Result, StackError};
use crate::events::EventHub;
use crate::state::{
    NewPermissionRequest, PermissionDecisionRecord, PermissionRequestRecord, PermissionStatus,
    StateStore,
};

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
#[derive(Debug, Clone, Serialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct PermissionRequestView {
    pub id: String,
    pub created_at: String,
    pub updated_at: String,
    #[schemars(extend("enum" = ["pending", "approved", "denied", "expired", "cancelled"]))]
    pub status: String,
    #[schemars(extend("enum" = ["command", "acp"]))]
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

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct PermissionDecisionView {
    pub id: String,
    pub request_id: String,
    pub created_at: String,
    /// The settling status; never `pending`.
    #[schemars(extend("enum" = ["approved", "denied", "expired", "cancelled"]))]
    pub decision: String,
    pub deciding_principal: Option<String>,
    pub reason: Option<String>,
}

struct PendingOp {
    waiter: oneshot::Sender<PermissionOutcome>,
}

/// Rows read when settling one session's pending requests. The query is scoped
/// to that session, so this bound only guards a runaway agent holding more
/// outstanding requests than an operator could ever answer.
const SESSION_PENDING_PERMISSION_SCAN_LIMIT: u32 = 1000;

/// Reason recorded on the waiter when the durable row was already decided
/// before the waiter existed. The decision row keeps the real deciding
/// principal and reason; this only labels the late hand-off.
const RACED_DECISION_REASON: &str = "decided before the waiter was registered";

/// Deciding principal and reason recorded on a request the configured
/// `acp_prompt_action` answered, distinguishing it in the audit trail from an
/// operator decision and from a timeout.
const POLICY_DECIDING_PRINCIPAL: &str = "policy";
const POLICY_APPROVAL_REASON: &str = "auto-approved by policy";

#[derive(Clone)]
pub struct PermissionService {
    state: Arc<TokioMutex<StateStore>>,
    events: EventHub,
    pending: Arc<TokioMutex<HashMap<String, PendingOp>>>,
    timeout: Duration,
    timeout_action: PermissionTimeoutAction,
    acp_prompt_action: AcpPromptAction,
}

impl PermissionService {
    pub fn new(
        state: Arc<TokioMutex<StateStore>>,
        events: EventHub,
        timeout: Duration,
        timeout_action: PermissionTimeoutAction,
        acp_prompt_action: AcpPromptAction,
    ) -> Self {
        Self {
            state,
            events,
            pending: Arc::new(TokioMutex::new(HashMap::new())),
            timeout,
            timeout_action,
            acp_prompt_action,
        }
    }

    /// How agent-raised requests are answered. Mediated command requests always
    /// ask, whatever this says.
    pub fn acp_prompt_action(&self) -> AcpPromptAction {
        self.acp_prompt_action
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
        let record = self.insert_request(&input, expires_at.as_deref()).await?;

        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            pending.insert(record.id.clone(), PendingOp { waiter: tx });
        }
        // The row is durable before the waiter exists, so a decider running in
        // that window (the session-cancel sweep polls several times a second)
        // settles it and finds nothing to fire. Re-read the row and answer the
        // waiter here, or the caller would await a decision nobody can send.
        self.publish_created_event(&record).await;

        // Published first so the created event precedes any decision event the
        // settle below fires for a request raced inside the waiter window.
        self.settle_waiter_if_already_decided(&record.id).await;

        self.spawn_timer(record.id.clone());

        Ok((record, rx))
    }

    /// Record a request that policy answers on arrival: the row is inserted
    /// already approved in one transaction, so it is never observable as
    /// pending and a midway failure writes nothing. No waiter and no expiry
    /// timer, because nothing is ever left outstanding to decide or expire.
    pub async fn approve_by_policy(
        &self,
        input: NewPermission,
    ) -> Result<(PermissionRequestRecord, PermissionDecisionView)> {
        let detail_json = serialize_detail(&input.detail)?;
        let (record, decision) = {
            let state = self.state.lock().await;
            state.append_approved_permission_request(
                NewPermissionRequest {
                    source: input.source.as_str(),
                    requester: input.requester.as_deref(),
                    subject_id: input.subject_id.as_deref(),
                    detail_json: &detail_json,
                    expires_at: None,
                },
                Some(POLICY_DECIDING_PRINCIPAL),
                Some(POLICY_APPROVAL_REASON),
            )?
        };
        self.publish_created_event(&record).await;
        self.publish_decision_event(&record.id, &decision, "permission.approved")
            .await;
        Ok((record, decision_view(decision)))
    }

    async fn insert_request(
        &self,
        input: &NewPermission,
        expires_at: Option<&str>,
    ) -> Result<PermissionRequestRecord> {
        let detail_json = serialize_detail(&input.detail)?;
        let state = self.state.lock().await;
        state.append_permission_request(NewPermissionRequest {
            source: input.source.as_str(),
            requester: input.requester.as_deref(),
            subject_id: input.subject_id.as_deref(),
            detail_json: &detail_json,
            expires_at,
        })
    }

    async fn publish_created_event(&self, record: &PermissionRequestRecord) {
        self.publish_event(
            &record.id,
            &record.created_at,
            "permission.created",
            json!({
                // `permission_id` is what log queries filter on; `id` stays for
                // clients that already key on it.
                "id": record.id,
                "permission_id": record.id,
                "source": record.source,
                "subject_id": record.subject_id,
                "expires_at": record.expires_at,
            }),
        )
        .await;
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

    /// Cancel only while the durable request is still pending. Returns false
    /// when another decider won the atomic state transition first.
    pub async fn cancel_if_pending(&self, id: &str, reason: &str) -> Result<bool> {
        match self.cancel(id, reason).await {
            Ok(()) => Ok(true),
            Err(StackError::InvalidPermissionTransition { .. }) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Settle every pending ACP-source request raised by `session_id` as
    /// cancelled, returning how many this call actually settled. Requests
    /// another decider won in the meantime are skipped, so the sweep is safe to
    /// repeat while waiting out a cancel.
    pub async fn cancel_pending_for_session(
        &self,
        session_id: &str,
        reason: &str,
    ) -> Result<usize> {
        let pending = {
            let state = self.state.lock().await;
            state.query_pending_permissions_for_subject(
                PermissionSource::Acp.as_str(),
                session_id,
                SESSION_PENDING_PERMISSION_SCAN_LIMIT,
            )?
        };
        let mut settled = 0;
        for row in pending {
            if self.cancel_if_pending(&row.id, reason).await? {
                settled += 1;
            }
        }
        Ok(settled)
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

    /// Fire the waiter for a request that was already decided before its waiter
    /// was registered. A no-op on the ordinary path, where the row is still
    /// pending and the decider that comes later fires the waiter itself.
    async fn settle_waiter_if_already_decided(&self, id: &str) {
        let status = {
            let state = self.state.lock().await;
            match state.get_permission_request(id) {
                Ok(Some(record)) => Some(PermissionStatus::from_wire(&record.status)),
                Ok(None) => {
                    tracing::warn!(permission_id = %id, "fresh permission request missing on re-read");
                    None
                }
                Err(error) => {
                    tracing::warn!(error = %error, permission_id = %id, "failed to re-read a fresh permission request");
                    None
                }
            }
        };
        let Some(status) = status else {
            // The same fallback the expiry timer uses for a failed transition:
            // answer the waiter so the caller cannot hang. Any durable row
            // stays pending for the real decision path, and its timer still
            // spawns because the caller continues past this settle.
            if let Some(op) = self.pending.lock().await.remove(id) {
                let _ = op.waiter.send(PermissionOutcome::Expired);
            }
            return;
        };
        let outcome = match status {
            PermissionStatus::Pending => return,
            // A raced approval's selected option is not recoverable from the
            // request row, and guessing one could deliver the approval as a
            // rejection; answering cancelled ends the turn without acting.
            PermissionStatus::Approved | PermissionStatus::Canceled => {
                PermissionOutcome::Canceled {
                    reason: RACED_DECISION_REASON.to_owned(),
                }
            }
            PermissionStatus::Denied => PermissionOutcome::Denied {
                reason: Some(RACED_DECISION_REASON.to_owned()),
            },
            PermissionStatus::Expired => PermissionOutcome::Expired,
        };
        if let Some(op) = self.pending.lock().await.remove(id) {
            let _ = op.waiter.send(outcome);
        }
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

        // A missing waiter means the timer already fired or the daemon
        // restarted; the durable decision row is still written for the audit
        // trail.
        if let Some(op) = self.pending.lock().await.remove(id) {
            let _ = op.waiter.send(outcome.clone());
        }

        let kind = match outcome {
            PermissionOutcome::Approved { .. } => "permission.approved",
            PermissionOutcome::Denied { .. } => "permission.denied",
            PermissionOutcome::Canceled { .. } => "permission.cancelled",
            PermissionOutcome::Expired => "permission.expired",
        };
        self.publish_decision_event(id, &decision, kind).await;

        Ok(decision_view(decision))
    }

    async fn publish_decision_event(
        &self,
        id: &str,
        decision: &PermissionDecisionRecord,
        kind: &str,
    ) {
        let mut payload = json!({
            "id": id,
            "permission_id": id,
            "decision": decision.decision,
            "deciding_principal": decision.deciding_principal,
            "reason": decision.reason,
        });
        link_request_subject(&self.state, id, &mut payload).await;
        self.publish_event(id, &decision.created_at, kind, payload)
            .await;
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

            // The atomic `decide_permission` keeps a concurrent approve/deny
            // from landing between the transition and the decision row.
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
                    // A concurrent decider settled the row and fired the waiter.
                    return;
                }
                Err(err) => {
                    // Fire the waiter with Expired so the caller cannot hang;
                    // the row stays pending for the restart sweep.
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

            let mut payload = json!({
                "id": id,
                "permission_id": id,
                "decision": new_status.as_str(),
                "deciding_principal": "system",
                "reason": "timeout",
            });
            link_request_subject(&state, &id, &mut payload).await;
            persist_and_publish_permission_event(&state, &events, &id, &now, kind, payload).await;
        });
    }

    async fn publish_event(&self, id: &str, created_at: &str, kind: &str, data: Value) {
        persist_and_publish_permission_event(&self.state, &self.events, id, created_at, kind, data)
            .await;
    }
}

/// Serialize a request's detail for the `detail_json` column. Shared by the
/// pending and decided-on-arrival insert paths.
fn serialize_detail(detail: &Value) -> Result<String> {
    serde_json::to_string(detail).map_err(|err| {
        tracing::error!(error = %err, "failed to serialize permission detail JSON");
        StackError::StateInvalidJson {
            field: "permission_requests.detail_json",
            reason: err.to_string(),
        }
    })
}

fn decision_view(decision: PermissionDecisionRecord) -> PermissionDecisionView {
    PermissionDecisionView {
        id: decision.id,
        request_id: decision.request_id,
        created_at: decision.created_at,
        decision: decision.decision,
        deciding_principal: decision.deciding_principal,
        reason: decision.reason,
    }
}

/// Extend a decision-event payload with the request's `source` / `subject_id`
/// and, for command-source rows, a `command_id` field. Log filters resolve
/// `command_id` via `json_extract(payload_json, '$.command_id')`, so without
/// this a client watching a command's event stream never sees why its
/// permission settled. ACP-source `subject_id` is a session id and must not
/// be presented as a command id. Best-effort: a failed read keeps the base
/// payload and logs.
async fn link_request_subject(state: &Arc<TokioMutex<StateStore>>, id: &str, payload: &mut Value) {
    let record = {
        let store = state.lock().await;
        store.get_permission_request(id)
    };
    match record {
        Ok(Some(record)) => {
            payload["source"] = json!(record.source);
            payload["subject_id"] = json!(record.subject_id);
            if record.source == PermissionSource::Command.as_str()
                && let Some(subject_id) = record.subject_id
            {
                payload["command_id"] = json!(subject_id);
            }
        }
        Ok(None) => {
            tracing::warn!(
                perm_id = id,
                "permission row missing while enriching decision event"
            );
        }
        Err(error) => {
            tracing::warn!(error = %error, perm_id = id, "failed to read permission row while enriching decision event");
        }
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
        "permission.cancelled" => "permission cancelled",
        "permission.expired" => "permission expired",
        _ => "permission event",
    };
    {
        let store = state.lock().await;
        if let Err(err) = store.append_event_with_source(
            "info",
            kind,
            crate::state::EVENT_SOURCE_PERMISSION,
            message,
            &payload_text,
        ) {
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
    // Millisecond precision so sub-second timeouts still write a non-NULL
    // expires_at.
    let millis = i64::try_from(timeout.as_millis()).unwrap_or(i64::MAX);
    let now = chrono::Utc::now();
    let expires = now + chrono::Duration::milliseconds(millis);
    Some(expires.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_service(action: PermissionTimeoutAction) -> (tempfile::TempDir, PermissionService) {
        fresh_service_with_timeout(action, Duration::from_millis(60))
    }

    fn fresh_service_with_timeout(
        action: PermissionTimeoutAction,
        timeout: Duration,
    ) -> (tempfile::TempDir, PermissionService) {
        fresh_service_with(action, timeout, AcpPromptAction::Ask)
    }

    fn fresh_service_with(
        action: PermissionTimeoutAction,
        timeout: Duration,
        acp_prompt_action: AcpPromptAction,
    ) -> (tempfile::TempDir, PermissionService) {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = tempdir.path().join("state.sqlite");
        let store = StateStore::open(&path).expect("open");
        store.migrate().expect("migrate");
        let state = Arc::new(TokioMutex::new(store));
        let events = EventHub::new();
        let service = PermissionService::new(state, events, timeout, action, acp_prompt_action);
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

    /// The durable row exists before its waiter is registered, so a decider
    /// running in that window (the session-cancel sweep polls several times a
    /// second) settles the row and finds no waiter to fire. The caller must
    /// still receive an outcome, or it parks forever on a decision that has
    /// already been made. Built by hand because the window is too narrow to
    /// hit reliably from concurrent callers.
    #[tokio::test]
    async fn a_request_decided_before_its_waiter_exists_still_answers_the_caller() {
        let (_dir, service) =
            fresh_service_with_timeout(PermissionTimeoutAction::Deny, Duration::from_secs(300));
        // Holding the waiter registry parks `request` exactly at the window: its
        // row is already durable, its waiter does not exist yet.
        let registry_guard = service.pending.lock().await;
        let requesting = tokio::spawn({
            let service = service.clone();
            async move {
                service
                    .request(NewPermission {
                        source: PermissionSource::Acp,
                        requester: Some("session:sess_race".to_owned()),
                        subject_id: Some("sess_race".to_owned()),
                        detail: json!({}),
                    })
                    .await
            }
        });

        let deadline = tokio::time::Instant::now() + RACED_WAITER_BUDGET;
        let id = loop {
            let pending = {
                let state = service.state.lock().await;
                state
                    .query_pending_permissions_for_subject(
                        PermissionSource::Acp.as_str(),
                        "sess_race",
                        SESSION_PENDING_PERMISSION_SCAN_LIMIT,
                    )
                    .expect("query pending")
            };
            if let Some(row) = pending.first() {
                break row.id.clone();
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the request never inserted its row"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        // Decide it durably with no waiter to fire, which is all a sweep can do
        // in this window.
        {
            let state = service.state.lock().await;
            state
                .decide_permission(
                    &id,
                    PermissionStatus::Canceled,
                    Some("system"),
                    Some("session-cancelled"),
                )
                .expect("decide");
        }
        drop(registry_guard);

        let (_record, receiver) = requesting.await.expect("join").expect("request");
        let outcome = tokio::time::timeout(RACED_WAITER_BUDGET, receiver)
            .await
            .expect("a request decided inside the window must still answer its caller")
            .expect("recv");
        assert!(
            matches!(outcome, PermissionOutcome::Canceled { .. }),
            "settled as {outcome:?}"
        );
        assert!(
            !service.pending.lock().await.contains_key(&id),
            "the fired waiter must be removed"
        );
    }

    /// A fired waiter resolves immediately; this only keeps a regression from
    /// hanging the suite.
    const RACED_WAITER_BUDGET: Duration = Duration::from_secs(5);

    #[tokio::test]
    async fn a_request_approved_before_its_waiter_exists_is_answered_cancelled() {
        let (_dir, service) =
            fresh_service_with_timeout(PermissionTimeoutAction::Deny, Duration::from_secs(300));
        // Same window as the cancel race above: row durable, waiter not yet
        // registered, but the decision that lands is an approval.
        let registry_guard = service.pending.lock().await;
        let requesting = tokio::spawn({
            let service = service.clone();
            async move {
                service
                    .request(NewPermission {
                        source: PermissionSource::Acp,
                        requester: Some("session:sess_race_approve".to_owned()),
                        subject_id: Some("sess_race_approve".to_owned()),
                        detail: json!({}),
                    })
                    .await
            }
        });

        let deadline = tokio::time::Instant::now() + RACED_WAITER_BUDGET;
        let id = loop {
            let pending = {
                let state = service.state.lock().await;
                state
                    .query_pending_permissions_for_subject(
                        PermissionSource::Acp.as_str(),
                        "sess_race_approve",
                        SESSION_PENDING_PERMISSION_SCAN_LIMIT,
                    )
                    .expect("query pending")
            };
            if let Some(row) = pending.first() {
                break row.id.clone();
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the request never inserted its row"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        {
            let state = service.state.lock().await;
            state
                .decide_permission(&id, PermissionStatus::Approved, Some("operator"), None)
                .expect("decide");
        }
        drop(registry_guard);

        let (_record, receiver) = requesting.await.expect("join").expect("request");
        let outcome = tokio::time::timeout(RACED_WAITER_BUDGET, receiver)
            .await
            .expect("a request approved inside the window must still answer its caller")
            .expect("recv");
        assert!(
            matches!(outcome, PermissionOutcome::Canceled { .. }),
            "the selected option is unrecoverable, so the turn must end unanswered: {outcome:?}"
        );
    }

    /// A request policy answers on arrival is durable in one step: the row is
    /// approved, the decision names the policy as the decider, and nothing is
    /// left pending or scheduled to expire.
    #[tokio::test]
    async fn policy_approval_records_an_approved_decision() {
        let (_dir, service) = fresh_service_with(
            PermissionTimeoutAction::Deny,
            Duration::from_secs(300),
            AcpPromptAction::Approve,
        );
        let (record, decision) = service
            .approve_by_policy(NewPermission {
                source: PermissionSource::Acp,
                requester: Some("session:sess_policy".to_owned()),
                subject_id: Some("sess_policy".to_owned()),
                detail: json!({}),
            })
            .await
            .expect("policy approval");

        assert_eq!(decision.request_id, record.id);
        assert_eq!(decision.decision, "approved");
        assert_eq!(decision.deciding_principal.as_deref(), Some("policy"));
        assert_eq!(decision.reason.as_deref(), Some("auto-approved by policy"));
        assert_eq!(
            record.expires_at, None,
            "a request decided on arrival has nothing to expire"
        );

        let view = service.get(&record.id).await.expect("get");
        assert_eq!(view.status, "approved");
        assert_eq!(view.source, "acp");
        assert!(service.pending(10).await.expect("pending").is_empty());

        let kinds: Vec<String> = {
            let state = service.state.lock().await;
            state
                .query_permission_events(crate::state::EventFilter {
                    limit: 10,
                    permission_id: Some(&record.id),
                    ..crate::state::EventFilter::default()
                })
                .expect("query permission events")
                .into_iter()
                .map(|row| row.kind)
                .collect()
        };
        assert!(kinds.iter().any(|kind| kind == "permission.created"));
        assert!(kinds.iter().any(|kind| kind == "permission.approved"));
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
    async fn session_sweep_cancels_only_that_session_acp_requests() {
        // A long timeout keeps the expiry timer out of the way: the sweep, not
        // the clock, must be what settles these rows.
        let (_dir, service) =
            fresh_service_with_timeout(PermissionTimeoutAction::Deny, Duration::from_secs(300));
        let mut ids = Vec::new();
        for (source, subject) in [
            (PermissionSource::Acp, "sess_a"),
            (PermissionSource::Acp, "sess_b"),
            (PermissionSource::Command, "sess_a"),
        ] {
            let (record, rx) = service
                .request(NewPermission {
                    source,
                    requester: Some(format!("session:{subject}")),
                    subject_id: Some(subject.to_owned()),
                    detail: json!({}),
                })
                .await
                .expect("request");
            // Holding the receivers keeps the waiters alive for the duration.
            ids.push((record.id, rx));
        }

        let settled = service
            .cancel_pending_for_session("sess_a", "session-cancelled")
            .await
            .expect("sweep");
        assert_eq!(settled, 1, "only the ACP request for sess_a is swept");
        assert_eq!(
            service.get(&ids[0].0).await.expect("get").status,
            "cancelled"
        );
        assert_eq!(service.get(&ids[1].0).await.expect("get").status, "pending");
        assert_eq!(service.get(&ids[2].0).await.expect("get").status, "pending");

        // Repeat sweeps are safe: nothing is left to settle.
        assert_eq!(
            service
                .cancel_pending_for_session("sess_a", "session-cancelled")
                .await
                .expect("second sweep"),
            0
        );
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
    async fn timeout_event_is_filterable_by_permission_id() {
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

        let rows = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let rows = {
                    let state = service.state.lock().await;
                    state
                        .query_permission_events(crate::state::EventFilter {
                            limit: 10,
                            permission_id: Some(&record.id),
                            ..crate::state::EventFilter::default()
                        })
                        .expect("query permission events")
                };
                if rows.iter().any(|row| row.kind == "permission.expired") {
                    return rows;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("timeout event should persist");

        let expired = rows
            .iter()
            .find(|row| row.kind == "permission.expired")
            .expect("expired event");
        let payload: serde_json::Value =
            serde_json::from_str(&expired.payload_json).expect("payload json");
        assert_eq!(payload["permission_id"], record.id);
    }

    #[tokio::test]
    async fn canceled_event_payload_carries_command_id_for_command_source() {
        let (_dir, service) = fresh_service(PermissionTimeoutAction::Deny);
        let (record, _rx) = service
            .request(NewPermission {
                source: PermissionSource::Command,
                requester: Some("command:cmd_x".to_owned()),
                subject_id: Some("cmd_x".to_owned()),
                detail: json!({ "command": "sudo true" }),
            })
            .await
            .expect("request");
        service
            .cancel(&record.id, "command-permission-waiter-lost")
            .await
            .expect("cancel");

        let rows = {
            let state = service.state.lock().await;
            state
                .query_permission_events(crate::state::EventFilter {
                    limit: 10,
                    permission_id: Some(&record.id),
                    ..crate::state::EventFilter::default()
                })
                .expect("query permission events")
        };
        let canceled = rows
            .iter()
            .find(|row| row.kind == "permission.cancelled")
            .expect("canceled event");
        let payload: serde_json::Value =
            serde_json::from_str(&canceled.payload_json).expect("payload json");
        assert_eq!(payload["command_id"], "cmd_x");
        assert_eq!(payload["source"], "command");
        assert_eq!(payload["subject_id"], "cmd_x");
        assert_eq!(payload["reason"], "command-permission-waiter-lost");

        let by_command = {
            let state = service.state.lock().await;
            state
                .query_permission_events(crate::state::EventFilter {
                    limit: 10,
                    command_id: Some("cmd_x"),
                    ..crate::state::EventFilter::default()
                })
                .expect("query by command id")
        };
        assert!(
            by_command
                .iter()
                .any(|row| row.kind == "permission.cancelled"),
            "cancellation must be filterable by command_id"
        );
    }

    #[tokio::test]
    async fn acp_source_cancel_payload_omits_command_id() {
        let (_dir, service) = fresh_service(PermissionTimeoutAction::Deny);
        let (record, _rx) = service
            .request(NewPermission {
                source: PermissionSource::Acp,
                requester: Some("sess_a".to_owned()),
                subject_id: Some("sess_a".to_owned()),
                detail: json!({}),
            })
            .await
            .expect("request");
        service
            .cancel(&record.id, "agent-stopped")
            .await
            .expect("cancel");

        let rows = {
            let state = service.state.lock().await;
            state
                .query_permission_events(crate::state::EventFilter {
                    limit: 10,
                    permission_id: Some(&record.id),
                    ..crate::state::EventFilter::default()
                })
                .expect("query permission events")
        };
        let canceled = rows
            .iter()
            .find(|row| row.kind == "permission.cancelled")
            .expect("canceled event");
        let payload: serde_json::Value =
            serde_json::from_str(&canceled.payload_json).expect("payload json");
        assert!(payload.get("command_id").is_none());
        assert_eq!(payload["source"], "acp");
        assert_eq!(payload["subject_id"], "sess_a");
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

    #[tokio::test]
    async fn cancel_if_pending_reports_a_lost_decision_race() {
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
            .approve(&record.id, Some("allow".to_owned()), None, "session-key")
            .await
            .expect("approve");

        assert!(
            !service
                .cancel_if_pending(&record.id, "acp-request-cancelled")
                .await
                .expect("race result")
        );
        assert!(matches!(
            rx.await.expect("recv"),
            PermissionOutcome::Approved { .. }
        ));
    }
}
