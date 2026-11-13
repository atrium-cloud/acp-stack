use super::*;

use crate::runtime::agent::provider_keys::agent_provider_id_for_provider_id;
use crate::runtime::agent::provider_keys::{
    HERMES_AGENT_ID, KIMI_MODEL_API_KEY_ENV, KIMI_MODEL_BASE_URL_ENV, KIMI_MODEL_DISPLAY_NAME_ENV,
    KIMI_MODEL_MAX_CONTEXT_SIZE_ENV, KIMI_MODEL_MAX_OUTPUT_SIZE_ENV, KIMI_MODEL_NAME_ENV,
    KIMI_MODEL_PROVIDER_TYPE_ENV, KIMI_NATIVE_PROVIDER_TYPE, env_var_for_agent_provider_id,
    hermes_launch_base_url, kimi_profile_for_provider_id, providers_for_agent,
    vendor_base_url_for_agent_provider_id,
};
use crate::runtime::agent::provider_model_catalog::hidden_provider_endpoints;

pub(crate) const KIMI_CODE_AGENT_ID: &str = "kimi";
/// The lane's own API-key ref. Production code reaches it through the provider mapping;
/// the tests below assert against it directly.
#[cfg(test)]
pub(crate) const KIMI_API_KEY_ENV: &str = "KIMI_API_KEY";
/// The row a config predating `[agent.provider]` launches on: the Kimi For Coding subscription.
pub(crate) const KIMI_DEFAULT_LANE_PROVIDER_ID: &str = "kimi-coding";

/// What the Kimi launch env derives from a mapped provider row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct KimiLane {
    pub base_url: &'static str,
    pub api_key_ref: &'static str,
    pub provider_type: &'static str,
    pub default_model: Option<&'static str>,
}

/// The lane for a mapped provider id; `None` for a provider Kimi does not run. A missing id
/// selects the subscription default lane.
pub(crate) fn kimi_lane_for_provider_id(provider_id: Option<&str>) -> Option<KimiLane> {
    let provider_id = provider_id.unwrap_or(KIMI_DEFAULT_LANE_PROVIDER_ID);
    let profile = kimi_profile_for_provider_id(provider_id)?;
    Some(KimiLane {
        base_url: vendor_base_url_for_agent_provider_id(KIMI_CODE_AGENT_ID, provider_id)?,
        api_key_ref: env_var_for_agent_provider_id(KIMI_CODE_AGENT_ID, provider_id)?,
        provider_type: profile.provider_type.as_str(),
        default_model: profile.default_model.as_deref(),
    })
}

pub(crate) const PI_AGENT_ID: &str = "pi";
pub(crate) const PI_HARNESS_COMMAND: &str = "pi";
/// The pi-acp bundle ships without Pi; it runs the `pi` this names.
pub(crate) const PI_ACP_PI_BIN_ENV: &str = "PI_ACP_PI_BIN";
/// Comma-separated Pi provider ids whose models the adapter leaves out of the session's list.
pub(crate) const PI_ACP_HIDE_PROVIDERS_ENV: &str = "PI_ACP_HIDE_PROVIDERS";
/// Agents whose launch env reads `hidden_provider_endpoints`; the endpoint probe runs for these.
pub(crate) const ENDPOINT_VERDICT_AGENT_IDS: [&str; 1] = [PI_AGENT_ID];

/// Kimi Code's launch namespace, which the runtime writes whole.
pub(crate) const KIMI_MANAGED_ENV_PREFIX: &str = "KIMI_MODEL_";
/// Launch vars the runtime owns for their agent; neither `[agent].env` nor workload env may declare them.
pub(crate) const RUNTIME_MANAGED_LAUNCH_ENV: [&str; 2] =
    [PI_ACP_PI_BIN_ENV, PI_ACP_HIDE_PROVIDERS_ENV];

pub(super) fn build_agent_process_env(
    agent: &AgentConfig,
    home: &Path,
    env: HashMap<String, String>,
) -> Result<HashMap<String, String>> {
    match agent.id.as_str() {
        KIMI_CODE_AGENT_ID => build_kimi_process_env(agent, env),
        PI_AGENT_ID => build_pi_process_env(agent, home, env),
        HERMES_AGENT_ID => build_hermes_process_env(agent, env),
        _ => Ok(env),
    }
}

