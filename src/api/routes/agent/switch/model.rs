//! The `model` field of `POST /v1/agent/switch`: request validation, resolution against the
//! target's advertisement, where the value is stored, and what the response reports about it.

use super::*;

use std::path::Path;

use agent_client_protocol::schema::v1::NewSessionResponse;

use crate::config::AgentConfig;
use crate::runtime::agent::agent_headless_config::provision_agent_headless_config;
use crate::runtime::agent::model_discovery::resolve_advertised_model_value;
use crate::runtime::agent::provider_keys::agent_provider_id_for_provider_id;

/// The follow-up an agent left without a model reports. HTTP clients act on `set_model` by
/// sending a switch that names `model`.
const MODEL_FOLLOW_UP: &str = "acps agent set --model <model-id>";

const MODEL_NOT_ADVERTISED_REASON: &str = "the agent does not advertise the requested model";
const MODEL_VALUE_SHAPE_REASON: &str =
    "must be a non-empty model id without leading or trailing whitespace";

/// Refuses a blank or padded value before anything else runs. The value is never echoed.
pub(super) fn validate_requested_model_value(model: Option<&str>) -> Result<()> {
    match model {
        Some(model) if model.trim().is_empty() || model.trim().len() != model.len() => {
            Err(StackError::InvalidParam {
                field: "model",
                reason: MODEL_VALUE_SHAPE_REASON.to_owned(),
            })
        }
        _ => Ok(()),
    }
}

pub(super) fn ensure_entry_sets_model(entry: &RegistryEntry) -> Result<()> {
    if entry.set_model {
        return Ok(());
    }
    Err(StackError::InvalidParam {
        field: "model",
        reason: format!("{} does not support model selection", entry.name),
    })
}

/// Stores the model where `acps agent set --model` does: in the provider slot with the root
/// cleared when a provider is set, otherwise in the root.
pub(super) fn set_agent_model(agent: &mut AgentConfig, model: String) {
    match agent.provider.as_mut() {
        Some(provider) => {
            provider.model = Some(model);
            agent.model = None;
        }
        None => agent.model = Some(model),
    }
}

/// Spawns one provisional session against `config` and returns its `session/new` advertisement.
/// The advertised model list does not vary by applied model, so the probe applies none.
pub(super) async fn probe_advertisement(
    home: &Path,
    config: &Config,
) -> Result<NewSessionResponse> {
    fetch_session_config_with_timeout(home, config, None, DEFAULT_MODELS_DISCOVERY_TIMEOUT)
        .await
        .map(|discovered| discovered.response)
}

/// Resolves `model` against the advertisement with the agent-native provider id, the way
/// `acps agent set --model` does, so bare and provider-prefixed ids land on the same value.
pub(super) fn resolve_advertised_switch_model(
    agent: &AgentConfig,
    response: &NewSessionResponse,
    model: &str,
) -> Result<String> {
    let provider_id = agent
        .provider
        .as_ref()
        .and_then(|provider| agent_provider_id_for_provider_id(&agent.id, &provider.id));
    resolve_advertised_model_value(response, provider_id, model).map_err(|error| {
        // The resolution error quotes the value, so it stays in the local log.
        tracing::info!(agent = %agent.id, %error, "switch model is not advertised by the agent");
        StackError::InvalidParam {
            field: "model",
            reason: MODEL_NOT_ADVERTISED_REASON.to_owned(),
        }
    })
}

/// The committed model a response reports: the provider slot, else the root.
pub(super) fn committed_model(agent: &AgentConfig) -> Option<String> {
    agent
        .provider
        .as_ref()
        .and_then(|provider| provider.model.as_deref())
        .or(agent.model.as_deref())
        .filter(|model| !model.trim().is_empty())
        .map(str::to_owned)
}

/// `set_model` and its follow-up: only an agent that supports model selection and has no
/// committed model still needs one.
pub(super) fn model_follow_up(
    entry: Option<&RegistryEntry>,
    agent: &AgentConfig,
) -> (bool, Option<&'static str>) {
    let set_model = entry.is_some_and(|entry| entry.set_model) && committed_model(agent).is_none();
    (set_model, set_model.then_some(MODEL_FOLLOW_UP))
}

/// Re-provisions agent-owned config from the committed config after a same-target request wrote
/// its candidate and then failed before the config write. A failed restore is logged and the
/// caller still reports the original failure.
pub(super) fn restore_committed_agent_config(committed: &Config, home: &Path) {
    if let Err(error) = provision_agent_headless_config(committed, home) {
        tracing::error!(
            agent = %committed.agent.id,
            %error,
            "restoring agent-owned config after a failed switch failed; it may still carry the rejected selection"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent_with_provider(provider_model: Option<&str>, root_model: Option<&str>) -> AgentConfig {
        let mut config = crate::config::load_config_from_str(include_str!(
            "../../../../../tests/fixtures/valid-opencode-stack.toml"
        ))
        .expect("fixture config");
        config.agent.model = root_model.map(str::to_owned);
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "opencode-go".to_owned(),
            model: provider_model.map(str::to_owned),
            api_key_ref: Some("OPENCODE_API_KEY".to_owned()),
            custom: None,
        });
        config.agent
    }

    #[test]
    fn blank_and_padded_model_values_are_refused_without_echo() {
        for value in ["", "   ", " glm-5.3-flash ", "glm-5.3-flash\n"] {
            let error = validate_requested_model_value(Some(value)).expect_err("refused");
            let message = error.to_string();
            assert!(message.contains("`model`"), "{message}");
            assert!(!message.contains("glm-5.3-flash"), "{message}");
        }
        validate_requested_model_value(Some("glm-5.3-flash")).expect("plain id accepted");
        validate_requested_model_value(None).expect("absent accepted");
    }

    #[test]
    fn model_lands_in_the_provider_slot_and_clears_the_root() {
        let mut agent = agent_with_provider(None, Some("stale"));
        set_agent_model(&mut agent, "opencode-go/glm-5.3-flash".to_owned());
        assert_eq!(agent.model, None);
        assert_eq!(
            agent
                .provider
                .as_ref()
                .and_then(|provider| provider.model.as_deref()),
            Some("opencode-go/glm-5.3-flash")
        );

        let mut providerless = agent_with_provider(None, None);
        providerless.provider = None;
        set_agent_model(&mut providerless, "kimi-k3".to_owned());
        assert_eq!(providerless.model.as_deref(), Some("kimi-k3"));
    }

    #[test]
    fn committed_model_prefers_the_provider_slot() {
        assert_eq!(
            committed_model(&agent_with_provider(Some("slot"), Some("root"))).as_deref(),
            Some("slot")
        );
        assert_eq!(
            committed_model(&agent_with_provider(None, Some("root"))).as_deref(),
            Some("root")
        );
        assert_eq!(
            committed_model(&agent_with_provider(Some("  "), None)),
            None
        );
    }
}
