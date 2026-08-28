//! Provisional ACP session helpers for model/mode/effort discovery: spawn the
//! agent, read `session/new` config options, shut it down.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::schema::v1::{NewSessionResponse, SessionConfigOption};

use crate::config::{AgentConfig, Config};
use crate::dev_gates::{
    FIXTURE_AGENT_CAPABILITIES_ENV, FIXTURE_CONFIG_OPTIONS_ENV, FIXTURE_NEW_SESSION_RESPONSE_ENV,
    fixture_path,
};
use crate::error::{Result, StackError};
use crate::runtime::agent::acp_bridge::{
    AcpBridge, AcpPermissionPolicy, AgentCapabilitiesDto, AgentSessionConfigCategory,
    KIMI_CODE_AGENT_ID, SessionEventSink, session_config_id_for_value, session_config_values,
    session_mode_selection_for_value, session_mode_values, session_model_selection_for_value,
    session_model_values,
};
use crate::runtime::agent::agent_headless_config::{CODEX_OPENROUTER_PROVIDER_ID, HERMES_AGENT_ID};
use crate::runtime::agent::provider_keys::{
    CLAUDE_CODE_AGENT_ID, CODEX_AGENT_ID, is_claude_code_profiled_provider,
    resolve_agent_environment, resolve_agent_environment_without_secrets,
};
use crate::secrets::SecretStore;

/// Default cap for one provisional discovery session, bounding an agent that
/// accepts initialize but then hangs.
pub const DEFAULT_MODELS_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(30);

pub fn model_value_is_explicit_without_discovery(agent: &AgentConfig) -> bool {
    agent.id == KIMI_CODE_AGENT_ID
        || (agent.id == CLAUDE_CODE_AGENT_ID
            && agent.provider.as_ref().is_some_and(|provider| {
                provider.custom.is_some() || is_claude_code_profiled_provider(&provider.id)
            }))
        // codex-acp advertises codex-core's bundled OpenAI preset catalog
        // regardless of provider, while Codex itself accepts arbitrary model
        // strings, so the advertised list must not gate the operator's choice.
        || (agent.id == CODEX_AGENT_ID
            && agent.provider.as_ref().is_some_and(|provider| {
                provider.custom.is_some() || provider.id == CODEX_OPENROUTER_PROVIDER_ID
            }))
        // Hermes advertises pre-1.0 `models`/`modes` session state rather than
        // ACP v1 `configOptions`, so its advertised list reads empty here.
        || agent.id == HERMES_AGENT_ID
}

/// Spawn the configured agent, open one provisional ACP session, and
/// return the raw `session/new` response.
pub fn fetch_session_config(home: &Path, config: &Config) -> Result<NewSessionResponse> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|source| StackError::ServeIo { source })?;
    runtime.block_on(fetch_session_config_with_timeout(
        home,
        config,
        DEFAULT_MODELS_DISCOVERY_TIMEOUT,
    ))
}

/// Async variant used by the HTTP API. Timeout, request errors, and success all
/// flow through `AcpBridge::terminate_probe` so the provisional child is reaped
/// before the call returns.
pub async fn fetch_session_config_with_timeout(
    home: &Path,
    config: &Config,
    timeout_duration: Duration,
) -> Result<NewSessionResponse> {
    if let Some(path) = fixture_path(FIXTURE_CONFIG_OPTIONS_ENV) {
        let body = std::fs::read_to_string(&path).map_err(|source| StackError::ConfigRead {
            path: path.clone(),
            source,
        })?;
        let options: Vec<SessionConfigOption> =
            serde_json::from_str(&body).map_err(|source| StackError::AgentConfigProvision {
                path,
                reason: format!("ACP session config options fixture is invalid: {source}"),
            })?;
        return Ok(NewSessionResponse::new("fixture").config_options(options));
    }

    if let Some(path) = fixture_path(FIXTURE_NEW_SESSION_RESPONSE_ENV) {
        let body = std::fs::read_to_string(&path).map_err(|source| StackError::ConfigRead {
            path: path.clone(),
            source,
        })?;
        return serde_json::from_str(&body).map_err(|source| StackError::AgentConfigProvision {
            path,
            reason: format!("ACP session/new fixture is invalid: {source}"),
        });
    }

    let env = resolve_agent_env(home, config)?;
    let cwd = config
        .agent
        .cwd
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&config.workspace.root));

    let bridge = AcpBridge::spawn(
        home,
        &config.agent,
        env,
        cwd.clone(),
        Arc::new(NoopSink),
        AcpPermissionPolicy::Cancel,
        &config.workspace.sandbox,
        crate::extensions::resolve_network_provider(config).as_ref(),
        None,
    )
    .await?;
    let discovery =
        match tokio::time::timeout(timeout_duration, bridge.new_session(cwd, Vec::new())).await {
            Ok(result) => result,
            Err(_) => Err(StackError::AgentInitializeFailed {
                reason: format!("model discovery exceeded the {timeout_duration:?} timeout"),
            }),
        };
    let shutdown = bridge.terminate_probe().await;
    match (discovery, shutdown) {
        (Ok(response), Ok(_)) => Ok(response),
        (Err(err), Ok(_)) => Err(err),
        (Ok(_), Err(err)) => Err(err),
        (Err(discovery_err), Err(teardown_err)) => Err(StackError::AgentInitializeFailed {
            reason: format!(
                "model discovery failed: {discovery_err}; probe teardown also failed: {teardown_err}"
            ),
        }),
    }
}

