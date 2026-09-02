use super::*;

use crate::runtime::agent::agent_headless_config::reroute_base_url;
use crate::runtime::agent::provider_keys::{
    HERMES_AGENT_ID, agent_provider_id_for_provider_id, hermes_base_url_env_for_native_provider_id,
    resolve_base_url_template, vendor_base_url_for_agent_provider_id,
};

pub(crate) const KIMI_CODE_AGENT_ID: &str = "kimi";
pub(crate) const KIMI_API_KEY_ENV: &str = "KIMI_API_KEY";
pub(super) const KIMI_MODEL_API_KEY_ENV: &str = "KIMI_MODEL_API_KEY";
pub(super) const KIMI_MODEL_NAME_ENV: &str = "KIMI_MODEL_NAME";
pub(super) const KIMI_MODEL_BASE_URL_ENV: &str = "KIMI_MODEL_BASE_URL";
pub(super) const KIMI_MODEL_PROVIDER_TYPE_ENV: &str = "KIMI_MODEL_PROVIDER_TYPE";
pub(super) const KIMI_MODEL_MAX_CONTEXT_SIZE_ENV: &str = "KIMI_MODEL_MAX_CONTEXT_SIZE";
pub(super) const KIMI_MODEL_MAX_OUTPUT_SIZE_ENV: &str = "KIMI_MODEL_MAX_OUTPUT_SIZE";
pub(super) const KIMI_MODEL_DISPLAY_NAME_ENV: &str = "KIMI_MODEL_DISPLAY_NAME";
// Kimi Code requires a model before its ACP process can initialize. This id is
// available on every subscription tier, whereas `k3` is gated to Moderato up.
pub(crate) const KIMI_CODE_DEFAULT_MODEL: &str = "kimi-for-coding";
// The Moonshot platform has its own catalog; the subscription-tier default does
// not exist there.
pub(crate) const KIMI_MOONSHOT_DEFAULT_MODEL: &str = "kimi-k3";
pub(super) const KIMI_CODE_BASE_URL: &str = "https://api.kimi.com/coding/v1";
pub(super) const KIMI_CODE_GLOBAL_BASE_URL: &str = "https://api.kimi.com/coding/v1";
pub(super) const KIMI_MOONSHOT_BASE_URL: &str = "https://api.moonshot.ai/v1";
pub(super) const KIMI_MOONSHOT_CN_BASE_URL: &str = "https://api.moonshot.cn/v1";
// Alias ids of the "Kimi For Coding" providers.toml row; `[agent.provider]`
// stores whichever one the operator selected.
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
/// launch env derives from the configured provider.
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
    // The fallback is only reachable for custom providers, which always require
    // an explicit model, so this value is never launched.
    kimi_provider_profile(provider_id)
        .map(|(_, _, model)| model)
        .unwrap_or(KIMI_CODE_DEFAULT_MODEL)
}

pub(crate) const PI_AGENT_ID: &str = "pi";
pub(crate) const PI_HARNESS_COMMAND: &str = "pi";
/// The pi-acp bundle ships without Pi; it runs the `pi` this names.
pub(crate) const PI_ACP_PI_BIN_ENV: &str = "PI_ACP_PI_BIN";

pub(crate) const ANTIGRAVITY_AGENT_ID: &str = "antigravity";
/// The Antigravity CLI's Gemini endpoint setting: a service root, so an override origin is the
/// whole value.
pub(crate) const ANTIGRAVITY_BASE_URL_ENV: &str = "GOOGLE_GEMINI_BASE_URL";

