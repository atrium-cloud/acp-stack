//! Manual agent update over HTTP: the admin-tier update trigger and the
//! session-tier version-visibility read.

use super::*;

use crate::config::DEFAULT_AGENT_AUTO_UPDATE_FREQUENCY;
use crate::runtime::install::agent_updater::{
    AgentUpdateOptions, AgentUpdateReport, NON_REGISTRY_SKIP_REASON, run_managed_agent_update,
};
use crate::runtime::install::agent_version_check::{
    AgentVersionStatus, LiveLatestVersionResolver, build_agent_check_report,
};

/// Marks lifecycle events emitted by the HTTP route apart from the
/// auto-update timer's; the `agent_lifecycle` table has no source column.
const UPDATE_TRIGGER_API: &str = "api";

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub(crate) struct AgentUpdateRequest {
    #[serde(default)]
    force: bool,
}

pub(crate) async fn agent_update_handler(
    State(state): State<AppState>,
    body: Option<Json<AgentUpdateRequest>>,
) -> std::result::Result<ApiSuccess<AgentUpdateReport>, StackError> {
    let request = body.map(|Json(request)| request).unwrap_or_default();
    let target_id = state.default_target_id().await?;
    let (config, target) = load_fresh_config_for_target(&state, &target_id).await?;
    let agent_id = config.agent.id.clone();
    // Everything fallible happens before `try_begin_update`; a `?` between
    // begin and finish would leave the supervisor stuck in `Updating`.
    let home = home_dir()?;
    let state_path = state.runtime_paths.state_path.clone();

    if !target.supervisor.try_begin_update().await {
        append_update_lifecycle(
            &state,
            "agent.update.skipped",
            "agent update skipped",
            serde_json::json!({
                "agent_id": agent_id,
                "reason": "agent is running",
                "trigger": UPDATE_TRIGGER_API,
            }),
        )
        .await;
        return Ok(ApiSuccess::new(AgentUpdateReport::skipped(
            agent_id,
            "agent is running",
        )));
    }
    // Must stay a detached task with no await between `try_begin_update` and
    // `spawn`: a client disconnect cancels only this handler future, so an
    // inline await would strand the supervisor in `Updating`.
    let update_task = tokio::spawn(run_update_and_release(
        state.clone(),
        target.supervisor.clone(),
        home,
        state_path,
        config,
        request.force,
    ));
    match update_task.await {
        Ok(result) => result.map(ApiSuccess::new),
        Err(err) => Err(StackError::AgentInitializeFailed {
            reason: format!("agent update task join failed: {err}"),
        }),
    }
}

