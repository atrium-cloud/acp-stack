//! Provisional ACP session helpers for model/mode discovery.
//!
//! Both `acps agent set` (CLI) and `GET /v1/models` (HTTP API) need to
//! query the configured agent for its ACP-advertised `model` and `mode`
//! `session/new` config options before letting the operator pick one.
//! That requires spawning the agent's binary, opening one short-lived
//! ACP session, reading the response's `config_options`, and shutting
//! the agent down — all in-process and synchronous from the caller's
//! POV.
//!
//! This module is the single place that owns that dance. The CLI flow
//! also honors two fixture env vars (set by the test suite) that let
//! tests bypass spawning the real agent; the API path uses the same
//! fixtures so an automated browser/UI driver can mock ACP behavior
//! without touching the production agent process.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::schema::{NewSessionResponse, SessionConfigOption};

use crate::config::Config;
use crate::error::{Result, StackError};
use crate::runtime::agent::acp_bridge::{
    AcpBridge, AgentSessionConfigCategory, SessionEventSink, session_config_id_for_value,
    session_config_values, session_model_selection_for_value, session_model_values,
};
use crate::secrets::SecretStore;

/// Operator-facing env var for short-circuiting the session/new dance
/// with a JSON array of `SessionConfigOption`s. Used by tests that
/// don't want to spawn the real agent binary.
pub const FIXTURE_CONFIG_OPTIONS_ENV: &str = "ACP_STACK_AGENT_CONFIG_OPTIONS_PATH";

/// Operator-facing env var for short-circuiting the session/new dance
/// with a full `NewSessionResponse` JSON document.
pub const FIXTURE_NEW_SESSION_RESPONSE_ENV: &str = "ACP_STACK_AGENT_NEW_SESSION_RESPONSE_PATH";

/// Default cap for a single provisional model-discovery session.
/// Healthy ACP agents return `session/new` quickly; this bounds the
/// process lifetime when an agent accepts initialize but hangs before
/// advertising config options.
pub const DEFAULT_MODELS_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(30);

pub fn development_fixture_path(name: &str) -> Option<PathBuf> {
    #[cfg(debug_assertions)]
    {
        std::env::var_os(name).map(PathBuf::from)
    }
    #[cfg(not(debug_assertions))]
    {
        let _ = name;
        None
    }
}

/// Spawn the configured agent, open one provisional ACP session, and
/// return the raw `session/new` response. Honors the two fixture env
/// vars first so tests don't have to spawn the real binary.
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

/// Async variant used by the HTTP API. Unlike the CLI wrapper, this
/// does not park discovery on a detached blocking thread: timeout,
/// request errors, and success all flow through `AcpBridge::shutdown`
/// so the provisional child process is reaped before the call returns.
pub async fn fetch_session_config_with_timeout(
    home: &Path,
    config: &Config,
    timeout_duration: Duration,
) -> Result<NewSessionResponse> {
    if let Some(path) = development_fixture_path(FIXTURE_CONFIG_OPTIONS_ENV) {
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

    if let Some(path) = development_fixture_path(FIXTURE_NEW_SESSION_RESPONSE_ENV) {
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

    let bridge =
        AcpBridge::spawn(&config.agent, env, cwd.clone(), Arc::new(NoopSink), None).await?;
    let discovery =
        match tokio::time::timeout(timeout_duration, bridge.new_session(cwd, Vec::new())).await {
            Ok(result) => result,
            Err(_) => Err(StackError::AgentInitializeFailed {
                reason: format!("model discovery exceeded the {timeout_duration:?} timeout"),
            }),
        };
    let shutdown = bridge.shutdown().await;
    match (discovery, shutdown) {
        (Ok(response), Ok(_)) => Ok(response),
        (Err(err), Ok(_)) => Err(err),
        (Ok(_), Err(err)) => Err(err),
        (Err(discovery_err), Err(shutdown_err)) => Err(StackError::AgentInitializeFailed {
            reason: format!(
                "model discovery failed: {discovery_err}; shutdown also failed: {shutdown_err}"
            ),
        }),
    }
}

/// Convenience for callers that just want the advertised string values
/// for one category. `Model` flows through the legacy-aware
/// `session_model_values` so older agents that surface model lists in
/// non-config-options shapes still work; `Mode` reads straight from
/// `config_options`.
pub fn advertised_values_for_category(
    response: &NewSessionResponse,
    category: AgentSessionConfigCategory,
) -> Result<Vec<String>> {
    match category {
        AgentSessionConfigCategory::Model => session_model_values(response),
        AgentSessionConfigCategory::Mode => {
            session_config_values(response.config_options.as_deref(), category)
        }
    }
}

/// Validate that `value` matches one of the agent's ACP-advertised
/// values for the given category. Returns `Ok(())` if accepted, or
/// `StackError::AgentConfigProvision` describing the rejection.
///
/// Both `acps agent set` and `acps init` use this before writing
/// `agent.provider.model`, `agent.model`, or `agent.mode` to disk so
/// the canonical config never disagrees with what the harness itself
/// will accept on `session/new`.
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
            session_config_id_for_value(response.config_options.as_deref(), category, value)
                .map(|_| ())
        }
    }
}

fn resolve_agent_env(home: &Path, config: &Config) -> Result<HashMap<String, String>> {
    if config.agent.env.is_empty() {
        return Ok(HashMap::new());
    }
    let store = SecretStore::open(home)?;
    let mut env = HashMap::with_capacity(config.agent.env.len());
    for name in &config.agent.env {
        let value = store.get(name)?;
        env.insert(name.clone(), value.to_owned());
    }
    Ok(env)
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
