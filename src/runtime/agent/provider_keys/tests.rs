use super::*;
use std::collections::BTreeMap;

#[test]
fn embedded_mapping_loads_and_validates() {
    let mapping = ProviderKeyMapping::from_toml_parts(EMBEDDED_ENV_VARS, EMBEDDED_PROVIDERS)
        .expect("mapping parses");

    assert!(!mapping.api_keys().is_empty());
    assert!(!mapping.providers.is_empty());
    assert!(
        mapping
            .providers
            .iter()
            .all(|provider| !provider.ids().is_empty())
    );
}

#[test]
fn opencode_api_key_allows_both_opencode_provider_ids() {
    assert!(env_ref_allows_provider("OPENCODE_API_KEY", "opencode"));
    assert!(env_ref_allows_provider("OPENCODE_API_KEY", "opencode-go"));
    assert!(!env_ref_allows_provider("OPENCODE_API_KEY", "openai"));
    assert_eq!(
        env_var_for_provider_id("opencode"),
        Some("OPENCODE_API_KEY")
    );
    assert_eq!(
        env_var_for_provider_id("opencode-go"),
        Some("OPENCODE_API_KEY")
    );
}

#[test]
fn agent_specific_api_key_env_vars_are_data_driven() {
    assert_eq!(
        env_var_for_agent_provider_id("opencode", "cloudflare-ai-gateway"),
        Some("CLOUDFLARE_API_TOKEN")
    );
    assert_eq!(
        env_var_for_agent_provider_id("pi", "cloudflare-ai-gateway"),
        Some("CLOUDFLARE_API_KEY")
    );
    assert!(env_ref_allows_provider(
        "CLOUDFLARE_API_TOKEN",
        "cloudflare-ai-gateway"
    ));
}

#[test]
fn provider_ids_collect_from_configured_secret_refs() {
    let providers = provider_ids_for_env_refs([
        "OPENAI_API_KEY",
        "CLOUDFLARE_API_KEY",
        "CLOUDFLARE_API_TOKEN",
        "AI_GATEWAY_API_KEY",
        "UNKNOWN_KEY",
    ]);

    assert_eq!(
        providers.into_iter().collect::<Vec<_>>(),
        [
            "ai-gateway",
            "cloudflare-ai-gateway",
            "cloudflare-workers-ai",
            "openai",
            "vercel",
            "vercel-ai-gateway"
        ]
    );
}

#[test]
fn provider_ids_resolve_to_primary_api_key_env_vars() {
    assert_eq!(env_var_for_provider_id("openai"), Some("OPENAI_API_KEY"));
    assert_eq!(
        env_var_for_provider_id("cloudflare-ai-gateway"),
        Some("CLOUDFLARE_API_KEY")
    );
    assert_eq!(
        env_var_for_provider_id("vercel-ai-gateway"),
        Some("AI_GATEWAY_API_KEY")
    );
    assert_eq!(
        env_var_for_provider_id("vercel"),
        Some("AI_GATEWAY_API_KEY")
    );
    assert_eq!(
        env_var_for_provider_id("fireworks"),
        Some("FIREWORKS_API_KEY")
    );
    assert_eq!(
        env_var_for_provider_id("fireworks-ai"),
        Some("FIREWORKS_API_KEY")
    );
    assert_eq!(env_var_for_provider_id("huggingface"), Some("HF_TOKEN"));
    assert_eq!(env_var_for_provider_id("zai"), Some("ZAI_API_KEY"));
    assert_eq!(env_var_for_provider_id("zhipuai"), Some("ZHIPU_API_KEY"));
    assert_eq!(
        env_var_for_provider_id("moonshotai"),
        Some("MOONSHOT_API_KEY")
    );
    assert_eq!(
        env_var_for_provider_id("minimax-coding-plan"),
        Some("MINIMAX_API_KEY")
    );
    assert_eq!(
        env_var_for_provider_id("microsoft-foundry"),
        Some("ANTHROPIC_FOUNDRY_API_KEY")
    );
}

#[test]
fn cloudflare_provider_refs_include_documented_companions() {
    assert_eq!(
        required_env_refs_for_provider_id("cloudflare-workers-ai", "CLOUDFLARE_API_KEY"),
        ["CLOUDFLARE_API_KEY", "CLOUDFLARE_ACCOUNT_ID"]
    );
    assert_eq!(
        required_env_refs_for_provider_id("cloudflare-ai-gateway", "CLOUDFLARE_API_KEY"),
        [
            "CLOUDFLARE_API_KEY",
            "CLOUDFLARE_ACCOUNT_ID",
            "CLOUDFLARE_GATEWAY_ID"
        ]
    );
    assert_eq!(
        required_env_refs_for_provider_id("cloudflare-ai-gateway", "CLOUDFLARE_API_TOKEN"),
        [
            "CLOUDFLARE_API_TOKEN",
            "CLOUDFLARE_ACCOUNT_ID",
            "CLOUDFLARE_GATEWAY_ID"
        ]
    );
    assert!(optional_env_refs_for_provider_id("cloudflare-workers-ai").is_empty());
    assert!(optional_env_refs_for_provider_id("cloudflare-ai-gateway").is_empty());
}

#[test]
fn provider_metadata_includes_models_dev_display_names() {
    let mapping = ProviderKeyMapping::load_embedded();

    assert_eq!(
        mapping
            .provider_mapping("opencode-go")
            .map(|provider| provider.name.as_str()),
        Some("OpenCode Go")
    );
    assert_eq!(
        mapping
            .provider_mapping("cloudflare-ai-gateway")
            .map(|provider| provider.name.as_str()),
        Some("Cloudflare AI Gateway")
    );
}

