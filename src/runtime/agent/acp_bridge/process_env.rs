use super::*;

use crate::runtime::agent::agent_headless_config::HERMES_AGENT_ID;

pub(crate) const KIMI_CODE_AGENT_ID: &str = "kimi";
pub(super) const KIMI_API_KEY_ENV: &str = "KIMI_API_KEY";
pub(super) const KIMI_MODEL_API_KEY_ENV: &str = "KIMI_MODEL_API_KEY";
pub(super) const KIMI_MODEL_NAME_ENV: &str = "KIMI_MODEL_NAME";
pub(super) const KIMI_MODEL_BASE_URL_ENV: &str = "KIMI_MODEL_BASE_URL";
// Kimi Code requires a model before its ACP process can initialize. Init pins
// this default into config when `--model` is not passed, and the launch env
// falls back to it when a hand-edited config omits `agent.model`. It is the
// one id available on every subscription tier, whereas `k3` is gated to
// Moderato and above.
pub(crate) const KIMI_CODE_DEFAULT_MODEL: &str = "kimi-for-coding";
// Kimi's provider default points at the general Moonshot API. Pinning the
// first-party coding endpoint is the boundary that keeps this catalog entry
// scoped to Kimi Code rather than exposing an undeclared custom-provider lane.
pub(super) const KIMI_CODE_BASE_URL: &str = "https://api.kimi.com/coding/v1";

// acps owns MCP composition: this opt-out keeps Hermes' own config.yaml MCP
// servers from launching into acps-managed sessions. The value must be
// exactly "1"; Hermes ignores anything else.
pub(super) const HERMES_SKIP_CONFIGURED_MCP_ENV: &str = "HERMES_ACP_SKIP_CONFIGURED_MCP";

pub(super) fn build_agent_process_env(
    agent: &AgentConfig,
    mut env: HashMap<String, String>,
) -> Result<HashMap<String, String>> {
    if agent.id == HERMES_AGENT_ID {
        if env.contains_key(HERMES_SKIP_CONFIGURED_MCP_ENV) {
            return Err(StackError::AgentInitializeFailed {
                reason: format!(
                    "Hermes launch env `{HERMES_SKIP_CONFIGURED_MCP_ENV}` is runtime-managed; remove it from [agent].env"
                ),
            });
        }
        // Keep the degradation visible: combined with Hermes not advertising
        // `mcpCapabilities`, this means Hermes sessions currently run with no
        // MCP servers from either side.
        tracing::info!(
            "disabling Hermes global MCP startup ({HERMES_SKIP_CONFIGURED_MCP_ENV}=1); acps owns MCP composition"
        );
        env.insert(HERMES_SKIP_CONFIGURED_MCP_ENV.to_owned(), "1".to_owned());
        return Ok(env);
    }

    if agent.id != KIMI_CODE_AGENT_ID {
        return Ok(env);
    }

    if let Some(name) = env
        .keys()
        .filter(|name| name.starts_with("KIMI_MODEL_"))
        .min()
    {
        return Err(StackError::AgentInitializeFailed {
            reason: format!(
                "Kimi Code launch env `{name}` is runtime-managed; configure only `{KIMI_API_KEY_ENV}` in [agent].env"
            ),
        });
    }

    let api_key = env
        .remove(KIMI_API_KEY_ENV)
        .ok_or_else(|| StackError::AgentInitializeFailed {
            reason: format!(
                "Kimi Code requires `{KIMI_API_KEY_ENV}` in [agent].env so acp-stack can construct its headless launch environment"
            ),
        })?;
    if api_key.trim().is_empty() {
        return Err(StackError::AgentInitializeFailed {
            reason: format!("Kimi Code secret `{KIMI_API_KEY_ENV}` must not be empty"),
        });
    }
    let model = agent.model.as_deref().unwrap_or(KIMI_CODE_DEFAULT_MODEL);
    if model.trim().is_empty() || model.len() != model.trim().len() {
        return Err(StackError::AgentInitializeFailed {
            reason: "Kimi Code requires a non-empty, trimmed agent.model".to_owned(),
        });
    }

    env.insert(KIMI_MODEL_API_KEY_ENV.to_owned(), api_key);
    env.insert(KIMI_MODEL_NAME_ENV.to_owned(), model.to_owned());
    env.insert(
        KIMI_MODEL_BASE_URL_ENV.to_owned(),
        KIMI_CODE_BASE_URL.to_owned(),
    );
    Ok(env)
}