/// A row can point Hermes at the host a sealed credential redeems at, through the base-URL env var
/// Hermes reads. Nothing is persisted: the value is derived at every launch.
fn build_hermes_process_env(
    agent: &AgentConfig,
    mut env: HashMap<String, String>,
) -> Result<HashMap<String, String>> {
    let Some((base_url_env, base_url)) = agent
        .provider
        .as_ref()
        .filter(|provider| provider.custom.is_none())
        .and_then(|provider| hermes_launch_base_url(&provider.id))
    else {
        return Ok(env);
    };
    // An operator-declared value would silently displace the host the credential redeems at.
    if env.contains_key(base_url_env) {
        return Err(StackError::AgentInitializeFailed {
            reason: format!(
                "Hermes Agent launch env `{base_url_env}` is runtime-managed; remove it from [agent].env"
            ),
        });
    }
    env.insert(base_url_env.to_owned(), base_url.to_owned());
    Ok(env)
}

/// Hidden endpoints come from the cached probe verdicts, so no spawn makes a network call here.
fn build_pi_process_env(
    agent: &AgentConfig,
    home: &Path,
    mut env: HashMap<String, String>,
) -> Result<HashMap<String, String>> {
    let declared = |name: &&str| env.contains_key(*name);
    if let Some(name) = RUNTIME_MANAGED_LAUNCH_ENV.into_iter().find(declared) {
        return Err(StackError::AgentInitializeFailed {
            reason: format!(
                "Pi Agent launch env `{name}` is runtime-managed; remove it from [agent].env"
            ),
        });
    }
    let hidden: Vec<&str> = hidden_provider_endpoints(home, agent)
        .into_iter()
        .filter_map(|endpoint_id| agent_provider_id_for_provider_id(PI_AGENT_ID, endpoint_id))
        .collect();
    if !hidden.is_empty() {
        env.insert(PI_ACP_HIDE_PROVIDERS_ENV.to_owned(), hidden.join(","));
    }
    let pi_path = super::spawn::resolve_command_path(PI_HARNESS_COMMAND, home).ok_or_else(
        || StackError::AgentInitializeFailed {
            reason: format!(
                "Pi Agent harness `{PI_HARNESS_COMMAND}` not found in {} or on PATH; the pi-acp adapter launches it through `{PI_ACP_PI_BIN_ENV}`",
                crate::runtime::install::local_bin_dir(home).display()
            ),
        },
    )?;
    env.insert(
        PI_ACP_PI_BIN_ENV.to_owned(),
        pi_path.to_string_lossy().into_owned(),
    );
    Ok(env)
}

