//! Error helpers for the `agent.*` namespace that surfaces while the agent
//! subprocess is running (spawn, lifecycle state, JSON-RPC requests).

use http::StatusCode;
use serde_json::{Map, Value, json};

use super::StackError;

/// Who a failed ACP request concerned, plus the adapter's own JSON-RPC error when it sent one.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentRequestContext {
    pub agent_id: Option<String>,
    pub target_id: Option<String>,
    pub acp_session_id: Option<String>,
    pub acp_error: Option<AcpErrorDetail>,
}

/// The adapter's JSON-RPC error, already redacted and bounded where it was captured.
#[derive(Debug, Clone, PartialEq)]
pub struct AcpErrorDetail {
    pub code: i32,
    pub message: String,
    pub data: Option<Value>,
}

impl StackError {
    /// `AgentRequestFailed` for a failure acp-stack itself detected, with no adapter error.
    pub fn agent_request_failed(method: &'static str, message: impl Into<String>) -> Self {
        Self::AgentRequestFailed {
            method,
            message: message.into(),
            context: Box::default(),
        }
    }
}

pub(super) fn agent_request_failed_display(
    method: &str,
    message: &str,
    context: &AgentRequestContext,
) -> String {
    let ids: Vec<String> = [
        ("agent", &context.agent_id),
        ("target", &context.target_id),
        ("session", &context.acp_session_id),
    ]
    .into_iter()
    .filter_map(|(label, id)| id.as_ref().map(|id| format!("{label} `{id}`")))
    .collect();
    let mut text = format!("agent request to {method} failed");
    if !ids.is_empty() {
        text.push_str(&format!(" ({})", ids.join(", ")));
    }
    match &context.acp_error {
        Some(detail) => {
            text.push_str(&format!(": ACP error {}: {}", detail.code, detail.message));
            if let Some(data) = &detail.data {
                text.push_str(&format!("; data: {data}"));
            }
        }
        None => text.push_str(&format!(": {message}")),
    }
    text
}

pub(super) fn public_details(err: &StackError) -> Option<Map<String, Value>> {
    let StackError::AgentRequestFailed { context, .. } = err else {
        return None;
    };
    let detail = context.acp_error.as_ref()?;
    let mut acp_error = json!({ "code": detail.code, "message": detail.message });
    if let (Some(data), Some(object)) = (&detail.data, acp_error.as_object_mut()) {
        object.insert("data".to_owned(), data.clone());
    }
    let mut details = Map::new();
    details.insert("acp_error".to_owned(), acp_error);
    Some(details)
}

pub(super) fn error_code(err: &StackError) -> Option<&'static str> {
    use StackError::*;
    Some(match err {
        AgentSpawnFailed { .. } => "agent.spawn_failed",
        AgentAlreadyRunning => "agent.already_running",
        AgentNotRunning => "agent.not_running",
        AgentInitializeFailed { .. } => "agent.initialize_failed",
        AgentNotInitialized => "agent.not_initialized",
        AgentUnsupportedCapability { .. } => "agent.unsupported_capability",
        AgentApiRequest { .. } => "agent.api_request_failed",
        AgentApiStatus { .. } => "agent.api_status_failed",
        AgentRequestFailed { .. } => "agent.request_failed",
        InferenceRequestFailed { status_code, .. } => {
            if (400..500).contains(status_code) {
                "agent.inference_4xx"
            } else {
                // 5xx and the 529-overloaded variant share this code.
                "agent.inference_5xx"
            }
        }
        AgentTestFailed { .. } => "agent.test_failed",
        AgentSwitchConflict { .. } => "agent.switch_conflict",
        AgentSwitchJournalCorrupt { .. } => "agent.switch_journal_corrupt",
        ProviderModelCatalog { .. } => "agent.provider_model_catalog_failed",
        ArrayTargetsFailed { .. } => "array.targets_failed",
        _ => return None,
    })
}

pub(super) fn public_message(err: &StackError) -> Option<String> {
    use StackError::*;
    Some(match err {
        AgentSpawnFailed { .. } => "failed to spawn agent subprocess".to_owned(),
        AgentAlreadyRunning => "agent is already running".to_owned(),
        AgentNotRunning => "agent is not running".to_owned(),
        // Reasons are built from join errors, I/O sources, and subprocess-derived
        // text at the call sites, so the public surface stays static.
        AgentInitializeFailed { .. } => "agent failed to initialize".to_owned(),
        AgentNotInitialized => "agent has not been initialized yet".to_owned(),
        AgentUnsupportedCapability { name } => format!("agent does not support `{name}`"),
        AgentApiRequest { path, .. } => format!("agent API request to {path} failed"),
        AgentApiStatus { path, status, .. } => {
            format!("agent API request to {path} failed with status {status}")
        }
        AgentRequestFailed {
            method, context, ..
        } => match context.acp_error {
            Some(_) => format!("agent rejected `{method}` request"),
            None => format!("agent request `{method}` failed"),
        },
        InferenceRequestFailed {
            status_code,
            reason_category,
        } => format!("inference endpoint returned {status_code} ({reason_category})"),
        // `reason` embeds workspace paths and spawn argv (see the enum doc);
        // the stage name is the identifier the API may carry.
        AgentTestFailed { stage, .. } => format!("agent test failed at {stage}"),
        AgentSwitchConflict { reason } => format!("agent switch conflict: {reason}"),
        // The on-disk path stays out of the public message; Display carries it
        // for local logs only.
        AgentSwitchJournalCorrupt { .. } => {
            "the pending agent-switch journal is corrupt local state".to_owned()
        }
        // `reason` carries upstream HTTP bodies and transport error text.
        ProviderModelCatalog { provider, .. } => {
            format!("provider `{provider}` model catalog fetch failed")
        }
        // `summary` concatenates per-target failure text that may include paths.
        ArrayTargetsFailed {
            action,
            failed,
            total,
            ..
        } => format!("array {action} failed for {failed} of {total} target(s)"),
        _ => return None,
    })
}

pub(super) fn http_status(err: &StackError) -> Option<StatusCode> {
    use StackError::*;
    Some(match err {
        AgentAlreadyRunning | AgentNotRunning | AgentSwitchConflict { .. } => StatusCode::CONFLICT,
        AgentNotInitialized => StatusCode::NOT_FOUND,
        AgentUnsupportedCapability { .. } => StatusCode::NOT_IMPLEMENTED,
        AgentInitializeFailed { .. } => StatusCode::BAD_GATEWAY,
        AgentSpawnFailed { .. } | AgentApiRequest { .. } | AgentApiStatus { .. } => {
            StatusCode::INTERNAL_SERVER_ERROR
        }
        AgentSwitchJournalCorrupt { .. } | ArrayTargetsFailed { .. } => {
            StatusCode::INTERNAL_SERVER_ERROR
        }
        AgentRequestFailed { .. } | AgentTestFailed { .. } | ProviderModelCatalog { .. } => {
            StatusCode::BAD_GATEWAY
        }
        InferenceRequestFailed { status_code, .. } => {
            // 4xx means the upstream rejected the request on its own terms;
            // 5xx means the upstream itself failed.
            if (400..500).contains(status_code) {
                StatusCode::FAILED_DEPENDENCY
            } else {
                StatusCode::BAD_GATEWAY
            }
        }
        _ => return None,
    })
}
