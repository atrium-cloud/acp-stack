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
    CLAUDE_CODE_AGENT_ID, CODEX_AGENT_ID, GOOSE_AGENT_ID, is_claude_code_profiled_provider,
    models_url_for_provider_id, resolve_agent_environment,
    resolve_agent_environment_without_secrets,
};
use crate::runtime::agent::provider_model_catalog::cached_models;
use crate::secrets::SecretStore;

/// Default cap for one provisional discovery session, bounding an agent that
/// accepts initialize but then hangs.
pub const DEFAULT_MODELS_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(30);

/// Values codex's `model_reasoning_effort` parser accepts. OpenRouter also reports `max`, which
/// codex has no variant for; pinning it would fail codex's config parse at startup.
pub const CODEX_REASONING_EFFORTS: [&str; 6] =
    ["none", "minimal", "low", "medium", "high", "xhigh"];

/// codex-acp advertises `reasoning_effort` only for codex-core's OpenAI presets, so an OpenRouter
/// model's effort is validated against the provider catalog and pinned in codex's config.toml
/// instead of set over ACP.
pub fn effort_value_is_explicit_without_discovery(agent: &AgentConfig) -> bool {
    agent.id == CODEX_AGENT_ID
        && agent.provider.as_ref().is_some_and(|provider| {
            provider.custom.is_none() && provider.id == CODEX_OPENROUTER_PROVIDER_ID
        })
}

/// Drop catalog effort values the harness cannot express.
pub fn harness_accepted_efforts(agent: &AgentConfig, efforts: Vec<String>) -> Vec<String> {
    if agent.id != CODEX_AGENT_ID {
        return efforts;
    }
    efforts
        .into_iter()
        .filter(|effort| CODEX_REASONING_EFFORTS.contains(&effort.as_str()))
        .collect()
}

/// Reasoning-effort values the provider catalog reports for the configured model, filtered to
/// what the harness accepts. Errors name the missing piece: no model, no catalog, or a model the
/// provider marks as effort-less.
pub fn catalog_effort_values(home: &Path, config: &Config) -> Result<Vec<String>> {
    let agent = &config.agent;
    let Some(provider) = agent.provider.as_ref() else {
        return Err(StackError::InvalidParam {
            field: "effort",
            reason: format!(
                "{} takes its reasoning effort from the provider catalog and has no provider configured",
                agent.name
            ),
        });
    };
    let Some(model) = configured_model_value(agent) else {
        return Err(StackError::InvalidParam {
            field: "effort",
            reason: format!(
                "{} needs a configured model before its reasoning-effort values can be resolved; select one first with `acps agent provider use <provider> --model <id>` (or `--model <id>` on init)",
                agent.name
            ),
        });
    };
    let Some(models) = cached_models(home, &provider.id) else {
        return Err(StackError::InvalidParam {
            field: "effort",
            reason: format!(
                "no `{}` model catalog is available to resolve reasoning-effort values for `{model}`",
                provider.id
            ),
        });
    };
    let efforts = models
        .into_iter()
        .find(|entry| entry.value == model)
        .map(|entry| entry.efforts)
        .unwrap_or_default();
    let efforts = harness_accepted_efforts(agent, efforts);
    if efforts.is_empty() {
        return Err(StackError::InvalidParam {
            field: "effort",
            reason: format!(
                "the `{}` catalog reports no reasoning-effort values for `{model}`",
                provider.id
            ),
        });
    }
    Ok(efforts)
}

/// Catalog counterpart of `validate_advertised_value` for the effort category.
pub fn validate_catalog_effort_value(home: &Path, config: &Config, value: &str) -> Result<()> {
    let values = catalog_effort_values(home, config)?;
    if values.iter().any(|candidate| candidate == value) {
        return Ok(());
    }
    Err(StackError::InvalidParam {
        field: "effort",
        reason: format!(
            "the provider catalog does not list `{value}` as an available reasoning effort for the configured model; catalog efforts: [{}]",
            values.join(", ")
        ),
    })
}

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
        // hermes-agent-acp advertises composite `provider/model` value ids built from
        // the gateway catalog, while the configured model is the bare id pinned
        // through config.yaml, so the advertised list cannot gate the choice.
        || agent.id == HERMES_AGENT_ID
        // goose reads `GOOSE_MODEL` from its own config before it can answer
        // `session/new`, so no provisional session can produce a list to gate
        // the choice; the model comes from the provider catalog or verbatim.
        || agent.id == GOOSE_AGENT_ID
}

/// Whether the configured model reaches the harness only through its on-disk config, making a
/// `session/set_config_option` model set redundant at best. goose is the exception among the
/// explicit-model harnesses: it also accepts a live model switch over ACP.
pub fn model_applies_from_disk_only(agent: &AgentConfig) -> bool {
    model_value_is_explicit_without_discovery(agent) && agent.id != GOOSE_AGENT_ID
}

