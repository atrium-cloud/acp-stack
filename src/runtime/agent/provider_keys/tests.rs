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
    assert!(provider_id_supports_agent("anthropic", "claude-code"));
    assert!(provider_id_supports_agent("amazon-bedrock", "claude-code"));
    assert!(provider_id_supports_agent(
        "google-vertex-anthropic",
        "claude-code"
    ));
    assert!(provider_id_supports_agent(
        "microsoft-foundry",
        "claude-code"
    ));
    assert!(provider_id_supports_agent("moonshotai", "claude-code"));
    assert!(provider_id_supports_agent(
        "xiaomi-token-plan-sgp",
        "claude-code"
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
        env_var_for_agent_provider_id("claude-code", "moonshotai"),
        Some("MOONSHOT_API_KEY")
    );
    assert!(provider_id_supports_agent("moonshotai-cn", "claude-code"));
    assert_eq!(
        env_var_for_agent_provider_id("claude-code", "moonshotai-cn"),
        Some("MOONSHOT_API_KEY")
    );
    assert!(provider_id_supports_agent("kimi-coding", "claude-code"));
    assert!(provider_id_supports_agent("kimi-for-coding", "claude-code"));
    assert_eq!(
        agent_provider_id_for_provider_id("claude-code", "kimi-coding"),
        Some("kimi-coding-plan")
    );
    assert_eq!(
        env_var_for_agent_provider_id("claude-code", "kimi-coding"),
        Some("KIMI_API_KEY")
    );
    assert_eq!(
        env_var_for_agent_provider_id("claude-code", "kimi-for-coding"),
        Some("KIMI_API_KEY")
    );
    assert_eq!(
        env_var_for_agent_provider_id("opencode", "kimi-for-coding"),
        Some("KIMI_API_KEY")
    );
    assert_eq!(
        env_var_for_agent_provider_id("claude-code", "amazon-bedrock"),
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
    // `inspect_pi` maps a `defaultProvider` value through
    // `canonical_provider_id_for_agent_native_id("pi", ...)`. Pi's native
    // ids match acps canonical ids for shared providers and differ only
    // where the mapping declares an alias.
    assert_eq!(
        canonical_provider_id_for_agent_native_id("pi", "anthropic"),
        Some("anthropic")
    );
    assert_eq!(
        canonical_provider_id_for_agent_native_id("pi", "openai"),
        Some("openai")
    );
    // Pi's `vercel-ai-gateway`/`fireworks`/`together` native ids collapse
    // to the same canonical id acps stores.
    assert_eq!(
        canonical_provider_id_for_agent_native_id("pi", "vercel-ai-gateway"),
        Some("vercel-ai-gateway")
    );
    assert_eq!(
        canonical_provider_id_for_agent_native_id("pi", "fireworks"),
        Some("fireworks")
    );
    // A provider Pi lists but acps does not map for `pi` yields no
    // canonical id, so the import surfaces an incompatible candidate.
    assert_eq!(
        canonical_provider_id_for_agent_native_id("pi", "totally-unknown-provider"),
        None
    );
}

#[test]
fn direct_agent_env_refs_are_data_driven() {
    assert_eq!(env_refs_for_agent_id("amp"), ["AMP_API_KEY"]);
    assert_eq!(env_refs_for_agent_id("kimi"), ["KIMI_API_KEY"]);
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
        required_env_refs_for_agent_provider_id("claude-code", "google-vertex-anthropic", None),
        ["ANTHROPIC_VERTEX_PROJECT_ID", "CLOUD_ML_REGION"]
    );
    assert_eq!(
        required_env_refs_for_agent_provider_id(
            "claude-code",
            "microsoft-foundry",
            Some("ANTHROPIC_FOUNDRY_API_KEY")
        ),
        ["ANTHROPIC_FOUNDRY_API_KEY", "ANTHROPIC_FOUNDRY_BASE_URL"]
    );
    assert!(provider_uses_agent_native_auth(
        "claude-code",
        "amazon-bedrock"
    ));
    // The Claude Code profile declares an explicit optional list so the
    // Pi-only auth overrides do not leak in via provider-level fallback.
    let bedrock_optional = optional_env_refs_for_agent_provider_id("claude-code", "amazon-bedrock");
    assert!(bedrock_optional.contains(&"AWS_PROFILE"));
    assert!(!bedrock_optional.contains(&"AWS_BEDROCK_SKIP_AUTH"));
    assert!(!bedrock_optional.contains(&"AWS_BEDROCK_FORCE_HTTP1"));
    assert!(!bedrock_optional.contains(&"AWS_BEDROCK_FORCE_CACHE"));
    assert!(provider_uses_agent_native_auth(
        "claude-code",
        "google-vertex-anthropic"
    ));
    assert!(!provider_uses_agent_native_auth(
        "claude-code",
        "microsoft-foundry"
    ));

    let summaries = providers_for_agent("claude-code");
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
        assert_eq!(
            env_var_for_agent_provider_id("claude-code", provider_id),
            None
        );
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
    .expect_err("claude_code profile without claude-code support fails");

    assert!(err.to_string().contains("does not support `claude-code`"));
}

#[test]
fn invalid_mapping_rejects_claude_code_role_models_without_default_model() {
    let err = ProviderKeyMapping::from_toml(
        r#"
[[providers]]
id = ["solo"]
name = "Solo"
agents = ["claude-code"]

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
agents = ["claude-code"]

[providers.api_key_env_vars]
claude-code = "SOLO_API_KEY"

[providers.claude_code]
agent_native_auth = true
"#,
    )
    .expect_err("native auth with claude-code api key env var fails");

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
agents = ["claude-code"]

[providers.claude_code]
agent_native_auth = true
"#,
    )
    .expect_err("native auth with api key mapping fails");

    assert!(err.to_string().contains("native auth"));
}