pub(super) fn build_agent_process_env(
    agent: &AgentConfig,
    home: &Path,
    env: HashMap<String, String>,
) -> Result<HashMap<String, String>> {
    match agent.id.as_str() {
        KIMI_CODE_AGENT_ID => {
            let endpoint = crate::secrets::managed_provider_endpoint_override_for_home(home)?;
            build_kimi_process_env(agent, env, endpoint.as_ref())
        }
        PI_AGENT_ID => build_pi_process_env(home, env),
        ANTIGRAVITY_AGENT_ID => {
            let endpoint = crate::secrets::managed_provider_endpoint_override_for_home(home)?;
            build_antigravity_process_env(env, endpoint.as_ref())
        }
        HERMES_AGENT_ID => {
            let endpoint = crate::secrets::managed_provider_endpoint_override_for_home(home)?;
            build_hermes_process_env(agent, env, endpoint.as_ref())
        }
        _ => Ok(env),
    }
}

/// A mapped Hermes provider whose native overlay declares a base-URL env var carries its endpoint
/// override there, so `config.yaml` keeps the native provider id. Nothing is persisted: the value
/// is derived from the stored override at every launch, so clearing the override stops exporting
/// it and the overlay falls back to its vendor default.
fn build_hermes_process_env(
    agent: &AgentConfig,
    mut env: HashMap<String, String>,
    endpoint: Option<&crate::secrets::ProviderEndpointOverride>,
) -> Result<HashMap<String, String>> {
    // A custom provider carries its endpoint on the managed named entry in config.yaml.
    let Some(provider) = agent
        .provider
        .as_ref()
        .filter(|provider| provider.custom.is_none())
    else {
        return Ok(env);
    };
    let Some(base_url_env) = agent_provider_id_for_provider_id(HERMES_AGENT_ID, &provider.id)
        .and_then(hermes_base_url_env_for_native_provider_id)
    else {
        return Ok(env);
    };
    // Guarded even without an override: an operator-declared value would otherwise silently
    // displace the vendor endpoint acp-stack provisioned the lane for.
    if env.contains_key(base_url_env) {
        return Err(StackError::AgentInitializeFailed {
            reason: format!(
                "Hermes Agent launch env `{base_url_env}` is runtime-managed; remove it from [agent].env"
            ),
        });
    }
    let Some(endpoint) = endpoint.filter(|endpoint| endpoint.provider_id == provider.id) else {
        return Ok(env);
    };
    let Some(vendor_base_url) =
        vendor_base_url_for_agent_provider_id(HERMES_AGENT_ID, &provider.id)
    else {
        return Err(StackError::AgentInitializeFailed {
            reason: format!(
                "Hermes Agent provider `{}` declares no vendor base URL, so its endpoint override cannot be composed",
                provider.id
            ),
        });
    };
    let vendor_base_url = resolve_base_url_template(vendor_base_url, &endpoint.companion_values)?;
    env.insert(
        base_url_env.to_owned(),
        reroute_base_url(&endpoint.base_url, &vendor_base_url)?,
    );
    Ok(env)
}

/// Antigravity has no provider selection; the override names the provider its credential is
/// stored under, and the rerouted base is the bare origin.
fn build_antigravity_process_env(
    mut env: HashMap<String, String>,
    endpoint: Option<&crate::secrets::ProviderEndpointOverride>,
) -> Result<HashMap<String, String>> {
    if env.contains_key(ANTIGRAVITY_BASE_URL_ENV) {
        return Err(StackError::AgentInitializeFailed {
            reason: format!(
                "Antigravity launch env `{ANTIGRAVITY_BASE_URL_ENV}` is runtime-managed; remove it from [agent].env"
            ),
        });
    }
    let Some(endpoint) = endpoint else {
        return Ok(env);
    };
    let Some(vendor_base_url) =
        vendor_base_url_for_agent_provider_id(ANTIGRAVITY_AGENT_ID, &endpoint.provider_id)
    else {
        return Err(StackError::AgentInitializeFailed {
            reason: format!(
                "Antigravity cannot route provider `{}` through a custom endpoint",
                endpoint.provider_id
            ),
        });
    };
    env.insert(
        ANTIGRAVITY_BASE_URL_ENV.to_owned(),
        reroute_base_url(&endpoint.base_url, vendor_base_url)?,
    );
    Ok(env)
}