fn build_kimi_process_env(
    agent: &AgentConfig,
    mut env: HashMap<String, String>,
) -> Result<HashMap<String, String>> {
    let provider = agent.provider.as_ref();
    let custom = provider.and_then(|provider| provider.custom.as_ref());
    // Resolve the lane before the runtime-managed guard so every error names
    // the credential ref the active lane reads.
    let (base_url, api_key_ref, default_model, mapped_provider_type) = if let Some(custom) = custom
    {
        let Some(api_key_ref) = provider.and_then(|provider| provider.api_key_ref.as_deref())
        else {
            return Err(StackError::AgentInitializeFailed {
                reason: "Kimi Code custom provider requires [agent.provider].api_key_ref"
                    .to_owned(),
            });
        };
        (custom.base_url.as_str(), api_key_ref, None, None)
    } else {
        let provider_id = provider.map(|provider| provider.id.as_str());
        let Some(lane) = kimi_lane_for_provider_id(provider_id) else {
            let supported = providers_for_agent(KIMI_CODE_AGENT_ID)
                .into_iter()
                .map(|summary| summary.id)
                .collect::<Vec<_>>()
                .join(", ");
            return Err(StackError::AgentInitializeFailed {
                reason: format!(
                    "Kimi Code does not support provider `{}`; supported providers are {supported}, and custom providers",
                    provider_id.unwrap_or_default(),
                ),
            });
        };
        let api_key_ref = provider
            .and_then(|provider| provider.api_key_ref.as_deref())
            .unwrap_or(lane.api_key_ref);
        (
            lane.base_url,
            api_key_ref,
            lane.default_model,
            Some(lane.provider_type),
        )
    };

    if let Some(name) = env
        .keys()
        .filter(|name| name.starts_with(KIMI_MANAGED_ENV_PREFIX))
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
    // Root-first, matching the supervisor's model-selection precedence.
    let Some(model) = agent
        .model
        .as_deref()
        .or_else(|| provider.and_then(|provider| provider.model.as_deref()))
        .or(default_model)
    else {
        return Err(StackError::AgentInitializeFailed {
            reason: match provider {
                Some(provider) if custom.is_none() => format!(
                    "Kimi Code provider `{}` has no default model; set [agent.provider].model",
                    provider.id
                ),
                _ => "Kimi Code custom provider requires a model in [agent.provider]".to_owned(),
            },
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
    if let Some(provider_type) =
        mapped_provider_type.filter(|kind| *kind != KIMI_NATIVE_PROVIDER_TYPE)
    {
        env.insert(
            KIMI_MODEL_PROVIDER_TYPE_ENV.to_owned(),
            provider_type.to_owned(),
        );
    }
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

    const KIMI_CODE_BASE_URL: &str = "https://api.kimi.com/coding/v1";
    const KIMI_CODE_GLOBAL_BASE_URL: &str = "https://api.kimi.ai/coding/v1";
    const KIMI_MOONSHOT_BASE_URL: &str = "https://api.moonshot.ai/v1";
    const KIMI_MOONSHOT_CN_BASE_URL: &str = "https://api.moonshot.cn/v1";
    const KIMI_CODE_DEFAULT_MODEL: &str = "kimi-for-coding";
    const KIMI_MOONSHOT_DEFAULT_MODEL: &str = "kimi-k3";
    const KIMI_SUBSCRIPTION_PROVIDER_IDS: [&str; 5] = [
        "kimi-coding",
        "kimi-code-plan-cn",
        "kimi-coding-plan",
        "kimi",
        "kimi-code",
    ];

    /// Kimi derivation never touches the home; the pi tests pass a real one.
    fn build_agent_process_env(
        agent: &AgentConfig,
        env: HashMap<String, String>,
    ) -> Result<HashMap<String, String>> {
        super::build_agent_process_env(agent, Path::new("/nonexistent-acp-stack-home"), env)
    }

    fn pi_agent() -> AgentConfig {
        let mut agent = kimi_agent(None);
        agent.id = PI_AGENT_ID.to_owned();
        agent.name = "Pi Agent".to_owned();
        agent.command = "pi-acp".to_owned();
        agent.args = Vec::new();
        agent.env = vec!["OPENROUTER_API_KEY".to_owned()];
        agent
    }

    fn home_with_managed_pi() -> tempfile::TempDir {
        let home = tempfile::tempdir().expect("temp home");
        let bin = crate::runtime::install::local_bin_dir(home.path());
        std::fs::create_dir_all(&bin).expect("managed bin dir");
        std::fs::write(bin.join(PI_HARNESS_COMMAND), "#!/bin/sh\n").expect("managed pi");
        home
    }

    #[test]
    fn pi_process_env_names_the_managed_pi_for_the_adapter() {
        let home = home_with_managed_pi();
        let env = HashMap::from([("OPENROUTER_API_KEY".to_owned(), "secret".to_owned())]);

        let prepared =
            super::build_agent_process_env(&pi_agent(), home.path(), env).expect("pi env");

        assert_eq!(
            prepared.get(PI_ACP_PI_BIN_ENV).map(String::as_str),
            Some(
                crate::runtime::install::local_bin_dir(home.path())
                    .join(PI_HARNESS_COMMAND)
                    .to_string_lossy()
                    .as_ref()
            )
        );
        assert_eq!(
            prepared.get("OPENROUTER_API_KEY").map(String::as_str),
            Some("secret")
        );
    }

    #[test]
    fn pi_process_env_hides_the_endpoint_the_key_cannot_use() {
        let home = home_with_managed_pi();
        let mut agent = pi_agent();
        let provider = serde_json::json!({ "id": "opencode" });
        agent.provider = Some(serde_json::from_value(provider).expect("provider"));
        let hidden = || {
            let env = super::build_agent_process_env(&agent, home.path(), HashMap::new());
            env.expect("pi env").remove(PI_ACP_HIDE_PROVIDERS_ENV)
        };
        assert_eq!(hidden(), None);

        let cache = crate::runtime::agent::provider_model_catalog::cache_path(home.path());
        std::fs::create_dir_all(cache.parent().expect("cache dir")).expect("create cache dir");
        let verdicts = r#"{"version": 2, "providers": {
            "opencode": {"fetched_at": 1, "models": [{"value": "glm-5.2"}]},
            "opencode-go": {"fetched_at": 0, "models": [], "unavailable_at": 1}}}"#;
        std::fs::write(&cache, verdicts).expect("write verdicts");
        assert_eq!(hidden().as_deref(), Some("opencode-go"));
    }

    #[test]
    fn pi_process_env_rejects_a_declared_pi_bin() {
        let home = home_with_managed_pi();
        let env = HashMap::from([(PI_ACP_PI_BIN_ENV.to_owned(), "declared".to_owned())]);

        let error = super::build_agent_process_env(&pi_agent(), home.path(), env)
            .expect_err("declared runtime-managed var must fail");

        assert!(error.to_string().contains(PI_ACP_PI_BIN_ENV), "{error}");
    }

    #[test]
    fn pi_process_env_requires_an_installed_pi() {
        let home = tempfile::tempdir().expect("temp home");
        if crate::runtime::process_runner::resolve_in_path(PI_HARNESS_COMMAND).is_some() {
            // A host `pi` on PATH makes the missing branch unobservable.
            return;
        }

        let error = super::build_agent_process_env(&pi_agent(), home.path(), HashMap::new())
            .expect_err("missing pi must fail");

        assert!(error.to_string().contains(PI_ACP_PI_BIN_ENV), "{error}");
    }

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
            config_options: Default::default(),
            harness_version: None,
            adapter: None,
            adapter_override: None,
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
                .or_else(|| {
                    kimi_lane_for_provider_id(Some(provider_id)).map(|lane| lane.api_key_ref)
                })
                .unwrap_or(KIMI_API_KEY_ENV)
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
            ("moonshotai", KIMI_MOONSHOT_BASE_URL),
            ("moonshotai-cn", KIMI_MOONSHOT_CN_BASE_URL),
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
            assert!(
                !prepared.contains_key(KIMI_MODEL_PROVIDER_TYPE_ENV),
                "the kimi wire is Kimi Code's default and is never exported"
            );
            assert!(!prepared.contains_key("MOONSHOT_API_KEY"));
        }
    }

    #[test]
    fn kimi_process_env_openai_wire_row_exports_provider_type_and_row_base() {
        let env = HashMap::from([("OPENROUTER_API_KEY".to_owned(), "secret".to_owned())]);
        let mut agent = kimi_agent_with_provider("openrouter", None);
        agent.provider.as_mut().expect("provider set").model =
            Some("moonshotai/kimi-k3".to_owned());

        let prepared = build_agent_process_env(&agent, env).expect("Kimi OpenRouter env");

        assert_eq!(
            prepared.get(KIMI_MODEL_BASE_URL_ENV).map(String::as_str),
            Some("https://openrouter.ai/api/v1")
        );
        assert_eq!(
            prepared
                .get(KIMI_MODEL_PROVIDER_TYPE_ENV)
                .map(String::as_str),
            Some("openai")
        );
        assert_eq!(
            prepared.get(KIMI_MODEL_API_KEY_ENV).map(String::as_str),
            Some("secret")
        );
        assert!(!prepared.contains_key("OPENROUTER_API_KEY"));
    }

    #[test]
    fn kimi_process_env_anthropic_wire_row_uses_the_kimi_base_without_v1() {
        let env = HashMap::from([("ANTHROPIC_API_KEY".to_owned(), "secret".to_owned())]);
        let mut agent = kimi_agent_with_provider("anthropic", None);
        agent.provider.as_mut().expect("provider set").model = Some("claude-sonnet-5".to_owned());

        let prepared = build_agent_process_env(&agent, env).expect("Kimi Anthropic env");

        assert_eq!(
            prepared.get(KIMI_MODEL_BASE_URL_ENV).map(String::as_str),
            Some("https://api.anthropic.com")
        );
        assert_eq!(
            prepared
                .get(KIMI_MODEL_PROVIDER_TYPE_ENV)
                .map(String::as_str),
            Some("anthropic")
        );
    }

    #[test]
    fn kimi_process_env_row_without_default_model_requires_a_configured_model() {
        let env = HashMap::from([("OPENROUTER_API_KEY".to_owned(), "secret".to_owned())]);
        let agent = kimi_agent_with_provider("openrouter", None);

        let error = build_agent_process_env(&agent, env).expect_err("no model must fail");

        assert!(error.to_string().contains("no default model"), "{error}");
        assert!(error.to_string().contains("openrouter"), "{error}");
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
    fn kimi_process_env_global_subscription_targets_the_global_region_host() {
        let env = HashMap::from([(KIMI_API_KEY_ENV.to_owned(), "secret".to_owned())]);
        let agent = kimi_agent_with_provider("kimi-coding-global", None);

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
        let mut agent = kimi_agent_with_provider("moonshotai", None);
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
        let agent = kimi_agent_with_provider("mistral", None);

        let error = build_agent_process_env(&agent, env).expect_err("unknown provider must fail");
        assert!(error.to_string().contains("mistral"), "{error}");
        assert!(error.to_string().contains("openrouter"), "{error}");
    }

    #[test]
    fn kimi_catalog_rows_all_resolve_to_a_launch_lane() {
        let subscription_ids: Vec<&str> = providers_for_agent(KIMI_CODE_AGENT_ID)
            .into_iter()
            .filter(|summary| summary.name == "Kimi For Coding")
            .map(|summary| summary.id)
            .collect();
        assert_eq!(subscription_ids, KIMI_SUBSCRIPTION_PROVIDER_IDS);
        for summary in providers_for_agent(KIMI_CODE_AGENT_ID) {
            let lane = kimi_lane_for_provider_id(Some(summary.id)).unwrap_or_else(|| {
                panic!(
                    "provider `{}` is offered to kimi but has no launch lane",
                    summary.id
                )
            });
            assert_eq!(
                summary.default_api_key_ref,
                Some(lane.api_key_ref),
                "{}",
                summary.id
            );
            if summary.name.starts_with("Kimi For Coding") {
                assert_eq!(lane.api_key_ref, KIMI_API_KEY_ENV, "{}", summary.id);
                assert_eq!(
                    lane.provider_type, KIMI_NATIVE_PROVIDER_TYPE,
                    "{}",
                    summary.id
                );
                assert_eq!(
                    lane.default_model,
                    Some(KIMI_CODE_DEFAULT_MODEL),
                    "{}",
                    summary.id
                );
            }
        }
        assert_eq!(
            kimi_lane_for_provider_id(None),
            kimi_lane_for_provider_id(Some(KIMI_DEFAULT_LANE_PROVIDER_ID))
        );
        assert_eq!(kimi_lane_for_provider_id(Some("mistral")), None);
    }

    fn hermes_agent(provider_id: &str, api_key_ref: &str) -> AgentConfig {
        let mut agent = kimi_agent(None);
        agent.id = HERMES_AGENT_ID.to_owned();
        agent.provider = Some(crate::config::AgentProviderConfig {
            id: provider_id.to_owned(),
            model: None,
            api_key_ref: Some(api_key_ref.to_owned()),
            custom: None,
        });
        agent
    }

    /// Hermes' default hosts for these providers are not the ones a sealed credential redeems at.
    #[test]
    fn hermes_process_env_points_a_provider_at_its_sealed_host() {
        for (provider_id, api_key_ref, base_url_env, base_url) in [
            (
                "kimi-coding",
                "KIMI_API_KEY",
                "KIMI_BASE_URL",
                "https://api.kimi.com/coding",
            ),
            (
                "stepfun",
                "STEPFUN_API_KEY",
                "STEPFUN_BASE_URL",
                "https://api.stepfun.com/v1",
            ),
        ] {
            let agent = hermes_agent(provider_id, api_key_ref);

            let prepared = build_agent_process_env(&agent, HashMap::new()).expect("Hermes env");

            assert_eq!(
                prepared.get(base_url_env).map(String::as_str),
                Some(base_url)
            );
        }
    }

    #[test]
    fn hermes_process_env_leaves_other_providers_on_their_default_host() {
        let env = HashMap::from([("OPENROUTER_API_KEY".to_owned(), "secret".to_owned())]);
        let agent = hermes_agent("openrouter", "OPENROUTER_API_KEY");

        assert_eq!(
            build_agent_process_env(&agent, env.clone()).expect("Hermes env"),
            env
        );
    }

    #[test]
    fn hermes_process_env_rejects_a_declared_base_url() {
        let env = HashMap::from([("KIMI_BASE_URL".to_owned(), "https://x".to_owned())]);
        let agent = hermes_agent("kimi-coding", "KIMI_API_KEY");

        let error = build_agent_process_env(&agent, env).expect_err("managed env must fail");

        assert!(error.to_string().contains("KIMI_BASE_URL"), "{error}");
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