/// goose resolves its model from its own config while starting a session, so a provisional
/// discovery session cannot be spawned for it until a model is configured.
pub fn session_new_requires_a_configured_model(agent: &AgentConfig) -> bool {
    agent.id == GOOSE_AGENT_ID
}

/// Configured model with session-create precedence: a root `agent.model` outranks the provider slot.
pub fn configured_model_value(agent: &AgentConfig) -> Option<&str> {
    agent
        .model
        .as_deref()
        .or_else(|| {
            agent
                .provider
                .as_ref()
                .and_then(|provider| provider.model.as_deref())
        })
        .filter(|model| !model.trim().is_empty())
}

/// True when a discovery spawn would reach an agent that cannot answer `session/new` yet. goose
/// reads the model from its provisioned config, which carries the provider slot only, so a root
/// `agent.model` does not unblock it.
pub fn discovery_is_blocked_without_a_model(agent: &AgentConfig) -> bool {
    session_new_requires_a_configured_model(agent)
        && agent
            .provider
            .as_ref()
            .and_then(|provider| provider.model.as_deref())
            .is_none_or(|model| model.trim().is_empty())
}

fn missing_model_for_discovery_error(agent: &AgentConfig) -> StackError {
    StackError::InvalidParam {
        field: "model",
        reason: format!(
            "{} resolves its model while starting a session, so its modes and reasoning-effort values cannot be discovered before one is configured; select a model first (`--model <id>` on init, or `acps agent provider use <provider> --model <id>`)",
            agent.name
        ),
    }
}