#[test]
fn provider_lookup_works_for_every_collapsed_provider_id() {
    let mapping = ProviderKeyMapping::load_embedded();

    for provider_id in [
        "vercel-ai-gateway",
        "vercel",
        "fireworks",
        "fireworks-ai",
        "together",
        "togetherai",
        "kimi-coding",
        "kimi-for-coding",
    ] {
        let provider = mapping
            .provider_mapping(provider_id)
            .expect("collapsed provider id resolves");
        assert!(provider.ids().iter().any(|id| id == provider_id));
    }
}

#[test]
fn models_dev_only_providers_are_opencode_scoped_without_default_env_refs() {
    for provider_id in ["helicone", "deepinfra", "github-models", "venice"] {
        assert!(provider_id_is_known(provider_id));
        assert!(provider_id_supports_agent(provider_id, "opencode"));
        assert!(!provider_id_supports_agent(provider_id, "pi"));
        assert_eq!(env_var_for_provider_id(provider_id), None);
        assert_eq!(env_var_for_agent_provider_id("opencode", provider_id), None);
    }
}

#[test]
fn azure_provider_refs_include_base_url_and_documented_options() {
    assert_eq!(
        required_env_refs_for_provider_id("azure-openai-responses", "AZURE_OPENAI_API_KEY"),
        ["AZURE_OPENAI_API_KEY", "AZURE_OPENAI_BASE_URL"]
    );
    assert_eq!(
        optional_env_refs_for_provider_id("azure-openai-responses"),
        [
            "AZURE_OPENAI_RESOURCE_NAME",
            "AZURE_OPENAI_API_VERSION",
            "AZURE_OPENAI_DEPLOYMENT_NAME_MAP"
        ]
    );
}

#[test]
fn provider_metadata_scopes_supported_agents() {
    assert!(provider_id_supports_agent("fireworks", "pi"));
    assert!(provider_id_supports_agent("fireworks", "opencode"));
    assert!(provider_id_supports_agent("fireworks-ai", "opencode"));
    assert!(provider_id_supports_agent("fireworks-ai", "pi"));
    assert!(provider_id_supports_agent("openai", "pi"));
    assert!(provider_id_supports_agent("openai", "opencode"));
    assert!(provider_id_supports_agent("openai", "codex"));
    assert!(provider_id_supports_agent("openrouter", "codex"));
    assert!(!provider_id_supports_agent("anthropic", "codex"));
    assert!(provider_id_supports_agent("anthropic", "claude"));
    assert!(provider_id_supports_agent("amazon-bedrock", "claude"));
    assert!(provider_id_supports_agent(
        "google-vertex-anthropic",
        "claude"
    ));
    assert!(provider_id_supports_agent("microsoft-foundry", "claude"));
    assert!(provider_id_supports_agent("moonshotai", "claude"));
    assert!(provider_id_supports_agent(
        "xiaomi-token-plan-sgp",
        "claude"
    ));
    assert!(!provider_id_supports_agent("xai", "codex"));
    assert!(!provider_id_supports_agent("openai", "amp"));
    assert!(provider_id_supports_agent("anthropic", "goose"));
    assert!(provider_id_supports_agent("openai", "goose"));
    assert!(provider_id_supports_agent("mistral", "goose"));
    assert!(provider_id_supports_agent("groq", "goose"));
    assert!(provider_id_supports_agent("openrouter", "goose"));
    assert!(provider_id_supports_agent("cerebras", "goose"));
    assert!(provider_id_supports_agent("xai", "goose"));
    assert!(!provider_id_supports_agent("deepseek", "goose"));
    assert_eq!(env_var_for_agent_provider_id("amp", "openai"), None);
    assert_eq!(
        env_var_for_agent_provider_id("goose", "openrouter"),
        Some("OPENROUTER_API_KEY")
    );
    assert_eq!(
        env_var_for_agent_provider_id("codex", "openrouter"),
        Some("OPENROUTER_API_KEY")
    );
    assert_eq!(
        env_var_for_agent_provider_id("claude", "moonshotai"),
        Some("MOONSHOT_API_KEY")
    );
    assert!(provider_id_supports_agent("moonshotai-cn", "claude"));
    assert_eq!(
        env_var_for_agent_provider_id("claude", "moonshotai-cn"),
        Some("MOONSHOT_API_KEY")
    );
    assert!(provider_id_supports_agent("kimi-coding", "claude"));
    assert!(provider_id_supports_agent("kimi-for-coding", "claude"));
    assert_eq!(
        agent_provider_id_for_provider_id("claude", "kimi-coding"),
        Some("kimi-coding-plan")
    );
    assert_eq!(
        env_var_for_agent_provider_id("claude", "kimi-coding"),
        Some("KIMI_API_KEY")
    );
    assert_eq!(
        env_var_for_agent_provider_id("claude", "kimi-for-coding"),
        Some("KIMI_API_KEY")
    );
    assert_eq!(
        env_var_for_agent_provider_id("opencode", "kimi-for-coding"),
        Some("KIMI_API_KEY")
    );
    assert_eq!(
        env_var_for_agent_provider_id("claude", "amazon-bedrock"),
        None
    );
}

#[test]
fn agent_native_provider_ids_are_data_driven() {
    assert_eq!(
        agent_provider_id_for_provider_id("pi", "vercel"),
        Some("vercel-ai-gateway")
    );
    assert_eq!(
        agent_provider_id_for_provider_id("opencode", "vercel-ai-gateway"),
        Some("vercel")
    );
    assert_eq!(
        agent_provider_id_for_provider_id("pi", "fireworks-ai"),
        Some("fireworks")
    );
    assert_eq!(
        agent_provider_id_for_provider_id("opencode", "fireworks"),
        Some("fireworks-ai")
    );
    assert_eq!(
        agent_provider_id_for_provider_id("pi", "togetherai"),
        Some("together")
    );
    assert_eq!(
        agent_provider_id_for_provider_id("opencode", "together"),
        Some("togetherai")
    );
    assert_eq!(
        agent_provider_id_for_provider_id("pi", "kimi-for-coding"),
        Some("kimi-coding")
    );
    assert_eq!(
        agent_provider_id_for_provider_id("opencode", "kimi-coding"),
        Some("kimi-for-coding")
    );
    assert_eq!(agent_provider_id_for_provider_id("amp", "openai"), None);
}

