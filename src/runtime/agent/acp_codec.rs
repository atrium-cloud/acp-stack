//! Free codec helpers for ACP session/config option encoding and inbound
//! request/notification translation.
//!
//! Extracted from `acp_bridge.rs` so the bridge file can focus on the
//! connection lifecycle. These helpers do not need an `AcpBridge` instance;
//! they translate between ACP protocol types and the daemon's own request
//! shapes.

use std::path::PathBuf;
use std::sync::Arc;

use agent_client_protocol::schema::{
    NewSessionResponse, PermissionOptionId, RequestPermissionOutcome, RequestPermissionRequest,
    SelectedPermissionOutcome, SessionConfigKind, SessionConfigOption, SessionConfigSelectOptions,
    SessionNotification,
};

use crate::error::{Result, StackError};
use crate::runtime::agent::acp_bridge::{AgentSessionConfigCategory, AgentSessionModelSelection};
use crate::runtime::agent::session_sink::SessionEventSink;
use crate::runtime::mediation::permissions::{
    NewPermission, PermissionOutcome, PermissionService, PermissionSource,
};

use super::acp_bridge::NotificationDrain;

pub fn session_config_id_for_value(
    config_options: Option<&[SessionConfigOption]>,
    category: AgentSessionConfigCategory,
    value: &str,
) -> Result<String> {
    let Some(config_options) = config_options else {
        return Err(StackError::AgentConfigProvision {
            path: PathBuf::from("ACP session config options"),
            reason: format!(
                "agent did not advertise a `{}` session config option",
                category.id()
            ),
        });
    };
    for option in config_options {
        let category_matches = option
            .category
            .as_ref()
            .is_some_and(|option_category| category.matches(option_category));
        let id_matches = option.id.0.as_ref() == category.id();
        if (category_matches || id_matches) && session_config_option_contains_value(option, value) {
            return Ok(option.id.0.to_string());
        }
    }
    Err(StackError::AgentConfigProvision {
        path: PathBuf::from("ACP session config options"),
        reason: format!(
            "agent did not advertise `{value}` as an available `{}`",
            category.id()
        ),
    })
}

pub fn session_config_values(
    config_options: Option<&[SessionConfigOption]>,
    category: AgentSessionConfigCategory,
) -> Result<Vec<String>> {
    let Some(config_options) = config_options else {
        return Err(StackError::AgentConfigProvision {
            path: PathBuf::from("ACP session config options"),
            reason: format!(
                "agent did not advertise a `{}` session config option",
                category.id()
            ),
        });
    };
    for option in config_options {
        let category_matches = option
            .category
            .as_ref()
            .is_some_and(|option_category| category.matches(option_category));
        let id_matches = option.id.0.as_ref() == category.id();
        if category_matches || id_matches {
            let mut values = session_config_option_values(option);
            values.sort();
            values.dedup();
            return Ok(values);
        }
    }
    Err(StackError::AgentConfigProvision {
        path: PathBuf::from("ACP session config options"),
        reason: format!(
            "agent did not advertise a `{}` session config option",
            category.id()
        ),
    })
}

pub fn session_model_selection_for_value(
    response: &NewSessionResponse,
    value: &str,
) -> Result<AgentSessionModelSelection> {
    if let Some(config_options) = response.config_options.as_deref()
        && let Ok(config_id) = session_config_id_for_value(
            Some(config_options),
            AgentSessionConfigCategory::Model,
            value,
        )
    {
        return Ok(AgentSessionModelSelection::ConfigOption { config_id });
    }
    if legacy_session_model_values(response)
        .iter()
        .any(|candidate| candidate == value)
    {
        return Ok(AgentSessionModelSelection::LegacyModel);
    }
    Err(StackError::AgentConfigProvision {
        path: PathBuf::from("ACP session config options"),
        reason: format!("agent did not advertise `{value}` as an available `model`"),
    })
}

pub fn session_model_values(response: &NewSessionResponse) -> Result<Vec<String>> {
    if let Some(config_options) = response.config_options.as_deref()
        && let Ok(values) =
            session_config_values(Some(config_options), AgentSessionConfigCategory::Model)
    {
        return Ok(values);
    }
    let mut values = legacy_session_model_values(response);
    values.sort();
    values.dedup();
    if values.is_empty() {
        return Err(StackError::AgentConfigProvision {
            path: PathBuf::from("ACP session config options"),
            reason: "agent did not advertise a `model` session config option".to_owned(),
        });
    }
    Ok(values)
}

