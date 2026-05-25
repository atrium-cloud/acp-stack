//! Agent runtime/lifecycle error helpers.
//!
//! Covers the half of the `agent.*` namespace that surfaces while the agent
//! subprocess is running (spawn, lifecycle state, JSON-RPC requests).

use http::StatusCode;

use super::StackError;

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
        AgentTestFailed { .. } => "agent.test_failed",
        _ => return None,
    })
}

pub(super) fn public_message(err: &StackError) -> Option<String> {
    use StackError::*;
    Some(match err {
        AgentSpawnFailed { .. } => "failed to spawn agent subprocess".to_owned(),
        AgentAlreadyRunning => "agent is already running".to_owned(),
        AgentNotRunning => "agent is not running".to_owned(),
        AgentInitializeFailed { reason } => format!("agent failed to initialize: {reason}"),
        AgentNotInitialized => "agent has not been initialized yet".to_owned(),
        AgentUnsupportedCapability { name } => format!("agent does not support `{name}`"),
        AgentApiRequest { path, .. } => format!("agent API request to {path} failed"),
        AgentApiStatus { path, status, .. } => {
            format!("agent API request to {path} failed with status {status}")
        }
        AgentRequestFailed { method, .. } => format!("agent rejected `{method}` request"),
        AgentTestFailed { stage, reason } => format!("agent test failed at {stage}: {reason}"),
        _ => return None,
    })
}

pub(super) fn http_status(err: &StackError) -> Option<StatusCode> {
    use StackError::*;
    Some(match err {
        AgentAlreadyRunning | AgentNotRunning => StatusCode::CONFLICT,
        AgentNotInitialized => StatusCode::NOT_FOUND,
        AgentUnsupportedCapability { .. } => StatusCode::NOT_IMPLEMENTED,
        AgentInitializeFailed { .. } => StatusCode::BAD_GATEWAY,
        AgentSpawnFailed { .. } | AgentApiRequest { .. } | AgentApiStatus { .. } => {
            StatusCode::INTERNAL_SERVER_ERROR
        }
        AgentRequestFailed { .. } | AgentTestFailed { .. } => StatusCode::BAD_GATEWAY,
        _ => return None,
    })
}