#[test]
fn pi_native_config_provider_ids_resolve_to_canonical() {
    assert_eq!(
        canonical_provider_id_for_agent_native_id("pi", "anthropic"),
        Some("anthropic")
    );
    assert_eq!(
        canonical_provider_id_for_agent_native_id("pi", "openai"),
        Some("openai")
    );
    assert_eq!(
        canonical_provider_id_for_agent_native_id("pi", "vercel-ai-gateway"),
        Some("vercel-ai-gateway")
    );
    assert_eq!(
        canonical_provider_id_for_agent_native_id("pi", "fireworks"),
        Some("fireworks")
    );
    assert_eq!(
        canonical_provider_id_for_agent_native_id("pi", "totally-unknown-provider"),
        None
    );
}

#[test]
fn direct_agent_env_refs_are_data_driven() {
    assert_eq!(env_refs_for_agent_id("amp"), ["AMP_API_KEY"]);
    assert_eq!(env_refs_for_agent_id("kilo"), ["KILO_API_KEY"]);
    assert_eq!(env_refs_for_agent_id("kimi"), ["KIMI_API_KEY"]);
    assert_eq!(env_refs_for_agent_id("antigravity"), ["GEMINI_API_KEY"]);
    assert!(env_refs_for_agent_id("opencode").is_empty());
    // Hermes is provider-backed, not a direct-secret agent.
    assert!(env_refs_for_agent_id("hermes").is_empty());
}

#[test]
fn hermes_maps_api_key_providers_only() {
    assert!(provider_id_supports_agent("openrouter", "hermes"));
    assert!(provider_id_supports_agent("openai", "hermes"));
    assert_eq!(
        env_var_for_agent_provider_id("hermes", "openrouter"),
        Some("OPENROUTER_API_KEY")
    );
    assert_eq!(
        agent_provider_id_for_provider_id("hermes", "openrouter"),
        Some("openrouter")
    );
    // OAuth-only Hermes providers are deliberately unmapped.
    assert!(!provider_id_supports_agent("github-copilot", "hermes"));
}

#[test]
fn cloud_provider_refs_include_documented_non_key_fields() {
    assert_eq!(
        companion_env_refs_for_provider_id("google-vertex"),
        ["GOOGLE_CLOUD_PROJECT", "GOOGLE_CLOUD_LOCATION"]
    );
    assert_eq!(
        optional_env_refs_for_provider_id("google-vertex"),
        ["GOOGLE_APPLICATION_CREDENTIALS"]
    );
    let bedrock = optional_env_refs_for_provider_id("amazon-bedrock");
    assert!(bedrock.contains(&"AWS_PROFILE"));
    assert!(bedrock.contains(&"AWS_ACCESS_KEY_ID"));
    assert!(bedrock.contains(&"AWS_CONTAINER_CREDENTIALS_RELATIVE_URI"));
    assert!(bedrock.contains(&"AWS_WEB_IDENTITY_TOKEN_FILE"));
}

#[test]
fn claude_code_provider_refs_use_agent_specific_profiles() {
    assert_eq!(
        required_env_refs_for_agent_provider_id("claude", "google-vertex-anthropic", None),
        ["ANTHROPIC_VERTEX_PROJECT_ID", "CLOUD_ML_REGION"]
    );
    assert_eq!(
        required_env_refs_for_agent_provider_id(
            "claude",
            "microsoft-foundry",
            Some("ANTHROPIC_FOUNDRY_API_KEY")
        ),
        ["ANTHROPIC_FOUNDRY_API_KEY", "ANTHROPIC_FOUNDRY_BASE_URL"]
    );
    assert!(provider_uses_agent_native_auth("claude", "amazon-bedrock"));
    // The explicit optional list stops Pi-only auth overrides leaking in via
    // provider-level fallback.
    let bedrock_optional = optional_env_refs_for_agent_provider_id("claude", "amazon-bedrock");
    assert!(bedrock_optional.contains(&"AWS_PROFILE"));
    assert!(!bedrock_optional.contains(&"AWS_BEDROCK_SKIP_AUTH"));
    assert!(!bedrock_optional.contains(&"AWS_BEDROCK_FORCE_HTTP1"));
    assert!(!bedrock_optional.contains(&"AWS_BEDROCK_FORCE_CACHE"));
    assert!(provider_uses_agent_native_auth(
        "claude",
        "google-vertex-anthropic"
    ));
    assert!(!provider_uses_agent_native_auth(
        "claude",
        "microsoft-foundry"
    ));
    // Codex's built-in openai lane is key-driven, yet still refused an endpoint
    // override — a separate capability.
    assert!(!provider_uses_agent_native_auth("codex", "openai"));
    assert_eq!(
        env_var_for_agent_provider_id("codex", "openai"),
        Some("OPENAI_API_KEY")
    );
    assert!(!agent_provider_accepts_endpoint_override("codex", "openai"));
    assert!(agent_provider_accepts_endpoint_override(
        "codex",
        "openrouter"
    ));

    let summaries = providers_for_agent("claude");
    let bedrock = summaries
        .iter()
        .find(|summary| summary.id == "amazon-bedrock")
        .expect("Bedrock should be listed for Claude Code");
    assert_eq!(bedrock.default_api_key_ref, None);
    assert!(bedrock.optional_env_refs.contains(&"AWS_PROFILE"));
    let foundry = summaries
        .iter()
        .find(|summary| summary.id == "microsoft-foundry")
        .expect("Foundry should be listed for Claude Code");
    assert_eq!(
        foundry.default_api_key_ref,
        Some("ANTHROPIC_FOUNDRY_API_KEY")
    );
    assert_eq!(foundry.companion_env_refs, ["ANTHROPIC_FOUNDRY_BASE_URL"]);
}