fn legacy_session_model_values(response: &NewSessionResponse) -> Vec<String> {
    response
        .models
        .as_ref()
        .map(|models| {
            models
                .available_models
                .iter()
                .map(|model| model.model_id.0.to_string())
                .collect()
        })
        .unwrap_or_default()
}

fn session_config_option_contains_value(option: &SessionConfigOption, value: &str) -> bool {
    session_config_option_values(option)
        .iter()
        .any(|candidate| candidate == value)
}

fn session_config_option_values(option: &SessionConfigOption) -> Vec<String> {
    match &option.kind {
        SessionConfigKind::Select(select) => match &select.options {
            SessionConfigSelectOptions::Ungrouped(options) => options
                .iter()
                .map(|option| option.value.0.to_string())
                .collect(),
            SessionConfigSelectOptions::Grouped(groups) => groups
                .iter()
                .flat_map(|group| group.options.iter())
                .map(|option| option.value.0.to_string())
                .collect(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// Forward a `session/request_permission` request through the durable
/// PermissionService, await the decision, and translate the result back to
/// the ACP `RequestPermissionOutcome`. Falls back to `Cancelled` on every
/// failure so the agent's prompt turn always settles deterministically.
pub(crate) async fn resolve_acp_permission(
    service: &PermissionService,
    request: RequestPermissionRequest,
) -> RequestPermissionOutcome {
    // Serialize the full request for the durable detail record. The schema
    // type is JSON-friendly; failure here only happens for non-JSON-safe
    // values, which the schema does not contain.
    let detail = match serde_json::to_value(&request) {
        Ok(value) => value,
        Err(err) => {
            tracing::warn!(error = %err, "failed to serialize permission request");
            return RequestPermissionOutcome::Cancelled;
        }
    };
    let session_id = request.session_id.0.to_string();
    let first_option_id = request
        .options
        .first()
        .map(|opt| opt.option_id.0.to_string());

    let (_record, rx) = match service
        .request(NewPermission {
            source: PermissionSource::Acp,
            requester: Some(format!("session:{session_id}")),
            subject_id: Some(session_id),
            detail,
        })
        .await
    {
        Ok(pair) => pair,
        Err(err) => {
            tracing::warn!(error = %err, "permission service rejected ACP passthrough");
            return RequestPermissionOutcome::Cancelled;
        }
    };

    match rx.await {
        Ok(PermissionOutcome::Approved { option_id, .. }) => {
            let chosen = option_id.or(first_option_id);
            match chosen {
                Some(id) => RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                    PermissionOptionId::new(id),
                )),
                None => RequestPermissionOutcome::Cancelled,
            }
        }
        _ => RequestPermissionOutcome::Cancelled,
    }
}

pub(super) fn enqueue_session_notification(
    sink: Arc<dyn SessionEventSink>,
    drain: Arc<NotificationDrain>,
    note: &SessionNotification,
) {
    // Serialize the verbatim notification payload so downstream queriers can
    // reconstruct the full ACP update without re-deriving from the typed enum.
    let payload = match serde_json::to_string(&note) {
        Ok(payload) => payload,
        Err(err) => {
            tracing::warn!(error = %err, "failed to serialize session/update; dropping");
            return;
        }
    };
    let session_id = note.session_id.0.to_string();
    let guard = drain.enter();
    tokio::spawn(async move {
        sink.append(&session_id, "session.update", &payload).await;
        drop(guard);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PermissionTimeoutAction;
    use crate::events::EventHub;
    use crate::runtime::mediation::permissions::PermissionService;
    use crate::state::StateStore;
    use agent_client_protocol::schema::{
        PermissionOption, PermissionOptionId, PermissionOptionKind, RequestPermissionRequest,
        SessionId, ToolCallId, ToolCallUpdate, ToolCallUpdateFields,
    };
    use std::time::Duration;
    use tokio::sync::Mutex as TokioMutex;

    fn fake_request(session_id: &str) -> RequestPermissionRequest {
        RequestPermissionRequest::new(
            SessionId::new(session_id),
            ToolCallUpdate::new(ToolCallId::new("tc_1"), ToolCallUpdateFields::default()),
            vec![PermissionOption::new(
                PermissionOptionId::new("allow"),
                "Allow",
                PermissionOptionKind::AllowOnce,
            )],
        )
    }

    async fn fresh_service() -> (tempfile::TempDir, PermissionService) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.sqlite");
        let store = StateStore::open(&path).expect("open");
        store.migrate().expect("migrate");
        let state = Arc::new(TokioMutex::new(store));
        let events = EventHub::new();
        (
            dir,
            PermissionService::new(
                state,
                events,
                Duration::from_secs(60),
                PermissionTimeoutAction::Deny,
            ),
        )
    }

    #[tokio::test]
    async fn approve_passthrough_returns_selected_option() {
        let (_dir, service) = fresh_service().await;
        let request = fake_request("sess_test");
        let service_for_task = service.clone();
        let outcome_task =
            tokio::spawn(async move { resolve_acp_permission(&service_for_task, request).await });

        // Drain the new permission row + approve it.
        let mut id = None;
        for _ in 0..50 {
            let pending = service.pending(10).await.expect("pending");
            if let Some(first) = pending.first() {
                id = Some(first.id.clone());
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let perm_id = id.expect("permission row must appear");
        service
            .approve(&perm_id, Some("allow".to_owned()), None, "session-key")
            .await
            .expect("approve");

        let outcome = outcome_task.await.expect("task joins");
        match outcome {
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome { option_id, .. }) => {
                assert_eq!(option_id.0.as_ref(), "allow");
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[tokio::test]
    async fn deny_passthrough_returns_cancelled() {
        let (_dir, service) = fresh_service().await;
        let request = fake_request("sess_test");
        let service_for_task = service.clone();
        let outcome_task =
            tokio::spawn(async move { resolve_acp_permission(&service_for_task, request).await });

        let mut id = None;
        for _ in 0..50 {
            let pending = service.pending(10).await.expect("pending");
            if let Some(first) = pending.first() {
                id = Some(first.id.clone());
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let perm_id = id.expect("permission row must appear");
        service
            .deny(&perm_id, None, "session-key")
            .await
            .expect("deny");

        let outcome = outcome_task.await.expect("task joins");
        assert!(matches!(outcome, RequestPermissionOutcome::Cancelled));
    }

    #[test]
    fn session_config_helpers_validate_select_values_by_category() {
        let options: Vec<SessionConfigOption> = serde_json::from_str(
            r#"[
                {
                    "id": "agent-model",
                    "name": "Model",
                    "category": "model",
                    "type": "select",
                    "currentValue": "openai/gpt-5.5",
                    "options": [
                        {"value": "openai/gpt-5.5", "name": "GPT-5.5"},
                        {"value": "anthropic/claude-sonnet-4-5", "name": "Claude Sonnet 4.5"}
                    ]
                },
                {
                    "id": "mode",
                    "name": "Mode",
                    "category": "mode",
                    "type": "select",
                    "currentValue": "smart",
                    "options": [
                        {"value": "smart", "name": "Smart"},
                        {"value": "fast", "name": "Fast"}
                    ]
                }
            ]"#,
        )
        .expect("session config options deserialize");

        let model_id = session_config_id_for_value(
            Some(&options),
            AgentSessionConfigCategory::Model,
            "openai/gpt-5.5",
        )
        .expect("model value should be accepted");
        assert_eq!(model_id, "agent-model");
        assert_eq!(
            session_config_values(Some(&options), AgentSessionConfigCategory::Mode)
                .expect("mode values"),
            ["fast", "smart"]
        );
        let err = session_config_id_for_value(
            Some(&options),
            AgentSessionConfigCategory::Model,
            "openai/not-advertised",
        )
        .expect_err("unknown model should be rejected");
        assert!(err.to_string().contains("openai/not-advertised"));
    }

    #[test]
    fn session_model_helpers_accept_legacy_model_state() {
        let response: NewSessionResponse = serde_json::from_str(
            r#"{
                "sessionId": "sess_legacy",
                "models": {
                    "currentModelId": "opencode-go/deepseek-v4-flash",
                    "availableModels": [
                        {
                            "modelId": "opencode-go/deepseek-v4-flash",
                            "name": "DeepSeek V4 Flash"
                        }
                    ]
                }
            }"#,
        )
        .expect("legacy model state deserializes");

        assert_eq!(
            session_model_values(&response).expect("legacy model values"),
            ["opencode-go/deepseek-v4-flash"]
        );
        assert_eq!(
            session_model_selection_for_value(&response, "opencode-go/deepseek-v4-flash")
                .expect("legacy model should validate"),
            AgentSessionModelSelection::LegacyModel
        );
    }
}