fn build_pi_process_env(
    home: &Path,
    mut env: HashMap<String, String>,
) -> Result<HashMap<String, String>> {
    if env.contains_key(PI_ACP_PI_BIN_ENV) {
        return Err(StackError::AgentInitializeFailed {
            reason: format!(
                "Pi Agent launch env `{PI_ACP_PI_BIN_ENV}` is runtime-managed; remove it from [agent].env"
            ),
        });
    }
    // A bare name never consults the cwd argument, so `home` stands in for it.
    let pi_path = super::spawn::resolve_command_path(PI_HARNESS_COMMAND, home, home).ok_or_else(
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
    endpoint: Option<&crate::secrets::ProviderEndpointOverride>,
) -> Result<HashMap<String, String>> {
    let provider = agent.provider.as_ref();
    let custom = provider.and_then(|provider| provider.custom.as_ref());
    // Resolve the lane before the runtime-managed guard so every error names
    // the credential ref the active lane reads.
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
    // Root-first, matching the supervisor's model-selection precedence.
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

    // The lane's vendor base keeps its path behind the override origin. The override may be
    // stored under any alias of the active lane (a provider-less config runs the Kimi For
    // Coding lane), so lanes are compared by their base rather than by id.
    let override_targets_active_lane =
        |endpoint: &&crate::secrets::ProviderEndpointOverride| match provider {
            Some(provider) if provider.id == endpoint.provider_id => true,
            _ => kimi_provider_profile(Some(&endpoint.provider_id))
                .is_some_and(|(lane_base_url, _, _)| lane_base_url == base_url),
        };
    let base_url = match endpoint.filter(override_targets_active_lane) {
        Some(endpoint) => reroute_base_url(&endpoint.base_url, base_url)?,
        None => base_url.to_owned(),
    };
    env.insert(KIMI_MODEL_API_KEY_ENV.to_owned(), api_key);
    env.insert(KIMI_MODEL_NAME_ENV.to_owned(), model.to_owned());
    env.insert(KIMI_MODEL_BASE_URL_ENV.to_owned(), base_url);
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
    fn pi_process_env_rejects_a_declared_pi_bin() {
        let home = home_with_managed_pi();
        let env = HashMap::from([(PI_ACP_PI_BIN_ENV.to_owned(), "/opt/pi".to_owned())]);

        let error = super::build_agent_process_env(&pi_agent(), home.path(), env)
            .expect_err("declared PI_ACP_PI_BIN must fail");

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

    fn override_for(provider_id: &str) -> crate::secrets::ProviderEndpointOverride {
        crate::secrets::ProviderEndpointOverride {
            provider_id: provider_id.to_owned(),
            base_url: "http://127.0.0.1:3129".to_owned(),
            companion_values: std::collections::BTreeMap::new(),
        }
    }

    #[test]
    fn kimi_process_env_endpoint_override_keeps_the_lane_path_behind_the_origin() {
        let env = HashMap::from([("MOONSHOT_API_KEY".to_owned(), "secret".to_owned())]);
        let agent = kimi_agent_with_provider(KIMI_MOONSHOT_PROVIDER_ID, None);

        let prepared =
            super::build_kimi_process_env(&agent, env, Some(&override_for("moonshotai")))
                .expect("Kimi env with override");

        assert_eq!(
            prepared.get(KIMI_MODEL_BASE_URL_ENV).map(String::as_str),
            Some("http://127.0.0.1:3129/v1")
        );
    }

    #[test]
    fn kimi_process_env_endpoint_override_reroutes_the_provider_less_default_lane() {
        for provider_id in KIMI_SUBSCRIPTION_PROVIDER_IDS {
            let env = HashMap::from([(KIMI_API_KEY_ENV.to_owned(), "secret".to_owned())]);

            let prepared = super::build_kimi_process_env(
                &kimi_agent(None),
                env,
                Some(&override_for(provider_id)),
            )
            .expect("Kimi env with override");

            assert_eq!(
                prepared.get(KIMI_MODEL_BASE_URL_ENV).map(String::as_str),
                Some("http://127.0.0.1:3129/coding/v1"),
                "override under `{provider_id}` must reroute the default lane"
            );
        }
    }

    #[test]
    fn kimi_process_env_provider_less_config_ignores_an_override_for_another_lane() {
        let env = HashMap::from([(KIMI_API_KEY_ENV.to_owned(), "secret".to_owned())]);

        let prepared = super::build_kimi_process_env(
            &kimi_agent(None),
            env,
            Some(&override_for("moonshotai")),
        )
        .expect("Kimi env");

        assert_eq!(
            prepared.get(KIMI_MODEL_BASE_URL_ENV).map(String::as_str),
            Some(KIMI_CODE_BASE_URL)
        );
    }

    #[test]
    fn kimi_process_env_endpoint_override_under_a_lane_alias_reroutes_the_configured_lane() {
        let env = HashMap::from([(KIMI_API_KEY_ENV.to_owned(), "secret".to_owned())]);
        let agent = kimi_agent_with_provider("kimi-code", None);

        let prepared = super::build_kimi_process_env(&agent, env, Some(&override_for("kimi")))
            .expect("Kimi env");

        assert_eq!(
            prepared.get(KIMI_MODEL_BASE_URL_ENV).map(String::as_str),
            Some("http://127.0.0.1:3129/coding/v1")
        );
    }

    #[test]
    fn kimi_process_env_endpoint_override_for_another_provider_is_ignored() {
        let env = HashMap::from([("MOONSHOT_API_KEY".to_owned(), "secret".to_owned())]);
        let agent = kimi_agent_with_provider(KIMI_MOONSHOT_PROVIDER_ID, None);

        let prepared = super::build_kimi_process_env(&agent, env, Some(&override_for("kimi-code")))
            .expect("Kimi env");

        assert_eq!(
            prepared.get(KIMI_MODEL_BASE_URL_ENV).map(String::as_str),
            Some(KIMI_MOONSHOT_BASE_URL)
        );
    }

    #[test]
    fn kimi_process_env_custom_provider_override_keeps_the_declared_path() {
        let env = HashMap::from([("CUSTOM_API_KEY".to_owned(), "secret".to_owned())]);
        let agent = kimi_agent_with_custom_provider(crate::config::CustomProviderApi::default());

        let prepared =
            super::build_kimi_process_env(&agent, env, Some(&override_for("myprovider")))
                .expect("Kimi custom env");

        assert_eq!(
            prepared.get(KIMI_MODEL_BASE_URL_ENV).map(String::as_str),
            Some("http://127.0.0.1:3129/v1")
        );
    }

    #[test]
    fn antigravity_process_env_endpoint_override_is_the_bare_origin() {
        let env = HashMap::from([("GEMINI_API_KEY".to_owned(), "secret".to_owned())]);

        let prepared = build_antigravity_process_env(env, Some(&override_for("google")))
            .expect("Antigravity env");

        assert_eq!(
            prepared.get(ANTIGRAVITY_BASE_URL_ENV).map(String::as_str),
            Some("http://127.0.0.1:3129")
        );
        assert_eq!(
            prepared.get("GEMINI_API_KEY").map(String::as_str),
            Some("secret")
        );
    }

    #[test]
    fn antigravity_process_env_without_an_override_is_unchanged() {
        let env = HashMap::from([("GEMINI_API_KEY".to_owned(), "secret".to_owned())]);

        assert_eq!(
            build_antigravity_process_env(env.clone(), None).expect("Antigravity env"),
            env
        );
    }

    #[test]
    fn antigravity_process_env_rejects_a_declared_base_url() {
        let env = HashMap::from([(ANTIGRAVITY_BASE_URL_ENV.to_owned(), "x".to_owned())]);

        let error = build_antigravity_process_env(env, None).expect_err("managed env must fail");

        assert!(
            error.to_string().contains(ANTIGRAVITY_BASE_URL_ENV),
            "{error}"
        );
    }

    #[test]
    fn antigravity_process_env_refuses_an_unmapped_provider() {
        let error = build_antigravity_process_env(HashMap::new(), Some(&override_for("openai")))
            .expect_err("unmapped provider must fail");

        assert!(error.to_string().contains("openai"), "{error}");
    }

    fn hermes_agent(provider_id: &str, api_key_ref: &str) -> AgentConfig {
        let mut agent = kimi_agent(None);
        agent.id = HERMES_AGENT_ID.to_owned();
        agent.name = "Hermes Agent".to_owned();
        agent.command = "hermes-agent-acp".to_owned();
        agent.args = Vec::new();
        agent.model = None;
        agent.env = vec![api_key_ref.to_owned()];
        agent.provider = Some(crate::config::AgentProviderConfig {
            id: provider_id.to_owned(),
            model: Some("deepseek/deepseek-v4-flash".to_owned()),
            api_key_ref: Some(api_key_ref.to_owned()),
            custom: None,
        });
        agent
    }

    #[test]
    fn hermes_process_env_carries_the_override_in_the_native_base_url_var() {
        let env = HashMap::from([("OPENROUTER_API_KEY".to_owned(), "secret".to_owned())]);
        let agent = hermes_agent("openrouter", "OPENROUTER_API_KEY");

        let prepared = build_hermes_process_env(&agent, env, Some(&override_for("openrouter")))
            .expect("Hermes env");

        assert_eq!(
            prepared.get("OPENROUTER_BASE_URL").map(String::as_str),
            Some("http://127.0.0.1:3129/api/v1")
        );
        assert_eq!(
            prepared.get("OPENROUTER_API_KEY").map(String::as_str),
            Some("secret")
        );
    }

    #[test]
    fn hermes_process_env_without_an_override_exports_no_base_url() {
        let env = HashMap::from([("OPENROUTER_API_KEY".to_owned(), "secret".to_owned())]);
        let agent = hermes_agent("openrouter", "OPENROUTER_API_KEY");

        assert_eq!(
            build_hermes_process_env(&agent, env.clone(), None).expect("Hermes env"),
            env
        );
    }

    #[test]
    fn hermes_process_env_ignores_an_override_for_another_provider() {
        let env = HashMap::from([("OPENROUTER_API_KEY".to_owned(), "secret".to_owned())]);
        let agent = hermes_agent("openrouter", "OPENROUTER_API_KEY");

        let prepared = build_hermes_process_env(&agent, env, Some(&override_for("anthropic")))
            .expect("Hermes env");

        assert!(!prepared.contains_key("OPENROUTER_BASE_URL"));
    }

    #[test]
    fn hermes_process_env_leaves_a_managed_lane_provider_alone() {
        let env = HashMap::from([("ANTHROPIC_API_KEY".to_owned(), "secret".to_owned())]);
        let agent = hermes_agent("anthropic", "ANTHROPIC_API_KEY");

        assert_eq!(
            build_hermes_process_env(&agent, env.clone(), Some(&override_for("anthropic")))
                .expect("Hermes env"),
            env
        );
    }

    #[test]
    fn hermes_process_env_leaves_a_custom_provider_alone() {
        let env = HashMap::from([("CUSTOM_API_KEY".to_owned(), "secret".to_owned())]);
        let mut agent =
            kimi_agent_with_custom_provider(crate::config::CustomProviderApi::default());
        agent.id = HERMES_AGENT_ID.to_owned();

        assert_eq!(
            build_hermes_process_env(&agent, env.clone(), Some(&override_for("myprovider")))
                .expect("Hermes env"),
            env
        );
    }

    #[test]
    fn hermes_process_env_rejects_a_declared_base_url() {
        let env = HashMap::from([("OPENROUTER_BASE_URL".to_owned(), "https://x".to_owned())]);
        let agent = hermes_agent("openrouter", "OPENROUTER_API_KEY");

        let error = build_hermes_process_env(&agent, env, None).expect_err("managed env must fail");

        assert!(error.to_string().contains("OPENROUTER_BASE_URL"), "{error}");
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