#[test]
fn invalid_mapping_rejects_duplicate_provider_ids() {
    let err = ProviderKeyMapping::from_toml(
        r#"
[[api_keys]]
env_var = "FIRST_API_KEY"
provider_ids = ["same"]

[[api_keys]]
env_var = "SECOND_API_KEY"
provider_ids = ["same"]

[[providers]]
id = ["same"]
name = "Same"
agents = ["pi"]
"#,
    )
    .expect_err("duplicate provider id fails");

    assert!(err.to_string().contains("duplicate provider id `same`"));
}

#[test]
fn invalid_mapping_rejects_plaintext_models_url() {
    let err = ProviderKeyMapping::from_toml(
        r#"
[[api_keys]]
env_var = "PLAIN_API_KEY"
provider_ids = ["plain"]

[[providers]]
id = ["plain"]
name = "Plain"
agents = ["pi"]
models_url = "http://example.com/v1/models"
"#,
    )
    .expect_err("plaintext models_url fails");

    assert!(
        err.to_string()
            .contains("provider `plain` models_url must be an HTTPS URL")
    );
}

#[test]
fn invalid_mapping_rejects_duplicate_provider_metadata_ids() {
    let err = ProviderKeyMapping::from_toml(
        r#"
[[api_keys]]
env_var = "FIRST_API_KEY"
provider_ids = ["same"]

[[providers]]
id = ["same", "alias"]
name = "Same"
agents = ["pi"]

[[providers]]
id = ["alias"]
name = "Alias"
agents = ["opencode"]
"#,
    )
    .expect_err("duplicate provider metadata id fails");

    assert!(
        err.to_string()
            .contains("duplicate provider env mapping `alias`")
    );
}

#[test]
fn invalid_mapping_rejects_native_provider_id_for_unknown_id() {
    let err = ProviderKeyMapping::from_toml(
        r#"
[[api_keys]]
env_var = "FIRST_API_KEY"
provider_ids = ["known"]

[[providers]]
id = ["known"]
name = "Known"
agents = ["pi"]

[providers.provider_ids]
missing = "pi"
"#,
    )
    .expect_err("unknown native provider id fails");

    assert!(
        err.to_string()
            .contains("maps unknown native provider id `missing`")
    );
}

#[test]
fn invalid_mapping_rejects_duplicate_native_agent_mapping() {
    let err = ProviderKeyMapping::from_toml(
        r#"
[[api_keys]]
env_var = "FIRST_API_KEY"
provider_ids = ["known"]

[[providers]]
id = ["known", "alias"]
name = "Known"
agents = ["pi"]

[providers.provider_ids]
known = "pi"
alias = "pi"
"#,
    )
    .expect_err("duplicate native agent mapping fails");

    assert!(
        err.to_string()
            .contains("multiple native provider ids for agent `pi`")
    );
}

#[test]
fn invalid_mapping_rejects_empty_values() {
    let err = ProviderKeyMapping::from_toml(
        r#"
[[api_keys]]
env_var = ""
provider_ids = ["openai"]

[[providers]]
id = ["openai"]
name = "OpenAI"
agents = ["pi"]
"#,
    )
    .expect_err("empty env var fails");

    assert!(err.to_string().contains("must not be empty"));
}

fn invalid_param_reason(error: StackError) -> String {
    match error {
        StackError::InvalidParam { reason, .. } => reason,
        other => panic!("expected InvalidParam, got {other:?}"),
    }
}

#[test]
fn env_keyed_values_accept_canonical_single_key() {
    let values = BTreeMap::from([("OPENAI_API_KEY".to_owned(), "sk-value".to_owned())]);
    validate_env_keyed_credential_values("openai", &values, "test.values")
        .expect("canonical single-key credential is valid");
}

#[test]
fn env_keyed_values_reject_unknown_provider() {
    let values = BTreeMap::from([("SOME_KEY".to_owned(), "value".to_owned())]);
    let error = validate_env_keyed_credential_values("no-such-provider", &values, "test.values")
        .expect_err("unknown provider must be rejected");
    assert!(invalid_param_reason(error).contains("no canonical API-key env var"));
}

#[test]
fn env_keyed_values_reject_missing_required_companion() {
    let primary = env_var_for_provider_id("cloudflare-ai-gateway")
        .expect("cloudflare-ai-gateway has a canonical env var");
    let companions = companion_env_refs_for_provider_id("cloudflare-ai-gateway");
    assert!(
        !companions.is_empty(),
        "fixture provider must require companions"
    );
    let values = BTreeMap::from([(primary.to_owned(), "cf-key".to_owned())]);
    let error =
        validate_env_keyed_credential_values("cloudflare-ai-gateway", &values, "test.values")
            .expect_err("missing companion must be rejected");
    assert!(invalid_param_reason(error).contains(companions[0]));
}

#[test]
fn env_keyed_values_accept_full_companion_set() {
    let primary = env_var_for_provider_id("cloudflare-ai-gateway")
        .expect("cloudflare-ai-gateway has a canonical env var");
    let mut values = BTreeMap::from([(primary.to_owned(), "cf-key".to_owned())]);
    for companion in companion_env_refs_for_provider_id("cloudflare-ai-gateway") {
        values.insert(companion.to_owned(), "companion-value".to_owned());
    }
    validate_env_keyed_credential_values("cloudflare-ai-gateway", &values, "test.values")
        .expect("full companion set is valid");
}

