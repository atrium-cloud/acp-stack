use axum::extract::State;
use serde::Serialize;

use crate::api::core::AppState;
use crate::envelope::ApiSuccess;
use crate::error::StackError;
use crate::fs_util::home_dir;
use crate::runtime::agent::config_options::{SessionConfigOptionSnapshot, project_config_options};
use crate::runtime::agent::model_discovery::{
    DEFAULT_MODELS_DISCOVERY_TIMEOUT, fetch_session_config_with_timeout,
};

/// `GET /v1/agent/config-options`: every session config option the configured
/// agent advertises on a provisional `session/new`, projected verbatim —
/// including `model_config`, `_`-prefixed customs, and category-less options
/// the typed `/v1/models` lanes do not carry. Discovery failure is a hard
/// error: unlike `/v1/models` there is no catalog to fall back to, and an
/// empty list would be indistinguishable from "advertises nothing".
#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct AgentConfigOptionsResponse {
    agent_id: String,
    config_options: Vec<SessionConfigOptionSnapshot>,
}

pub(crate) async fn agent_config_options_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<AgentConfigOptionsResponse>, StackError> {
    let (config, _) = state.default_agent_target().await?;
    let agent_id = config.agent.id.clone();
    let home = home_dir()?;
    let response =
        fetch_session_config_with_timeout(&home, &config, DEFAULT_MODELS_DISCOVERY_TIMEOUT).await?;
    let config_options = project_config_options(response.config_options.as_deref().unwrap_or(&[]));
    Ok(ApiSuccess::new(AgentConfigOptionsResponse {
        agent_id,
        config_options,
    }))
}