/// Spawn the agent for its `initialize` handshake only, capture the advertised
/// capabilities, and tear the process down without creating a session.
pub fn fetch_agent_capabilities(home: &Path, config: &Config) -> Result<AgentCapabilitiesDto> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|source| StackError::ServeIo { source })?;
    runtime.block_on(fetch_agent_capabilities_async(home, config))
}

/// Async variant of [`fetch_agent_capabilities`]. No extra timeout may wrap the
/// spawn: `AcpBridge::spawn` already bounds the handshake, and wrapping the
/// future here would leak the child process on expiry instead of reaping it.
pub async fn fetch_agent_capabilities_async(
    home: &Path,
    config: &Config,
) -> Result<AgentCapabilitiesDto> {
    if let Some(path) = fixture_path(FIXTURE_AGENT_CAPABILITIES_ENV) {
        let body = std::fs::read_to_string(&path).map_err(|source| StackError::ConfigRead {
            path: path.clone(),
            source,
        })?;
        return serde_json::from_str(&body).map_err(|source| StackError::AgentInitializeFailed {
            reason: format!(
                "agent capabilities fixture at {} is invalid: {source}",
                path.display()
            ),
        });
    }

    let env = resolve_agent_env(home, config)?;
    let cwd = config
        .agent
        .cwd
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&config.workspace.root));

    let bridge = AcpBridge::spawn(
        home,
        &config.agent,
        env,
        cwd,
        Arc::new(NoopSink),
        AcpPermissionPolicy::Cancel,
        &config.workspace.sandbox,
        crate::extensions::resolve_network_provider(config).as_ref(),
        None,
    )
    .await?;
    let capabilities = bridge.capabilities().clone();
    // The handshake already succeeded, so a teardown failure is logged rather
    // than discarding a good advertisement.
    if let Err(error) = bridge.terminate_probe().await {
        tracing::warn!(%error, "capability probe teardown failed after a successful handshake");
    }
    Ok(capabilities)
}

/// Advertised string values for one category. `Model` routes through the
/// legacy-aware `session_model_values` so older agents that surface model lists
/// outside `config_options` still work.
pub fn advertised_values_for_category(
    response: &NewSessionResponse,
    category: AgentSessionConfigCategory,
) -> Result<Vec<String>> {
    match category {
        AgentSessionConfigCategory::Model => session_model_values(response),
        AgentSessionConfigCategory::Mode => session_mode_values(response),
        AgentSessionConfigCategory::Effort => {
            session_config_values(response.config_options.as_deref(), category)
        }
    }
}

/// Validate `value` against the agent's ACP-advertised values for a category.
/// Callers MUST run this before writing a model/mode/effort to disk so the
/// config never disagrees with what the harness accepts on `session/new`.
pub fn validate_advertised_value(
    response: &NewSessionResponse,
    category: AgentSessionConfigCategory,
    value: &str,
) -> Result<()> {
    match category {
        AgentSessionConfigCategory::Model => {
            session_model_selection_for_value(response, value).map(|_| ())
        }
        AgentSessionConfigCategory::Mode => {
            session_mode_selection_for_value(response, value).map(|_| ())
        }
        AgentSessionConfigCategory::Effort => {
            session_config_id_for_value(response.config_options.as_deref(), category, value)
                .map(|_| ())
        }
    }
}

pub fn resolve_advertised_model_value(
    response: &NewSessionResponse,
    provider_id: Option<&str>,
    model_id: &str,
) -> Result<String> {
    let values = session_model_values(response)?;
    let exact_is_advertised = session_model_selection_for_value(response, model_id).is_ok();
    if let Some(provider_id) = provider_id
        && exact_is_advertised
        && advertised_model_provider_matches(model_id, provider_id)
    {
        return Ok(model_id.to_owned());
    }
    if let Some(provider_id) = provider_id {
        let provider_qualified = format!("{provider_id}/{model_id}");
        if values.iter().any(|value| value == &provider_qualified)
            && session_model_selection_for_value(response, &provider_qualified).is_ok()
        {
            return Ok(provider_qualified);
        }
    }
    let mut base_matches = values
        .iter()
        .filter(|value| advertised_model_base_matches(value, provider_id, model_id))
        .cloned()
        .collect::<Vec<_>>();
    base_matches.sort();
    base_matches.dedup();
    if base_matches.len() == 1
        && session_model_selection_for_value(response, &base_matches[0]).is_ok()
    {
        return Ok(base_matches.remove(0));
    }
    if exact_is_advertised {
        return Ok(model_id.to_owned());
    }
    session_model_selection_for_value(response, model_id).map(|_| model_id.to_owned())
}