#[test]
fn env_keyed_values_reject_key_outside_contract() {
    let values = BTreeMap::from([
        ("OPENAI_API_KEY".to_owned(), "sk-value".to_owned()),
        ("UNRELATED_ENV".to_owned(), "value".to_owned()),
    ]);
    let error = validate_env_keyed_credential_values("openai", &values, "test.values")
        .expect_err("key outside the provider contract must be rejected");
    assert!(invalid_param_reason(error).contains("UNRELATED_ENV"));
}

#[test]
fn env_keyed_values_allow_optional_env_vars() {
    let optional = optional_env_refs_for_provider_id("azure-openai-responses");
    assert!(
        !optional.is_empty(),
        "fixture provider must have optional env vars"
    );
    let primary = env_var_for_provider_id("azure-openai-responses")
        .expect("azure-openai-responses has a canonical env var");
    let mut values = BTreeMap::from([(primary.to_owned(), "az-key".to_owned())]);
    for companion in companion_env_refs_for_provider_id("azure-openai-responses") {
        values.insert(companion.to_owned(), "companion-value".to_owned());
    }
    values.insert(optional[0].to_owned(), "optional-value".to_owned());
    validate_env_keyed_credential_values("azure-openai-responses", &values, "test.values")
        .expect("optional env vars are allowed");
}

#[test]
fn env_keyed_values_reject_empty_value() {
    let values = BTreeMap::from([("OPENAI_API_KEY".to_owned(), String::new())]);
    let error = validate_env_keyed_credential_values("openai", &values, "test.values")
        .expect_err("empty value must be rejected");
    assert!(invalid_param_reason(error).contains("must not be empty"));
}

#[test]
fn embedded_claude_code_profiles_parse_from_provider_metadata() {
    assert!(is_claude_code_profiled_provider("deepseek"));
    assert!(is_claude_code_profiled_provider("xiaomi-token-plan-sgp"));
    assert!(is_claude_code_profiled_provider("anthropic"));
    assert!(!is_claude_code_profiled_provider("openai"));

    let anthropic = claude_code_profile_for_provider_id("anthropic").expect("anthropic profile");
    assert!(!anthropic.agent_native_auth);
    assert!(anthropic.base_url.is_none());
    assert!(anthropic.env.is_empty());
}

#[test]
fn claude_code_native_auth_profiles_resolve_no_api_key() {
    for provider_id in ["amazon-bedrock", "google-vertex-anthropic"] {
        let profile = claude_code_profile_for_provider_id(provider_id)
            .unwrap_or_else(|| panic!("{provider_id} profile should exist"));
        assert!(profile.agent_native_auth);
        assert_eq!(env_var_for_agent_provider_id("claude", provider_id), None);
    }
}

#[test]
fn claude_code_profiles_declare_role_model_defaults() {
    let cases = [
        (
            "deepseek",
            "deepseek-v4-pro[1m]",
            "deepseek-v4-pro[1m]",
            "deepseek-v4-pro[1m]",
            "deepseek-v4-flash",
        ),
        (
            "zai",
            "glm-5.3[1m]",
            "glm-5.3[1m]",
            "glm-5.3[1m]",
            "glm-4.7",
        ),
        (
            "zhipuai",
            "glm-5.3[1m]",
            "glm-5.3[1m]",
            "glm-5.3[1m]",
            "glm-4.7",
        ),
    ];

    for (provider_id, default_model, opus_model, sonnet_model, haiku_model) in cases {
        let profile = claude_code_profile_for_provider_id(provider_id)
            .unwrap_or_else(|| panic!("{provider_id} profile"));

        assert_eq!(profile.default_model.as_deref(), Some(default_model));
        assert_eq!(profile.default_opus_model.as_deref(), Some(opus_model));
        assert_eq!(profile.default_sonnet_model.as_deref(), Some(sonnet_model));
        assert_eq!(profile.default_haiku_model.as_deref(), Some(haiku_model));
    }
}

#[test]
fn invalid_mapping_rejects_claude_code_profile_without_agent_support() {
    let err = ProviderKeyMapping::from_toml(
        r#"
[[providers]]
id = ["solo"]
name = "Solo"
agents = ["pi"]

[providers.claude_code]
default_model = "some-model"
"#,
    )
    .expect_err("claude_code profile without claude support fails");

    assert!(err.to_string().contains("does not support `claude`"));
}

#[test]
fn invalid_mapping_rejects_claude_code_role_models_without_default_model() {
    let err = ProviderKeyMapping::from_toml(
        r#"
[[providers]]
id = ["solo"]
name = "Solo"
agents = ["claude"]

[providers.claude_code]
default_opus_model = "opus-model"
"#,
    )
    .expect_err("role model defaults without default_model fails");

    assert!(err.to_string().contains("without default_model"));
}

#[test]
fn invalid_mapping_rejects_claude_code_native_auth_with_api_key_env_var() {
    let err = ProviderKeyMapping::from_toml(
        r#"
[[providers]]
id = ["solo"]
name = "Solo"
agents = ["claude"]

[providers.api_key_env_vars]
claude = "SOLO_API_KEY"

[providers.claude_code]
agent_native_auth = true
"#,
    )
    .expect_err("native auth with claude api key env var fails");

    assert!(err.to_string().contains("native auth"));
}

#[test]
fn invalid_mapping_rejects_claude_code_native_auth_with_api_key_mapping() {
    let err = ProviderKeyMapping::from_toml(
        r#"
[[api_keys]]
env_var = "SOLO_API_KEY"
provider_ids = ["solo"]

[[providers]]
id = ["solo"]
name = "Solo"
agents = ["claude"]

[providers.claude_code]
agent_native_auth = true
"#,
    )
    .expect_err("native auth with api key mapping fails");

    assert!(err.to_string().contains("native auth"));
}

