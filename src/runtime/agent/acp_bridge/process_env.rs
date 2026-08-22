use super::*;

use crate::runtime::agent::agent_headless_config::HERMES_AGENT_ID;

pub(crate) const KIMI_CODE_AGENT_ID: &str = "kimi";
pub(crate) const KIMI_API_KEY_ENV: &str = "KIMI_API_KEY";
pub(super) const KIMI_MODEL_API_KEY_ENV: &str = "KIMI_MODEL_API_KEY";
pub(super) const KIMI_MODEL_NAME_ENV: &str = "KIMI_MODEL_NAME";
pub(super) const KIMI_MODEL_BASE_URL_ENV: &str = "KIMI_MODEL_BASE_URL";
pub(super) const KIMI_MODEL_PROVIDER_TYPE_ENV: &str = "KIMI_MODEL_PROVIDER_TYPE";
pub(super) const KIMI_MODEL_MAX_CONTEXT_SIZE_ENV: &str = "KIMI_MODEL_MAX_CONTEXT_SIZE";
pub(super) const KIMI_MODEL_MAX_OUTPUT_SIZE_ENV: &str = "KIMI_MODEL_MAX_OUTPUT_SIZE";
pub(super) const KIMI_MODEL_DISPLAY_NAME_ENV: &str = "KIMI_MODEL_DISPLAY_NAME";
// Kimi Code requires a model before its ACP process can initialize. Init pins
// this default into config when `--model` is not passed, and the launch env
// falls back to it when a hand-edited config omits the model. It is the
// one id available on every subscription tier, whereas `k3` is gated to
// Moderato and above.
pub(crate) const KIMI_CODE_DEFAULT_MODEL: &str = "kimi-for-coding";
// The Moonshot platform bills per token and has its own model catalog, so the
// subscription-tier default does not exist there.
pub(crate) const KIMI_MOONSHOT_DEFAULT_MODEL: &str = "kimi-k3";
pub(super) const KIMI_CODE_BASE_URL: &str = "https://api.kimi.com/coding/v1";
pub(super) const KIMI_CODE_GLOBAL_BASE_URL: &str = "https://api.kimi.ai/coding/v1";
pub(super) const KIMI_MOONSHOT_BASE_URL: &str = "https://api.moonshot.ai/v1";
pub(super) const KIMI_MOONSHOT_CN_BASE_URL: &str = "https://api.moonshot.cn/v1";
// Alias ids of the "Kimi For Coding" providers.toml row. `[agent.provider]`
// stores whichever alias the operator selected, so the launch-env branch must
// recognize all of them; a test cross-checks this list against the embedded
// provider mapping.
pub(super) const KIMI_SUBSCRIPTION_PROVIDER_IDS: [&str; 5] = [
    "kimi-coding",
    "kimi-for-coding",
    "kimi-coding-plan",
    "kimi",
    "kimi-code",
];
pub(super) const KIMI_SUBSCRIPTION_GLOBAL_PROVIDER_ID: &str = "kimi-coding-global";
pub(super) const KIMI_MOONSHOT_PROVIDER_ID: &str = "moonshotai";
pub(super) const KIMI_MOONSHOT_CN_PROVIDER_ID: &str = "moonshotai-cn";

