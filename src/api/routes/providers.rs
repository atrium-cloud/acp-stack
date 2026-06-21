//! Provider and ACP-advertised model discovery for the unified API.
//!
//! `GET /v1/providers` returns the providers supported for the
//! currently configured agent (id, display name, default API-key ref,
//! companion/optional env refs). The data is sourced from the embedded
//! provider/env mapping under `data/`, so this endpoint is offline-only
//! and does not spawn the agent.
//!
//! `GET /v1/models` spawns a provisional ACP session against the
//! configured agent, reads its `session/new` advertised
//! `config_options`, and returns the model + mode value lists. This
//! mirrors what `acps agent set` does interactively, so a UI driver
//! can render exactly the same picker without shelling out to the CLI.
//! The endpoint is session-tier (valid session key required).

use axum::extract::State;
use serde::Serialize;

use crate::envelope::ApiSuccess;
use crate::error::StackError;
use crate::fs_util::home_dir;
use crate::runtime::agent::acp_bridge::AgentSessionConfigCategory;
use crate::runtime::agent::model_discovery::{
    DEFAULT_MODELS_DISCOVERY_TIMEOUT, advertised_values_for_category,
    fetch_session_config_with_timeout,
};
use crate::runtime::agent::provider_keys::{AgentProviderSummary, providers_for_agent};

use super::super::core::AppState;

#[derive(Serialize)]
pub(crate) struct ProvidersResponse {
    agent_id: String,
    providers: Vec<ProviderJson>,
}

#[derive(Serialize)]
struct ProviderJson {
    id: &'static str,
    name: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_provider_id: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    default_api_key_ref: Option<&'static str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    companion_env_refs: Vec<&'static str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    optional_env_refs: Vec<&'static str>,
}

impl From<AgentProviderSummary> for ProviderJson {
    fn from(summary: AgentProviderSummary) -> Self {
        Self {
            id: summary.id,
            name: summary.name,
            agent_provider_id: summary.agent_provider_id,
            default_api_key_ref: summary.default_api_key_ref,
            companion_env_refs: summary.companion_env_refs,
            optional_env_refs: summary.optional_env_refs,
        }
    }
}

pub(crate) async fn providers_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<ProvidersResponse>, StackError> {
    let (config, _) = state.default_agent_target().await?;
    let agent_id = config.agent.id;
    let providers = providers_for_agent(&agent_id)
        .into_iter()
        .map(ProviderJson::from)
        .collect();
    Ok(ApiSuccess::new(ProvidersResponse {
        agent_id,
        providers,
    }))
}

#[derive(Serialize)]
pub(crate) struct ModelsResponse {
    agent_id: String,
    /// ACP-advertised `model` values, in registry-declared order.
    models: Vec<String>,
    /// ACP-advertised `mode` values. Empty when the agent does not
    /// expose a mode option.
    modes: Vec<String>,
}

pub(crate) async fn models_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<ModelsResponse>, StackError> {
    // Resolve the default target from disk so `acps agent default set`
    // and Array config edits are visible without a daemon restart.
    let (config, _) = state.default_agent_target().await?;
    let agent_id = config.agent.id.clone();
    let home = home_dir()?;
    let response =
        fetch_session_config_with_timeout(&home, &config, DEFAULT_MODELS_DISCOVERY_TIMEOUT).await?;
    // Surface a malformed/missing `model` advertisement as an error
    // so the operator knows discovery failed rather than silently
    // rendering an empty picker. `mode` is genuinely optional.
    let models = advertised_values_for_category(&response, AgentSessionConfigCategory::Model)?;
    let modes = advertised_values_for_category(&response, AgentSessionConfigCategory::Mode)
        .unwrap_or_default();

    Ok(ApiSuccess::new(ModelsResponse {
        agent_id,
        models,
        modes,
    }))
}