#[test]
fn hermes_api_modes_are_data_driven() {
    for provider_id in ["anthropic", "kimi", "kimi-coding", "minimax", "minimax-cn"] {
        assert_eq!(
            hermes_api_mode_for_provider_id(provider_id),
            Some("anthropic_messages"),
            "{provider_id}"
        );
    }
    for provider_id in ["openai", "xai", "meta", "meta-ai", "actual"] {
        assert_eq!(
            hermes_api_mode_for_provider_id(provider_id),
            Some("codex_responses"),
            "{provider_id}"
        );
    }
    for provider_id in ["openrouter", "google", "zai", "opencode", "opencode-go"] {
        assert_eq!(
            hermes_api_mode_for_provider_id(provider_id),
            Some("chat_completions"),
            "{provider_id}"
        );
    }
    assert_eq!(hermes_api_mode_for_provider_id("not-a-provider"), None);
}

/// The native base-URL env lane replaces the managed named entry, so every listed pair must be a
/// hermes provider acps can actually route: reachable through the mapping under the same id
/// hermes' overlay uses, and carrying a vendor base to reroute.
#[test]
fn hermes_base_url_env_lane_covers_only_routable_hermes_providers() {
    for (native_provider_id, base_url_env) in HERMES_PROVIDER_BASE_URL_ENV {
        let provider_id =
            canonical_provider_id_for_agent_native_id(HERMES_AGENT_ID, native_provider_id)
                .unwrap_or_else(|| panic!("`{native_provider_id}` is not a hermes provider"));
        assert!(
            agent_provider_accepts_endpoint_override(HERMES_AGENT_ID, provider_id),
            "`{provider_id}` carries `{base_url_env}` but is refused an endpoint override"
        );
        assert!(
            vendor_base_url_for_agent_provider_id(HERMES_AGENT_ID, provider_id).is_some(),
            "`{provider_id}` has no vendor base URL to reroute"
        );
        assert_eq!(
            agent_provider_id_for_provider_id(HERMES_AGENT_ID, provider_id),
            Some(native_provider_id),
            "acps must write `{native_provider_id}` as the native id for `{provider_id}`"
        );
    }
    // Hermes' anthropic overlay declares no base-URL variable, so it stays on the managed entry.
    assert_eq!(
        hermes_base_url_env_for_native_provider_id("anthropic"),
        None
    );
    assert_eq!(
        hermes_base_url_env_for_native_provider_id("not-a-provider"),
        None
    );
}

#[test]
fn endpoint_override_pairs_are_data_driven() {
    assert!(agent_provider_accepts_endpoint_override(
        "hermes",
        "openrouter"
    ));
    // Unknown ids are configured custom providers.
    assert!(agent_provider_accepts_endpoint_override(
        "hermes",
        "myprovider"
    ));
    assert!(!agent_provider_accepts_endpoint_override("codex", "openai"));
    assert!(agent_provider_accepts_endpoint_override(
        "codex",
        "openrouter"
    ));
    // Goose needs a host setting for the native provider.
    assert!(agent_provider_accepts_endpoint_override(
        "goose",
        "openrouter"
    ));
    assert!(!agent_provider_accepts_endpoint_override(
        "goose", "cerebras"
    ));
    // Kimi lanes are data-driven from the provider rows; a provider without a row has no lane.
    assert!(agent_provider_accepts_endpoint_override(
        "kimi",
        "moonshotai"
    ));
    assert!(agent_provider_accepts_endpoint_override(
        "kimi",
        "kimi-code"
    ));
    // Keyed Claude Code lanes reroute; native-auth lanes ignore ANTHROPIC_BASE_URL.
    assert!(agent_provider_accepts_endpoint_override(
        "claude",
        "moonshotai"
    ));
    assert!(agent_provider_accepts_endpoint_override(
        "claude",
        "anthropic"
    ));
    // A mapped provider the agent does not run has no native slot to write into.
    assert!(!agent_provider_accepts_endpoint_override("kimi", "mistral"));
    assert!(agent_provider_accepts_endpoint_override(
        "kimi",
        "openrouter"
    ));
    assert!(!agent_provider_accepts_endpoint_override(
        "antigravity",
        "openai"
    ));
    assert!(agent_provider_accepts_endpoint_override(
        "antigravity",
        "google"
    ));
    assert!(agent_provider_accepts_endpoint_override("kilo", "kilo"));
    assert!(agent_provider_accepts_endpoint_override(
        "kilo",
        "openrouter"
    ));
    // A mapped provider without a vendor base cannot be composed.
    assert!(!agent_provider_accepts_endpoint_override(
        "opencode",
        "deepinfra"
    ));
    assert!(agent_provider_accepts_endpoint_override(
        "opencode",
        "cloudflare-ai-gateway"
    ));
    assert!(agent_provider_accepts_endpoint_override(
        "opencode", "jiekou"
    ));
    assert!(agent_provider_accepts_endpoint_override(
        "hermes", "kilocode"
    ));
    // An unmapped id is a configured custom provider carrying its own base, except for the
    // agents that have no custom-provider path at all.
    assert!(agent_provider_accepts_endpoint_override(
        "opencode",
        "myprovider"
    ));
    assert!(!agent_provider_accepts_endpoint_override(
        "kilo",
        "myprovider"
    ));
    assert!(!agent_provider_accepts_endpoint_override(
        "antigravity",
        "myprovider"
    ));
    assert!(agent_provider_accepts_endpoint_override(
        "opencode",
        "opencode-go"
    ));
}