/// The post-lock update sequence: started event, blocking update, lock
/// release, terminal event. Runs detached from the request handler so it
/// always completes even when the HTTP caller disconnects.
async fn run_update_and_release(
    state: AppState,
    supervisor: std::sync::Arc<crate::runtime::agent::supervisor::AgentSupervisor>,
    home: std::path::PathBuf,
    state_path: std::path::PathBuf,
    config: Config,
    force: bool,
) -> std::result::Result<AgentUpdateReport, StackError> {
    let agent_id = config.agent.id.clone();
    append_update_lifecycle(
        &state,
        "agent.update.started",
        "agent update started",
        serde_json::json!({ "agent_id": agent_id, "trigger": UPDATE_TRIGGER_API }),
    )
    .await;
    let result = tokio::task::spawn_blocking(move || {
        run_managed_agent_update(
            home,
            state_path,
            config,
            AgentUpdateOptions {
                force,
                agent_running: false,
            },
        )
    })
    .await;
    supervisor.finish_update().await;
    match result {
        Ok(Ok(report)) => {
            let (event_kind, event_message) = if report.skipped {
                ("agent.update.skipped", "agent update skipped")
            } else if report.has_failed_steps() {
                ("agent.update.failed", "agent update failed")
            } else {
                ("agent.update.finished", "agent update finished")
            };
            let mut payload = serde_json::to_value(&report)
                .unwrap_or_else(|_| serde_json::json!({ "agent_id": report.agent_id }));
            if let Some(object) = payload.as_object_mut() {
                object.insert(
                    "trigger".to_owned(),
                    serde_json::Value::String(UPDATE_TRIGGER_API.to_owned()),
                );
            }
            append_update_lifecycle(&state, event_kind, event_message, payload).await;
            Ok(report)
        }
        Ok(Err(err)) => {
            append_update_lifecycle(
                &state,
                "agent.update.failed",
                "agent update failed",
                serde_json::json!({ "error": err.to_string(), "trigger": UPDATE_TRIGGER_API }),
            )
            .await;
            Err(err)
        }
        Err(err) => {
            // Pair the earlier `agent.update.started` row with a terminal
            // failure so the SQLite trail is never left open-ended.
            append_update_lifecycle(
                &state,
                "agent.update.failed",
                "agent update failed",
                serde_json::json!({ "error": err.to_string(), "trigger": UPDATE_TRIGGER_API }),
            )
            .await;
            Err(StackError::AgentInitializeFailed {
                reason: format!("agent update task join failed: {err}"),
            })
        }
    }
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct AgentUpdateStatusResponse {
    agent_id: String,
    managed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pinned: Option<String>,
    auto_update: AgentAutoUpdatePolicyJson,
    components: Vec<AgentUpdateStatusComponent>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct AgentAutoUpdatePolicyJson {
    enabled: bool,
    frequency: String,
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct AgentUpdateStatusComponent {
    #[schemars(extend("enum" = ["install", "harness", "adapter"]))]
    step: String,
    #[serde(flatten)]
    result: AgentVersionStatus,
}

pub(crate) async fn agent_update_status_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<AgentUpdateStatusResponse>, StackError> {
    let config = Config::load_lenient_from_path(&state.runtime_paths.config_path)?;
    let registry = load_active_registry()?;
    let agent_id = config.agent.id.clone();
    let pinned = config.agent.harness_version.clone();
    // An absent `[agent.auto_update]` section means disabled, matching the
    // timer; a concrete frequency is still reported so callers can render one.
    let auto_update = match config.agent.auto_update.as_ref() {
        Some(auto_update) => AgentAutoUpdatePolicyJson {
            enabled: auto_update.enabled,
            frequency: auto_update.frequency.clone(),
        },
        None => AgentAutoUpdatePolicyJson {
            enabled: false,
            frequency: DEFAULT_AGENT_AUTO_UPDATE_FREQUENCY.to_owned(),
        },
    };
    let entry = match registry.lookup_required(&agent_id) {
        Ok(entry) => entry.clone(),
        Err(StackError::AgentRegistryMissing { .. }) => {
            return Ok(ApiSuccess::new(AgentUpdateStatusResponse {
                agent_id,
                managed: false,
                reason: Some(NON_REGISTRY_SKIP_REASON.to_owned()),
                pinned,
                auto_update,
                components: Vec::new(),
            }));
        }
        Err(error) => return Err(error),
    };
    let installed_rows = {
        let store = state.state.lock().await;
        store.latest_successful_installer_runs_for_agent(&agent_id)?
    };
    // Upstream lookups are blocking HTTP; a failure degrades that component to
    // `unknown` in the report rather than failing the route.
    let agent_config = config.agent.clone();
    let report = tokio::task::spawn_blocking(move || {
        build_agent_check_report(
            &entry,
            &agent_config,
            &installed_rows,
            &LiveLatestVersionResolver,
        )
    })
    .await
    .map_err(|err| StackError::AgentInitializeFailed {
        reason: format!("agent version check task join failed: {err}"),
    })?;
    Ok(ApiSuccess::new(AgentUpdateStatusResponse {
        agent_id,
        managed: true,
        reason: None,
        pinned,
        auto_update,
        components: report
            .into_iter()
            .map(|(step, result)| AgentUpdateStatusComponent { step, result })
            .collect(),
    }))
}

/// Record an `agent.update.*` lifecycle event; the update itself already
/// happened, so a failed append is warn-logged rather than propagated.
async fn append_update_lifecycle(
    state: &AppState,
    kind: &str,
    message: &str,
    payload: serde_json::Value,
) {
    let payload = payload.to_string();
    let store = state.state.lock().await;
    if let Err(err) = store.append_agent_lifecycle(kind, message, &payload) {
        tracing::warn!(error = %err, kind, "agent update: failed to record lifecycle event");
    }
}