fn advertised_model_base_matches(value: &str, provider_id: Option<&str>, model_id: &str) -> bool {
    let base = value.split_once('[').map_or(value, |(base, _)| base);
    if let Some((provider, model)) = base.split_once('/') {
        return provider_id.is_none_or(|provider_id| provider == provider_id) && model == model_id;
    }
    base == model_id
}

fn advertised_model_provider(value: &str) -> Option<&str> {
    let base = value.split_once('[').map_or(value, |(base, _)| base);
    base.split_once('/').map(|(provider, _)| provider)
}

fn advertised_model_provider_matches(value: &str, provider_id: &str) -> bool {
    advertised_model_provider(value).is_some_and(|provider| provider == provider_id)
}

fn resolve_agent_env(home: &Path, config: &Config) -> Result<HashMap<String, String>> {
    if let Some(environment) = resolve_agent_environment_without_secrets(config) {
        return Ok(environment.env);
    }
    let store = SecretStore::open(home)?;
    Ok(resolve_agent_environment(config, &store)?.env)
}

struct NoopSink;

impl SessionEventSink for NoopSink {
    fn append<'a>(
        &'a self,
        _session_id: &'a str,
        _kind: &'a str,
        _payload_json: &'a str,
    ) -> futures::future::BoxFuture<'a, ()> {
        Box::pin(async {})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AgentCustomProviderConfig, AgentProviderConfig, CustomProviderApi};

    fn agent(id: &str, provider: Option<AgentProviderConfig>) -> AgentConfig {
        AgentConfig {
            id: id.to_owned(),
            name: id.to_owned(),
            command: id.to_owned(),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            expected_sha256: None,
            restart: "on-crash".to_owned(),
            mode: None,
            model: None,
            effort: None,
            config_options: Default::default(),
            harness_version: None,
            adapter: None,
            adapter_override: None,
            install: None,
            provider,
            providers: None,
            subagent: None,
            auto_update: None,
        }
    }

    fn mapped_provider(id: &str) -> Option<AgentProviderConfig> {
        Some(AgentProviderConfig {
            id: id.to_owned(),
            model: None,
            api_key_ref: None,
            custom: None,
        })
    }

    fn custom_provider() -> Option<AgentProviderConfig> {
        Some(AgentProviderConfig {
            id: "my-gateway".to_owned(),
            model: None,
            api_key_ref: None,
            custom: Some(AgentCustomProviderConfig {
                name: "My Gateway".to_owned(),
                base_url: "https://gateway.example/v1".to_owned(),
                api: CustomProviderApi::default(),
                model_name: None,
                context: 200_000,
                output_max_tokens: 64_000,
            }),
        })
    }

    #[test]
    fn codex_openrouter_and_custom_skip_discovery_validation() {
        assert!(model_value_is_explicit_without_discovery(&agent(
            "codex",
            mapped_provider("openrouter")
        )));
        assert!(model_value_is_explicit_without_discovery(&agent(
            "codex",
            custom_provider()
        )));
    }

    #[test]
    fn codex_openai_still_validates_against_advertised_models() {
        assert!(!model_value_is_explicit_without_discovery(&agent(
            "codex",
            mapped_provider("openai")
        )));
        assert!(!model_value_is_explicit_without_discovery(&agent(
            "codex", None
        )));
    }

    #[test]
    fn claude_code_profiled_and_custom_skip_discovery_validation() {
        assert!(model_value_is_explicit_without_discovery(&agent(
            "claude-code",
            mapped_provider("moonshotai")
        )));
        assert!(model_value_is_explicit_without_discovery(&agent(
            "claude-code",
            custom_provider()
        )));
    }

    #[test]
    fn hermes_skips_discovery_validation() {
        assert!(model_value_is_explicit_without_discovery(&agent(
            "hermes",
            mapped_provider("openrouter")
        )));
        assert!(model_value_is_explicit_without_discovery(&agent(
            "hermes", None
        )));
    }

    #[test]
    fn other_agents_validate_against_advertised_models() {
        assert!(!model_value_is_explicit_without_discovery(&agent(
            "opencode",
            mapped_provider("openrouter")
        )));
    }
}