#[test]
fn vendor_base_urls_prefer_the_agent_entry() {
    assert_eq!(
        vendor_base_url_for_agent_provider_id("opencode", "anthropic"),
        Some("https://api.anthropic.com/v1")
    );
    assert_eq!(
        vendor_base_url_for_agent_provider_id("pi", "anthropic"),
        Some("https://api.anthropic.com")
    );
    assert_eq!(
        vendor_base_url_for_agent_provider_id("antigravity", "google"),
        Some("https://generativelanguage.googleapis.com")
    );
    // The hermes managed entry pins the chat_completions transport, so its
    // Google base is the OpenAI-compatible surface.
    assert_eq!(
        vendor_base_url_for_agent_provider_id("hermes", "google"),
        Some("https://generativelanguage.googleapis.com/v1beta/openai")
    );
    assert_eq!(
        vendor_base_url_for_agent_provider_id("pi", "google"),
        Some("https://generativelanguage.googleapis.com/v1beta")
    );
    // pi drives the Vercel gateway over its Anthropic-compatible surface.
    assert_eq!(
        vendor_base_url_for_agent_provider_id("pi", "vercel-ai-gateway"),
        Some("https://ai-gateway.vercel.sh")
    );
    assert_eq!(
        vendor_base_url_for_agent_provider_id("hermes", "vercel-ai-gateway"),
        Some("https://ai-gateway.vercel.sh/v1")
    );
    assert_eq!(
        vendor_base_url_for_agent_provider_id("hermes", "commandcode"),
        Some("https://api.commandcode.ai/provider/v1")
    );
    assert_eq!(
        vendor_base_url_for_agent_provider_id("opencode", "jiekou"),
        Some("https://api.jiekou.ai/openai")
    );
    assert_eq!(
        vendor_base_url_for_agent_provider_id("kimi", "mistral"),
        None
    );
    assert_eq!(
        vendor_base_url_for_agent_provider_id("kimi", "minimax"),
        Some("https://api.minimax.io/anthropic")
    );
    // Per-account segments stay as placeholders until the stored companions fill them.
    assert_eq!(
        vendor_base_url_for_agent_provider_id("opencode", "cloudflare-ai-gateway"),
        Some(
            "https://gateway.ai.cloudflare.com/v1/{CLOUDFLARE_ACCOUNT_ID}/{CLOUDFLARE_GATEWAY_ID}/compat"
        )
    );
    assert_eq!(
        vendor_base_url_for_agent_provider_id("opencode", "deepinfra"),
        None
    );
}

#[test]
fn templated_vendor_base_urls_resolve_from_stored_companions() {
    assert_eq!(
        base_url_template_placeholders(
            "https://gateway.ai.cloudflare.com/v1/{CLOUDFLARE_ACCOUNT_ID}/{CLOUDFLARE_GATEWAY_ID}/compat"
        ),
        ["CLOUDFLARE_ACCOUNT_ID", "CLOUDFLARE_GATEWAY_ID"]
    );
    assert!(base_url_template_placeholders("https://api.openai.com/v1").is_empty());

    let values = BTreeMap::from([
        ("CLOUDFLARE_ACCOUNT_ID".to_owned(), "acct".to_owned()),
        ("CLOUDFLARE_GATEWAY_ID".to_owned(), "gw".to_owned()),
    ]);
    assert_eq!(
        resolve_base_url_template(
            "https://gateway.ai.cloudflare.com/v1/{CLOUDFLARE_ACCOUNT_ID}/{CLOUDFLARE_GATEWAY_ID}/compat",
            &values
        )
        .expect("resolves"),
        "https://gateway.ai.cloudflare.com/v1/acct/gw/compat"
    );
    assert_eq!(
        resolve_base_url_template("https://api.openai.com/v1", &BTreeMap::new()).expect("literal"),
        "https://api.openai.com/v1"
    );
    let error = resolve_base_url_template(
        "https://{DATABRICKS_HOST}/ai-gateway/mlflow/v1",
        &BTreeMap::new(),
    )
    .expect_err("missing companion");
    assert!(error.to_string().contains("DATABRICKS_HOST"), "{error}");
}

#[test]
fn templated_vendor_base_urls_may_only_name_contract_companions() {
    let env_vars = r#"
[[api_keys]]
env_var = "EXAMPLE_API_KEY"
provider_ids = ["example"]
companion_env_vars = ["EXAMPLE_ACCOUNT_ID"]
optional_env_vars = []
"#;
    let accepted = r#"
[[providers]]
id = ["example"]
name = "Example"
agents = ["opencode"]
base_url = "https://api.example.com/{EXAMPLE_ACCOUNT_ID}/v1"
"#;
    ProviderKeyMapping::from_toml_parts(env_vars, accepted).expect("companion placeholder loads");

    let rejected = r#"
[[providers]]
id = ["example"]
name = "Example"
agents = ["opencode"]
base_url = "https://api.example.com/{EXAMPLE_REGION}/v1"
"#;
    let error = ProviderKeyMapping::from_toml_parts(env_vars, rejected)
        .expect_err("unknown placeholder is rejected");
    assert!(error.to_string().contains("EXAMPLE_REGION"), "{error}");

    let unbalanced = r#"
[[providers]]
id = ["example"]
name = "Example"
agents = ["opencode"]
base_url = "https://api.example.com/{EXAMPLE_ACCOUNT_ID/v1"
"#;
    let error = ProviderKeyMapping::from_toml_parts(env_vars, unbalanced)
        .expect_err("unbalanced brace is rejected");
    assert!(error.to_string().contains("unbalanced"), "{error}");
}