/// The (base URL, default api-key env ref, default model) triple the Kimi
/// launch env derives from the configured provider. `None` (no
/// `[agent.provider]`) keeps the historical implicit default: the
/// first-party subscription endpoint.
pub(crate) fn kimi_provider_profile(
    provider_id: Option<&str>,
) -> Option<(&'static str, &'static str, &'static str)> {
    match provider_id {
        None => Some((
            KIMI_CODE_BASE_URL,
            KIMI_API_KEY_ENV,
            KIMI_CODE_DEFAULT_MODEL,
        )),
        Some(id) if KIMI_SUBSCRIPTION_PROVIDER_IDS.contains(&id) => Some((
            KIMI_CODE_BASE_URL,
            KIMI_API_KEY_ENV,
            KIMI_CODE_DEFAULT_MODEL,
        )),
        Some(KIMI_SUBSCRIPTION_GLOBAL_PROVIDER_ID) => Some((
            KIMI_CODE_GLOBAL_BASE_URL,
            KIMI_API_KEY_ENV,
            KIMI_CODE_DEFAULT_MODEL,
        )),
        Some(KIMI_MOONSHOT_PROVIDER_ID) => Some((
            KIMI_MOONSHOT_BASE_URL,
            "MOONSHOT_API_KEY",
            KIMI_MOONSHOT_DEFAULT_MODEL,
        )),
        Some(KIMI_MOONSHOT_CN_PROVIDER_ID) => Some((
            KIMI_MOONSHOT_CN_BASE_URL,
            "MOONSHOT_API_KEY",
            KIMI_MOONSHOT_DEFAULT_MODEL,
        )),
        Some(_) => None,
    }
}