/// Model values the provider catalog reports, for agents whose model list cannot come from a
/// provisional ACP session. Errors name the missing piece: no provider, a custom provider, a
/// provider that publishes no listing endpoint, or no cached catalog.
pub fn catalog_model_values(home: &Path, config: &Config) -> Result<Vec<String>> {
    let agent = &config.agent;
    let Some(provider) = agent.provider.as_ref() else {
        return Err(StackError::InvalidParam {
            field: "model",
            reason: format!(
                "{} takes its model list from the provider catalog and has no provider configured",
                agent.name
            ),
        });
    };
    if provider.custom.is_some() {
        return Err(StackError::InvalidParam {
            field: "model",
            reason: format!(
                "custom provider `{}` publishes no model catalog; pass the model id explicitly",
                provider.id
            ),
        });
    }
    if models_url_for_provider_id(&provider.id).is_none() {
        return Err(StackError::InvalidParam {
            field: "model",
            reason: format!(
                "provider `{}` publishes no model listing endpoint; pass the model id explicitly",
                provider.id
            ),
        });
    }
    let Some(models) = cached_models(home, &provider.id) else {
        return Err(StackError::InvalidParam {
            field: "model",
            reason: format!("no `{}` model catalog is available", provider.id),
        });
    };
    Ok(models.into_iter().map(|model| model.value).collect())
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

    // Fail before the spawn rather than after the harness rejects `session/new`
    // for a reason only it can name.
    if discovery_is_blocked_without_a_model(&config.agent) {
        return Err(missing_model_for_discovery_error(&config.agent));
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
        &config.workspace.default_shell,
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
        &config.workspace.default_shell,
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
            "claude",
            mapped_provider("moonshotai")
        )));
        assert!(model_value_is_explicit_without_discovery(&agent(
            "claude",
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

    fn goose_config(provider_model: Option<&str>) -> Config {
        let mut config = crate::config::load_config_from_str(include_str!(
            "../../../tests/fixtures/valid-opencode-stack.toml"
        ))
        .expect("fixture config");
        config.agent.id = GOOSE_AGENT_ID.to_owned();
        config.agent.name = "Goose".to_owned();
        config.agent.command = "goose".to_owned();
        config.agent.provider = Some(AgentProviderConfig {
            id: "openrouter".to_owned(),
            model: provider_model.map(str::to_owned),
            api_key_ref: Some("OPENROUTER_API_KEY".to_owned()),
            custom: None,
        });
        config
    }

    /// Writes the on-disk catalog cache the way a successful fetch would.
    fn seed_provider_catalog(home: &Path, provider_id: &str, models: &[&str]) {
        let path = crate::runtime::agent::provider_model_catalog::cache_path(home);
        std::fs::create_dir_all(path.parent().expect("cache parent")).expect("create cache dir");
        let fetched_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_secs();
        let entries: Vec<serde_json::Value> = models
            .iter()
            .map(|value| serde_json::json!({ "value": value }))
            .collect();
        let body = serde_json::json!({
            "version": 2,
            "providers": { provider_id: { "fetched_at": fetched_at, "models": entries } }
        });
        std::fs::write(&path, body.to_string()).expect("write cache");
    }

    #[test]
    fn goose_takes_its_model_verbatim_and_still_applies_it_over_acp() {
        let goose = agent(GOOSE_AGENT_ID, mapped_provider("openrouter"));
        assert!(model_value_is_explicit_without_discovery(&goose));
        assert!(
            !model_applies_from_disk_only(&goose),
            "goose accepts a live model switch, so the ACP set must not be skipped"
        );
        assert!(model_applies_from_disk_only(&agent(
            "codex",
            mapped_provider("openrouter")
        )));
    }

    #[test]
    fn goose_discovery_is_blocked_until_a_provider_model_is_configured() {
        assert!(session_new_requires_a_configured_model(&agent(
            GOOSE_AGENT_ID,
            mapped_provider("openrouter")
        )));
        assert!(!session_new_requires_a_configured_model(&agent(
            "opencode",
            mapped_provider("openrouter")
        )));
        assert!(discovery_is_blocked_without_a_model(
            &goose_config(None).agent
        ));
        assert!(discovery_is_blocked_without_a_model(
            &goose_config(Some("  ")).agent
        ));
        assert!(!discovery_is_blocked_without_a_model(
            &goose_config(Some("openrouter/model-a")).agent
        ));
        assert!(!discovery_is_blocked_without_a_model(&agent(
            "opencode",
            mapped_provider("openrouter")
        )));
    }

    #[test]
    fn a_discovery_session_is_refused_before_the_model_less_agent_is_spawned() {
        let home = tempfile::tempdir().expect("tempdir");

        let error = fetch_session_config(home.path(), &goose_config(None))
            .expect_err("a model-less goose must never be spawned for discovery");

        assert!(
            error
                .to_string()
                .contains("resolves its model while starting a session"),
            "{error}"
        );
    }

    #[test]
    fn catalog_model_values_come_from_the_cached_provider_catalog() {
        let home = tempfile::tempdir().expect("tempdir");
        seed_provider_catalog(
            home.path(),
            "openrouter",
            &["openrouter/model-a", "openrouter/model-b"],
        );

        let values =
            catalog_model_values(home.path(), &goose_config(None)).expect("catalog values");

        assert_eq!(values, vec!["openrouter/model-a", "openrouter/model-b"]);
    }

    #[test]
    fn catalog_model_values_name_the_missing_piece() {
        let home = tempfile::tempdir().expect("tempdir");
        let mut without_catalog = goose_config(None);
        without_catalog
            .agent
            .provider
            .as_mut()
            .expect("provider")
            .id = "openai".to_owned();

        let error = catalog_model_values(home.path(), &without_catalog)
            .expect_err("openai publishes no listing endpoint for the catalog lane");
        assert!(
            error.to_string().contains("no model listing endpoint"),
            "{error}"
        );

        let error = catalog_model_values(home.path(), &goose_config(None))
            .expect_err("an unseeded cache has no catalog");
        assert!(
            error.to_string().contains("no `openrouter` model catalog"),
            "{error}"
        );

        let mut without_provider = goose_config(None);
        without_provider.agent.provider = None;
        let error = catalog_model_values(home.path(), &without_provider)
            .expect_err("no provider, no catalog");
        assert!(
            error.to_string().contains("no provider configured"),
            "{error}"
        );

        let mut custom = goose_config(None);
        custom.agent.provider = custom_provider();
        let error = catalog_model_values(home.path(), &custom)
            .expect_err("a custom provider publishes no catalog");
        assert!(
            error
                .to_string()
                .contains("custom provider `my-gateway` publishes no model catalog"),
            "{error}"
        );
    }

    #[test]
    fn other_agents_validate_against_advertised_models() {
        assert!(!model_value_is_explicit_without_discovery(&agent(
            "opencode",
            mapped_provider("openrouter")
        )));
    }

    #[test]
    fn only_codex_openrouter_takes_effort_from_the_catalog() {
        assert!(effort_value_is_explicit_without_discovery(&agent(
            "codex",
            mapped_provider("openrouter")
        )));
        assert!(!effort_value_is_explicit_without_discovery(&agent(
            "codex",
            mapped_provider("openai")
        )));
        assert!(!effort_value_is_explicit_without_discovery(&agent(
            "codex",
            custom_provider()
        )));
        assert!(!effort_value_is_explicit_without_discovery(&agent(
            "codex", None
        )));
        assert!(!effort_value_is_explicit_without_discovery(&agent(
            "opencode",
            mapped_provider("openrouter")
        )));
    }

    #[test]
    fn codex_drops_catalog_efforts_it_cannot_parse() {
        let efforts = ["max", "xhigh", "high", "medium", "low", "minimal", "none"]
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        assert_eq!(
            harness_accepted_efforts(
                &agent("codex", mapped_provider("openrouter")),
                efforts.clone()
            ),
            vec!["xhigh", "high", "medium", "low", "minimal", "none"]
        );
        assert_eq!(
            harness_accepted_efforts(
                &agent("opencode", mapped_provider("openrouter")),
                efforts.clone()
            ),
            efforts
        );
    }
}