#[test]
fn kimi_profiles_are_data_driven() {
    for (provider_id, provider_type, default_model) in [
        ("kimi-code", "kimi", Some("kimi-for-coding")),
        ("kimi-coding-global", "kimi", Some("kimi-for-coding")),
        ("moonshotai", "kimi", Some("kimi-k3")),
        ("moonshotai-cn", "kimi", Some("kimi-k3")),
        ("openrouter", "openai", None),
        ("openai", "openai_responses", None),
        ("anthropic", "anthropic", None),
        ("minimax", "anthropic", None),
    ] {
        let profile = kimi_profile_for_provider_id(provider_id)
            .unwrap_or_else(|| panic!("`{provider_id}` has a kimi profile"));
        assert_eq!(profile.provider_type, provider_type, "{provider_id}");
        assert_eq!(
            profile.default_model.as_deref(),
            default_model,
            "{provider_id}"
        );
        assert!(
            agent_provider_accepts_endpoint_override(KIMI_AGENT_ID, provider_id),
            "{provider_id}"
        );
    }
    assert_eq!(kimi_profile_for_provider_id("mistral"), None);
    assert!(!agent_provider_accepts_endpoint_override(
        KIMI_AGENT_ID,
        "mistral"
    ));
}

/// The Anthropic wire takes the origin, so those rows carry a kimi base without `/v1`.
#[test]
fn kimi_anthropic_wire_rows_declare_an_origin_base() {
    for provider in ProviderKeyMapping::load_embedded().providers() {
        let Some(profile) = provider.kimi.as_ref() else {
            continue;
        };
        let base = provider
            .vendor_base_url(KIMI_AGENT_ID)
            .unwrap_or_else(|| panic!("`{}` has a kimi base", provider.primary_id()));
        if profile.provider_type == "anthropic" {
            assert!(
                !base.trim_end_matches('/').ends_with("/v1"),
                "`{}` anthropic-wire base `{base}` must omit /v1",
                provider.primary_id()
            );
        }
    }
}

#[test]
fn invalid_mapping_rejects_kimi_provider_without_profile() {
    let err = ProviderKeyMapping::from_toml(
        r#"
[[providers]]
id = ["solo"]
name = "Solo"
agents = ["kimi"]
base_url = "https://api.solo.example/v1"
"#,
    )
    .expect_err("kimi-enabled provider without profile fails");

    assert!(
        err.to_string()
            .contains("declares no [providers.kimi] profile")
    );
}

#[test]
fn invalid_mapping_rejects_unknown_kimi_provider_type() {
    let err = ProviderKeyMapping::from_toml(
        r#"
[[providers]]
id = ["solo"]
name = "Solo"
agents = ["kimi"]
base_url = "https://api.solo.example/v1"

[providers.kimi]
provider_type = "bogus"
"#,
    )
    .expect_err("unknown kimi provider_type fails");

    assert!(
        err.to_string()
            .contains("kimi.provider_type must be one of")
    );
}

#[test]
fn invalid_mapping_rejects_kimi_profile_without_agent_support_or_base() {
    let err = ProviderKeyMapping::from_toml(
        r#"
[[providers]]
id = ["solo"]
name = "Solo"
agents = ["opencode"]

[providers.kimi]
provider_type = "openai"
"#,
    )
    .expect_err("kimi profile on a non-kimi provider fails");
    assert!(err.to_string().contains("does not support `kimi`"));

    let err = ProviderKeyMapping::from_toml(
        r#"
[[providers]]
id = ["solo"]
name = "Solo"
agents = ["kimi"]

[providers.kimi]
provider_type = "openai"
"#,
    )
    .expect_err("kimi row without base fails");
    assert!(err.to_string().contains("declares no base_url"));
}

#[test]
fn invalid_mapping_rejects_kimi_profile_with_a_templated_base() {
    let env_vars = r#"
[[api_keys]]
env_var = "EXAMPLE_API_KEY"
provider_ids = ["example"]
companion_env_vars = ["EXAMPLE_ACCOUNT_ID"]
optional_env_vars = []
"#;
    let err = ProviderKeyMapping::from_toml_parts(
        env_vars,
        r#"
[[providers]]
id = ["example"]
name = "Example"
agents = ["kimi"]
base_url = "https://api.example.com/{EXAMPLE_ACCOUNT_ID}/v1"

[providers.kimi]
provider_type = "openai"
"#,
    )
    .expect_err("kimi row with a templated base fails");

    assert!(err.to_string().contains("must be literal"), "{err}");
}

#[test]
fn invalid_mapping_rejects_hermes_provider_without_profile() {
    let err = ProviderKeyMapping::from_toml(
        r#"
[[providers]]
id = ["solo"]
name = "Solo"
agents = ["hermes"]
"#,
    )
    .expect_err("hermes-enabled provider without profile fails");

    assert!(
        err.to_string()
            .contains("declares no [providers.hermes] profile")
    );
}

#[test]
fn invalid_mapping_rejects_unknown_hermes_api_mode() {
    let err = ProviderKeyMapping::from_toml(
        r#"
[[providers]]
id = ["solo"]
name = "Solo"
agents = ["hermes"]

[providers.hermes]
api_mode = "bogus"
"#,
    )
    .expect_err("unknown hermes api_mode fails");

    assert!(err.to_string().contains("hermes.api_mode must be one of"));
}

#[test]
fn invalid_mapping_rejects_hermes_profile_without_agent_support() {
    let err = ProviderKeyMapping::from_toml(
        r#"
[[providers]]
id = ["solo"]
name = "Solo"
agents = ["pi"]

[providers.hermes]
api_mode = "chat_completions"
"#,
    )
    .expect_err("hermes profile without hermes support fails");

    assert!(err.to_string().contains("does not support `hermes`"));
}

#[test]
fn hermes_profile_may_omit_api_mode_to_refuse_overrides() {
    // A profile without api_mode marks an unknown wire transport, so the pair
    // refuses endpoint overrides instead of guessing one.
    ProviderKeyMapping::from_toml(
        r#"
[[providers]]
id = ["solo"]
name = "Solo"
agents = ["hermes"]

[providers.hermes]
"#,
    )
    .expect("hermes profile without api_mode validates");
}