pub(crate) fn kimi_default_model_for_provider(provider_id: Option<&str>) -> &'static str {
    // The unmapped-provider fallback is only reachable for custom providers,
    // and every custom-provider write path requires an explicit model, so the
    // fallback value is never launched.
    kimi_provider_profile(provider_id)
        .map(|(_, _, model)| model)
        .unwrap_or(KIMI_CODE_DEFAULT_MODEL)
}

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

    let provider = agent.provider.as_ref();
    let custom = provider.and_then(|provider| provider.custom.as_ref());
    // Resolve the lane before the runtime-managed guard so every error names
    // the credential ref the active lane actually reads.
    let (base_url, api_key_ref, default_model) = if let Some(custom) = custom {
        let Some(api_key_ref) = provider.and_then(|provider| provider.api_key_ref.as_deref())
        else {
            return Err(StackError::AgentInitializeFailed {
                reason: "Kimi Code custom provider requires [agent.provider].api_key_ref"
                    .to_owned(),
            });
        };
        (custom.base_url.as_str(), api_key_ref, None)
    } else {
        let provider_id = provider.map(|provider| provider.id.as_str());
        let Some((base_url, default_api_key_ref, default_model)) =
            kimi_provider_profile(provider_id)
        else {
            return Err(StackError::AgentInitializeFailed {
                reason: format!(
                    "Kimi Code does not support provider `{}`; supported providers are the Kimi For Coding subscription ({}, {KIMI_SUBSCRIPTION_GLOBAL_PROVIDER_ID}), the Moonshot platform ({KIMI_MOONSHOT_PROVIDER_ID}, {KIMI_MOONSHOT_CN_PROVIDER_ID}), and custom providers",
                    provider_id.unwrap_or_default(),
                    KIMI_SUBSCRIPTION_PROVIDER_IDS.join(", "),
                ),
            });
        };
        let api_key_ref = provider
            .and_then(|provider| provider.api_key_ref.as_deref())
            .unwrap_or(default_api_key_ref);
        (base_url, api_key_ref, Some(default_model))
    };

    if let Some(name) = env
        .keys()
        .filter(|name| name.starts_with("KIMI_MODEL_"))
        .min()
    {
        return Err(StackError::AgentInitializeFailed {
            reason: format!(
                "Kimi Code launch env `{name}` is runtime-managed; configure only `{api_key_ref}` in [agent].env"
            ),
        });
    }

    let api_key = env
        .remove(api_key_ref)
        .ok_or_else(|| StackError::AgentInitializeFailed {
            reason: format!(
                "Kimi Code requires `{api_key_ref}` in [agent].env so acp-stack can construct its headless launch environment"
            ),
        })?;
    if api_key.trim().is_empty() {
        return Err(StackError::AgentInitializeFailed {
            reason: format!("Kimi Code secret `{api_key_ref}` must not be empty"),
        });
    }
    // Root-first, matching the supervisor's model-selection precedence; the
    // CLI write paths clear the losing slot so only hand-edited configs can
    // populate both.
    let Some(model) = agent
        .model
        .as_deref()
        .or_else(|| provider.and_then(|provider| provider.model.as_deref()))
        .or(default_model)
    else {
        return Err(StackError::AgentInitializeFailed {
            reason: "Kimi Code custom provider requires a model in [agent.provider]".to_owned(),
        });
    };
    if model.trim().is_empty() || model.len() != model.trim().len() {
        return Err(StackError::AgentInitializeFailed {
            reason: "Kimi Code requires a non-empty, trimmed model".to_owned(),
        });
    }

    env.insert(KIMI_MODEL_API_KEY_ENV.to_owned(), api_key);
    env.insert(KIMI_MODEL_NAME_ENV.to_owned(), model.to_owned());
    env.insert(KIMI_MODEL_BASE_URL_ENV.to_owned(), base_url.to_owned());
    if let Some(custom) = custom {
        env.insert(
            KIMI_MODEL_PROVIDER_TYPE_ENV.to_owned(),
            custom.api.as_kimi_provider_type().to_owned(),
        );
        env.insert(
            KIMI_MODEL_MAX_CONTEXT_SIZE_ENV.to_owned(),
            custom.context.to_string(),
        );
        env.insert(
            KIMI_MODEL_MAX_OUTPUT_SIZE_ENV.to_owned(),
            custom.output_max_tokens.to_string(),
        );
        if let Some(model_name) = custom.model_name.as_deref() {
            env.insert(
                KIMI_MODEL_DISPLAY_NAME_ENV.to_owned(),
                model_name.to_owned(),
            );
        }
    }
    Ok(env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AgentConfig;
    use std::collections::HashMap;

    fn kimi_agent(model: Option<&str>) -> AgentConfig {
        AgentConfig {
            id: KIMI_CODE_AGENT_ID.to_owned(),
            name: "Kimi Code".to_owned(),
            command: "kimi".to_owned(),
            args: vec!["acp".to_owned()],
            cwd: None,
            env: vec![KIMI_API_KEY_ENV.to_owned()],
            expected_sha256: None,
            restart: "on-crash".to_owned(),
            mode: None,
            model: model.map(str::to_owned),
            effort: None,
            harness_version: None,
            adapter: None,
            provider: None,
            providers: None,
            subagent: None,
            auto_update: None,
            install: None,
        }
    }

    #[test]
    fn kimi_process_env_uses_default_model_and_hides_canonical_secret_name() {
        let env = HashMap::from([(KIMI_API_KEY_ENV.to_owned(), "secret".to_owned())]);

        let prepared = build_agent_process_env(&kimi_agent(None), env).expect("Kimi env");

        assert_eq!(
            prepared.get(KIMI_MODEL_API_KEY_ENV).map(String::as_str),
            Some("secret")
        );
        assert_eq!(
            prepared.get(KIMI_MODEL_NAME_ENV).map(String::as_str),
            Some("kimi-for-coding")
        );
        assert_eq!(
            prepared.get(KIMI_MODEL_BASE_URL_ENV).map(String::as_str),
            Some(KIMI_CODE_BASE_URL)
        );
        assert!(!prepared.contains_key(KIMI_API_KEY_ENV));
    }

    fn kimi_agent_with_custom_provider(api: crate::config::CustomProviderApi) -> AgentConfig {
        let mut agent = kimi_agent(None);
        agent.model = None;
        agent.env = vec!["CUSTOM_API_KEY".to_owned()];
        agent.provider = Some(crate::config::AgentProviderConfig {
            id: "myprovider".to_owned(),
            model: Some("my-model".to_owned()),
            api_key_ref: Some("CUSTOM_API_KEY".to_owned()),
            custom: Some(crate::config::AgentCustomProviderConfig {
                name: "My Provider".to_owned(),
                base_url: "https://api.myprovider.example/v1".to_owned(),
                api,
                model_name: Some("My Model".to_owned()),
                context: 131072,
                output_max_tokens: 32768,
            }),
        });
        agent
    }

    #[test]
    fn kimi_process_env_custom_provider_maps_wire_to_provider_type() {
        use crate::config::CustomProviderApi;
        for (api, provider_type) in [
            (CustomProviderApi::ChatCompletions, "openai"),
            (CustomProviderApi::AnthropicMessages, "anthropic"),
            (CustomProviderApi::Responses, "openai_responses"),
        ] {
            let env = HashMap::from([("CUSTOM_API_KEY".to_owned(), "secret".to_owned())]);
            let agent = kimi_agent_with_custom_provider(api);

            let prepared = build_agent_process_env(&agent, env).expect("Kimi custom env");

            assert_eq!(
                prepared
                    .get(KIMI_MODEL_PROVIDER_TYPE_ENV)
                    .map(String::as_str),
                Some(provider_type)
            );
            assert_eq!(
                prepared.get(KIMI_MODEL_BASE_URL_ENV).map(String::as_str),
                Some("https://api.myprovider.example/v1")
            );
            assert_eq!(
                prepared.get(KIMI_MODEL_API_KEY_ENV).map(String::as_str),
                Some("secret")
            );
            assert_eq!(
                prepared.get(KIMI_MODEL_NAME_ENV).map(String::as_str),
                Some("my-model")
            );
            assert_eq!(
                prepared
                    .get(KIMI_MODEL_MAX_CONTEXT_SIZE_ENV)
                    .map(String::as_str),
                Some("131072")
            );
            assert_eq!(
                prepared
                    .get(KIMI_MODEL_MAX_OUTPUT_SIZE_ENV)
                    .map(String::as_str),
                Some("32768")
            );
            assert_eq!(
                prepared
                    .get(KIMI_MODEL_DISPLAY_NAME_ENV)
                    .map(String::as_str),
                Some("My Model")
            );
            assert!(!prepared.contains_key("CUSTOM_API_KEY"));
        }
    }

    #[test]
    fn kimi_process_env_custom_provider_requires_model() {
        let env = HashMap::from([("CUSTOM_API_KEY".to_owned(), "secret".to_owned())]);
        let mut agent =
            kimi_agent_with_custom_provider(crate::config::CustomProviderApi::default());
        agent.provider.as_mut().expect("provider set").model = None;

        let error =
            build_agent_process_env(&agent, env).expect_err("custom without model must fail");
        assert!(error.to_string().contains("requires a model"), "{error}");
    }

    #[test]
    fn kimi_process_env_root_model_wins_over_provider_model() {
        let env = HashMap::from([("CUSTOM_API_KEY".to_owned(), "secret".to_owned())]);
        let mut agent =
            kimi_agent_with_custom_provider(crate::config::CustomProviderApi::default());
        agent.model = Some("root-model".to_owned());

        let prepared = build_agent_process_env(&agent, env).expect("Kimi custom env");
        assert_eq!(
            prepared.get(KIMI_MODEL_NAME_ENV).map(String::as_str),
            Some("root-model")
        );
    }

    fn kimi_agent_with_provider(provider_id: &str, api_key_ref: Option<&str>) -> AgentConfig {
        let mut agent = kimi_agent(None);
        agent.env = vec![
            api_key_ref
                .unwrap_or(if provider_id.starts_with("moonshotai") {
                    "MOONSHOT_API_KEY"
                } else {
                    KIMI_API_KEY_ENV
                })
                .to_owned(),
        ];
        agent.provider = Some(crate::config::AgentProviderConfig {
            id: provider_id.to_owned(),
            model: None,
            api_key_ref: api_key_ref.map(str::to_owned),
            custom: None,
        });
        agent
    }

    #[test]
    fn kimi_process_env_moonshot_provider_targets_platform_endpoint() {
        for (provider_id, base_url) in [
            (KIMI_MOONSHOT_PROVIDER_ID, KIMI_MOONSHOT_BASE_URL),
            (KIMI_MOONSHOT_CN_PROVIDER_ID, KIMI_MOONSHOT_CN_BASE_URL),
        ] {
            let env = HashMap::from([("MOONSHOT_API_KEY".to_owned(), "secret".to_owned())]);
            let agent = kimi_agent_with_provider(provider_id, None);

            let prepared = build_agent_process_env(&agent, env).expect("Kimi Moonshot env");

            assert_eq!(
                prepared.get(KIMI_MODEL_API_KEY_ENV).map(String::as_str),
                Some("secret")
            );
            assert_eq!(
                prepared.get(KIMI_MODEL_NAME_ENV).map(String::as_str),
                Some(KIMI_MOONSHOT_DEFAULT_MODEL)
            );
            assert_eq!(
                prepared.get(KIMI_MODEL_BASE_URL_ENV).map(String::as_str),
                Some(base_url)
            );
            assert!(!prepared.contains_key("MOONSHOT_API_KEY"));
        }
    }

    #[test]
    fn kimi_process_env_subscription_aliases_keep_coding_endpoint() {
        for provider_id in KIMI_SUBSCRIPTION_PROVIDER_IDS {
            let env = HashMap::from([(KIMI_API_KEY_ENV.to_owned(), "secret".to_owned())]);
            let agent = kimi_agent_with_provider(provider_id, None);

            let prepared = build_agent_process_env(&agent, env).expect("Kimi subscription env");

            assert_eq!(
                prepared.get(KIMI_MODEL_BASE_URL_ENV).map(String::as_str),
                Some(KIMI_CODE_BASE_URL)
            );
            assert_eq!(
                prepared.get(KIMI_MODEL_NAME_ENV).map(String::as_str),
                Some(KIMI_CODE_DEFAULT_MODEL)
            );
        }
    }

    #[test]
    fn kimi_process_env_global_subscription_targets_global_endpoint() {
        let env = HashMap::from([(KIMI_API_KEY_ENV.to_owned(), "secret".to_owned())]);
        let agent = kimi_agent_with_provider(KIMI_SUBSCRIPTION_GLOBAL_PROVIDER_ID, None);

        let prepared = build_agent_process_env(&agent, env).expect("Kimi global env");

        assert_eq!(
            prepared.get(KIMI_MODEL_BASE_URL_ENV).map(String::as_str),
            Some(KIMI_CODE_GLOBAL_BASE_URL)
        );
        assert_eq!(
            prepared.get(KIMI_MODEL_API_KEY_ENV).map(String::as_str),
            Some("secret")
        );
        assert_eq!(
            prepared.get(KIMI_MODEL_NAME_ENV).map(String::as_str),
            Some(KIMI_CODE_DEFAULT_MODEL)
        );
    }

    #[test]
    fn kimi_process_env_provider_model_wins_over_defaults() {
        let env = HashMap::from([("MOONSHOT_API_KEY".to_owned(), "secret".to_owned())]);
        let mut agent = kimi_agent_with_provider(KIMI_MOONSHOT_PROVIDER_ID, None);
        agent.provider.as_mut().expect("provider set").model = Some("kimi-k2.5".to_owned());

        let prepared = build_agent_process_env(&agent, env).expect("Kimi Moonshot env");

        assert_eq!(
            prepared.get(KIMI_MODEL_NAME_ENV).map(String::as_str),
            Some("kimi-k2.5")
        );
    }

    #[test]
    fn kimi_process_env_rejects_unknown_provider() {
        let env = HashMap::from([(KIMI_API_KEY_ENV.to_owned(), "secret".to_owned())]);
        let agent = kimi_agent_with_provider("openrouter", None);

        let error = build_agent_process_env(&agent, env).expect_err("unknown provider must fail");
        assert!(error.to_string().contains("openrouter"), "{error}");
    }

    #[test]
    fn kimi_subscription_alias_list_matches_embedded_provider_mapping() {
        use crate::runtime::agent::provider_keys::providers_for_agent;
        let subscription_ids: Vec<&str> = providers_for_agent(KIMI_CODE_AGENT_ID)
            .into_iter()
            .filter(|summary| summary.name == "Kimi For Coding")
            .map(|summary| summary.id)
            .collect();
        assert_eq!(subscription_ids, KIMI_SUBSCRIPTION_PROVIDER_IDS);
        for summary in providers_for_agent(KIMI_CODE_AGENT_ID) {
            let expected_ref = if summary.name.starts_with("Kimi For Coding") {
                KIMI_API_KEY_ENV
            } else {
                "MOONSHOT_API_KEY"
            };
            assert_eq!(
                summary.default_api_key_ref,
                Some(expected_ref),
                "{}",
                summary.id
            );
            assert!(
                kimi_provider_profile(Some(summary.id)).is_some(),
                "provider `{}` is offered to kimi but has no launch-env profile",
                summary.id
            );
        }
    }

    #[test]
    fn kimi_process_env_uses_explicit_model() {
        let env = HashMap::from([(KIMI_API_KEY_ENV.to_owned(), "secret".to_owned())]);

        let prepared = build_agent_process_env(&kimi_agent(Some("kimi-for-coding-highspeed")), env)
            .expect("Kimi env");

        assert_eq!(
            prepared.get(KIMI_MODEL_NAME_ENV).map(String::as_str),
            Some("kimi-for-coding-highspeed")
        );
    }

    #[test]
    fn kimi_process_env_requires_canonical_api_key() {
        let error = build_agent_process_env(&kimi_agent(None), HashMap::new())
            .expect_err("missing Kimi key must fail");

        assert!(error.to_string().contains(KIMI_API_KEY_ENV), "{error}");
    }

    #[test]
    fn kimi_process_env_rejects_empty_api_key() {
        let env = HashMap::from([(KIMI_API_KEY_ENV.to_owned(), "  ".to_owned())]);

        let error =
            build_agent_process_env(&kimi_agent(None), env).expect_err("empty Kimi key must fail");

        assert!(error.to_string().contains("must not be empty"), "{error}");
    }

    #[test]
    fn kimi_process_env_rejects_runtime_managed_values() {
        for name in [
            KIMI_MODEL_API_KEY_ENV,
            KIMI_MODEL_NAME_ENV,
            KIMI_MODEL_BASE_URL_ENV,
        ] {
            let env = HashMap::from([
                (KIMI_API_KEY_ENV.to_owned(), "secret".to_owned()),
                (name.to_owned(), "override".to_owned()),
            ]);

            let error = build_agent_process_env(&kimi_agent(None), env)
                .expect_err("managed Kimi env must fail");
            assert!(error.to_string().contains(name), "{error}");
        }
    }

    #[test]
    fn hermes_process_env_scopes_out_configured_mcp() {
        let mut agent = kimi_agent(None);
        agent.id = "hermes".to_owned();
        agent.env = vec!["OPENROUTER_API_KEY".to_owned()];
        let env = HashMap::from([("OPENROUTER_API_KEY".to_owned(), "secret".to_owned())]);

        let prepared = build_agent_process_env(&agent, env).expect("Hermes env");

        assert_eq!(
            prepared
                .get(HERMES_SKIP_CONFIGURED_MCP_ENV)
                .map(String::as_str),
            Some("1")
        );
        assert_eq!(
            prepared.get("OPENROUTER_API_KEY").map(String::as_str),
            Some("secret")
        );
    }

    #[test]
    fn hermes_process_env_rejects_operator_declared_mcp_skip() {
        let mut agent = kimi_agent(None);
        agent.id = "hermes".to_owned();
        let env = HashMap::from([(HERMES_SKIP_CONFIGURED_MCP_ENV.to_owned(), "0".to_owned())]);

        let error = build_agent_process_env(&agent, env).expect_err("managed Hermes env must fail");
        assert!(
            error.to_string().contains(HERMES_SKIP_CONFIGURED_MCP_ENV),
            "{error}"
        );
    }

    #[test]
    fn other_agent_process_env_is_unchanged() {
        let mut agent = kimi_agent(None);
        agent.id = "opencode".to_owned();
        let env = HashMap::from([("OPENAI_API_KEY".to_owned(), "secret".to_owned())]);

        assert_eq!(
            build_agent_process_env(&agent, env.clone()).expect("OpenCode env"),
            env
        );
    }
}
