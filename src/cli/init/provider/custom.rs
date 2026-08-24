//! The `--custom-provider` init flow, writing `[agent.provider.custom]` back
//! through canonical validation.

use super::*;

pub(crate) fn apply_custom_provider_to_config(
    args: &InitArgs,
    config: &mut Config,
    config_path: &Path,
    provider_id: String,
) -> Result<Vec<String>> {
    let interactive = prompts_enabled(args);
    let provider_name = required_init_custom_value(
        prompt::HostedPromptKind::ProviderName,
        interactive,
        "provider-name",
        args.provider_name.clone(),
    )?;
    let base_url = required_init_custom_value(
        prompt::HostedPromptKind::BaseUrl,
        interactive,
        "base-url",
        args.base_url.clone(),
    )?;
    let api_key_ref = required_init_custom_value(
        prompt::HostedPromptKind::ApiKeyRef,
        interactive,
        "api-key-ref",
        args.api_key_ref.clone(),
    )?;
    let model = required_init_custom_value(
        prompt::HostedPromptKind::Model,
        interactive,
        "model",
        args.model.clone(),
    )?;
    let model_name = args.model_name.clone().unwrap_or_else(|| model.clone());
    let api = parse_init_custom_provider_api(
        args.provider_api.as_deref(),
        default_init_custom_provider_api(&config.agent.id),
    )?;
    validate_custom_provider_api_for_agent(&config.agent.id, api, "provider-api")?;
    let context = parse_init_custom_token_limit(
        "context",
        args.context.as_deref(),
        DEFAULT_CUSTOM_MODEL_CONTEXT,
    )?;
    let output_max_tokens = parse_init_custom_token_limit(
        "output-max-tokens",
        args.output_max_tokens.as_deref(),
        DEFAULT_CUSTOM_MODEL_OUTPUT_MAX_TOKENS,
    )?;
    if !crate::config::agent_env_declares(&config.agent.env, &api_key_ref) {
        config.agent.env.push(api_key_ref.clone());
    }
    config.agent.provider = Some(AgentProviderConfig {
        id: provider_id,
        model: Some(model),
        api_key_ref: Some(api_key_ref.clone()),
        custom: Some(AgentCustomProviderConfig {
            name: provider_name,
            base_url,
            api,
            model_name: Some(model_name),
            context,
            output_max_tokens,
        }),
    });
    reconcile_kimi_lane_env_declarations(&mut config.agent);
    let canonical = config.to_canonical_toml()?;
    let validated = config::load_config_from_str(&canonical)?;
    *config = validated;
    if config
        .agent
        .provider
        .as_ref()
        .and_then(|provider| provider.custom.as_ref())
        .is_none()
    {
        return Err(StackError::AgentConfigProvision {
            path: config_path.to_path_buf(),
            reason: "custom provider config was not retained".to_owned(),
        });
    }
    Ok(vec![api_key_ref])
}

pub(crate) fn reject_reserved_custom_provider_id(provider_id: &str) -> Result<()> {
    if provider_id_is_known(provider_id) {
        return Err(StackError::InvalidParam {
            field: "provider",
            reason: format!(
                "`{provider_id}` is reserved by the mapped-provider registry; choose a distinct custom id such as `{provider_id}-1`"
            ),
        });
    }
    Ok(())
}

pub(crate) fn required_init_custom_value(
    kind: prompt::HostedPromptKind,
    interactive: bool,
    field: &'static str,
    value: Option<String>,
) -> Result<String> {
    if let Some(value) = value
        && !value.trim().is_empty()
        && value.trim().len() == value.len()
    {
        return Ok(value);
    }
    let missing = || StackError::InvalidParam {
        field,
        reason: format!("--{field} is required for custom provider init"),
    };
    match prompt::text(kind, interactive, field, true)? {
        Some(answer) => {
            let answer = answer.trim().to_owned();
            if answer.is_empty() {
                Err(missing())
            } else {
                Ok(answer)
            }
        }
        None => Err(missing()),
    }
}

pub(crate) fn default_init_custom_provider_api(agent_id: &str) -> CustomProviderApi {
    if agent_id == "codex" {
        CustomProviderApi::Responses
    } else if agent_id == CLAUDE_CODE_AGENT_ID {
        CustomProviderApi::AnthropicMessages
    } else {
        CustomProviderApi::ChatCompletions
    }
}

pub(crate) fn parse_init_custom_provider_api(
    value: Option<&str>,
    default: CustomProviderApi,
) -> Result<CustomProviderApi> {
    match value {
        None => Ok(default),
        Some("chat-completions") => Ok(CustomProviderApi::ChatCompletions),
        Some("responses") => Ok(CustomProviderApi::Responses),
        Some("anthropic-messages") => Ok(CustomProviderApi::AnthropicMessages),
        Some(_) => Err(StackError::InvalidParam {
            field: "provider-api",
            reason: "must be `chat-completions`, `responses`, or `anthropic-messages`".to_owned(),
        }),
    }
}

pub(crate) fn parse_init_custom_token_limit(
    field: &'static str,
    value: Option<&str>,
    default: u64,
) -> Result<u64> {
    let Some(value) = value else {
        return Ok(default);
    };
    if value.contains(',') {
        return Err(StackError::InvalidParam {
            field,
            reason: "must be a plain integer without commas".to_owned(),
        });
    }
    let parsed = value.parse::<u64>().map_err(|_| StackError::InvalidParam {
        field,
        reason: "must be a positive integer".to_owned(),
    })?;
    if parsed == 0 {
        return Err(StackError::InvalidParam {
            field,
            reason: "must be greater than 0".to_owned(),
        });
    }
    Ok(parsed)
}
