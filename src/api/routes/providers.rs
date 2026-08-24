//! Provider and ACP-advertised model discovery for the unified API.

use axum::extract::State;
use serde::Serialize;

use crate::envelope::ApiSuccess;
use crate::error::StackError;
use crate::fs_util::home_dir;
use crate::runtime::agent::acp_bridge::AgentSessionConfigCategory;
use crate::runtime::agent::model_discovery::{
    DEFAULT_MODELS_DISCOVERY_TIMEOUT, advertised_values_for_category,
    fetch_session_config_with_timeout, model_value_is_explicit_without_discovery,
};
use crate::runtime::agent::provider_keys::{
    AgentProviderSummary, models_url_for_provider_id, providers_for_agent,
};
use crate::runtime::agent::provider_model_catalog::{cached_models, refresh_provider_models};

use super::super::core::AppState;

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct ProvidersResponse {
    agent_id: String,
    providers: Vec<ProviderJson>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct ProviderJson {
    /// Provider id, same namespace as `ProviderStatusJson.provider_id`.
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

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct ModelsResponse {
    agent_id: String,
    /// `"provider_catalog"` when `models` comes from the provider's live
    /// model listing; `"acp_advertised"` when it comes from the agent's
    /// ACP `session/new` config options.
    #[schemars(extend("enum" = ["provider_catalog", "acp_advertised"]))]
    source: &'static str,
    models: Vec<ModelJson>,
    /// ACP-advertised `mode` values. Empty when the agent does not
    /// expose a mode option (or, on the catalog fallback path, when ACP
    /// discovery failed).
    modes: Vec<String>,
    /// ACP-advertised reasoning-effort values (the `thought_level`
    /// session config option). Empty when the agent does not expose an
    /// effort option (or, on the catalog fallback path, when ACP
    /// discovery failed).
    efforts: Vec<String>,
    /// Set when the provider declares a model listing endpoint but the
    /// catalog is unavailable (fetch failed and no cache).
    #[serde(skip_serializing_if = "Option::is_none")]
    catalog_error: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct ModelJson {
    /// Model id accepted verbatim by `acps agent set --model`.
    pub(crate) value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) display_name: Option<String>,
}

const MODELS_SOURCE_PROVIDER_CATALOG: &str = "provider_catalog";
const MODELS_SOURCE_ACP_ADVERTISED: &str = "acp_advertised";

pub(crate) async fn models_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<ModelsResponse>, StackError> {
    // Resolve from disk so config edits are visible without a daemon restart.
    let (config, _) = state.default_agent_target().await?;
    let agent_id = config.agent.id.clone();
    let home = home_dir()?;

    // The provider catalog only serves agents whose harness takes the model
    // verbatim from its on-disk config; agents with real ACP discovery
    // advertise harness-specific values a raw catalog would not match.
    let provider_id = config
        .agent
        .provider
        .as_ref()
        .filter(|provider| {
            provider.custom.is_none() && model_value_is_explicit_without_discovery(&config.agent)
        })
        .map(|provider| provider.id.clone());
    let provider_declares_catalog = provider_id
        .as_deref()
        .is_some_and(|id| models_url_for_provider_id(id).is_some());
    let mut catalog_error = None;
    let catalog = if provider_declares_catalog {
        match refresh_provider_models(&home, &config).await {
            Ok(models) => models,
            Err(error) => {
                let reason = error.to_string();
                tracing::warn!(reason = %reason, "provider model catalog refresh failed");
                catalog_error = Some(reason);
                // A stale cache entry still serves through a provider outage.
                provider_id
                    .as_deref()
                    .and_then(|id| cached_models(&home, id))
            }
        }
    } else {
        None
    };

    if let Some(models) = catalog {
        // A discovery failure must not take down a response the catalog can
        // serve on its own.
        let (modes, efforts) = match fetch_session_config_with_timeout(
            &home,
            &config,
            DEFAULT_MODELS_DISCOVERY_TIMEOUT,
        )
        .await
        {
            Ok(response) => (
                advertised_values_for_category(&response, AgentSessionConfigCategory::Mode)
                    .unwrap_or_default(),
                advertised_values_for_category(&response, AgentSessionConfigCategory::Effort)
                    .unwrap_or_default(),
            ),
            Err(error) => {
                tracing::warn!(error = %error, "config-option discovery failed; serving catalog models without modes or efforts");
                (Vec::new(), Vec::new())
            }
        };
        return Ok(ApiSuccess::new(ModelsResponse {
            agent_id,
            source: MODELS_SOURCE_PROVIDER_CATALOG,
            models: models
                .into_iter()
                .map(|model| ModelJson {
                    value: model.value,
                    display_name: model.display_name,
                })
                .collect(),
            modes,
            efforts,
            catalog_error: None,
        }));
    }

    let response =
        fetch_session_config_with_timeout(&home, &config, DEFAULT_MODELS_DISCOVERY_TIMEOUT).await?;
    // A missing `model` advertisement is an error for discovery-backed agents,
    // so the operator learns discovery failed instead of seeing an empty picker.
    let models = match advertised_values_for_category(&response, AgentSessionConfigCategory::Model)
    {
        Ok(values) => values,
        // Explicit-model agents (Hermes) advertise no ACP model options at all.
        Err(error) if model_value_is_explicit_without_discovery(&config.agent) => {
            tracing::warn!(error = %error, "no ACP model advertisement; serving empty model list");
            Vec::new()
        }
        Err(error) => return Err(error),
    };
    let modes = advertised_values_for_category(&response, AgentSessionConfigCategory::Mode)
        .unwrap_or_default();
    let efforts = advertised_values_for_category(&response, AgentSessionConfigCategory::Effort)
        .unwrap_or_default();

    Ok(ApiSuccess::new(ModelsResponse {
        agent_id,
        source: MODELS_SOURCE_ACP_ADVERTISED,
        models: models
            .into_iter()
            .map(|value| ModelJson {
                value,
                display_name: None,
            })
            .collect(),
        modes,
        efforts,
        catalog_error,
    }))
}
