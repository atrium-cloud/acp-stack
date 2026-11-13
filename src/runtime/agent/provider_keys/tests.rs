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
            "openai-api",
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

/// The OpenCode row's ids each answer from their own endpoint: `opencode` and its Hermes alias
/// from Zen, `opencode-go` from Go.
#[test]
fn opencode_ids_answer_from_their_endpoint() {
    const ZEN: &str = "https://opencode.ai/inference/v1";
    const GO: &str = "https://opencode.ai/inference/go/v1";
    let zen = ("OpenCode Zen", ZEN, "opencode-zen", "OPENCODE_ZEN_API_KEY");
    let go = ("OpenCode Go", GO, "opencode-go", "OPENCODE_GO_API_KEY");
    for (provider_id, (name, base, hermes_id, hermes_env)) in [
        ("opencode", zen),
        ("opencode-zen", zen),
        ("opencode-go", go),
    ] {
        assert_eq!(provider_name_for_provider_id(provider_id), Some(name));
        assert_eq!(
            vendor_base_url_for_agent_provider_id("kimi", provider_id),
            Some(base)
        );
        assert_eq!(
            models_url_for_provider_id(provider_id).map(str::to_owned),
            Some(format!("{base}/models"))
        );
        assert_eq!(
            agent_provider_id_for_provider_id("hermes", provider_id),
            Some(hermes_id)
        );
        assert_eq!(
            env_var_for_agent_provider_id("hermes", provider_id),
            Some(hermes_env)
        );
    }
    assert_eq!(
        provider_id_for_agent_vendor_base_url("kimi", ZEN),
        Some("opencode")
    );
    assert_eq!(
        provider_id_for_agent_vendor_base_url("kimi", GO),
        Some("opencode-go")
    );
    assert_eq!(
        provider_ids_for_env_refs(["OPENCODE_ZEN_API_KEY"]),
        BTreeSet::from(["opencode", "opencode-zen"])
    );
    assert_eq!(
        provider_ids_for_env_refs(["OPENCODE_GO_API_KEY"]),
        BTreeSet::from(["opencode-go"])
    );
    assert!(!env_ref_allows_provider("OPENCODE_GO_API_KEY", "opencode"));
    let hermes: Vec<_> = providers_for_agent("hermes")
        .into_iter()
        .filter(|summary| summary.id.starts_with("opencode"))
        .map(|summary| (summary.id, summary.name, summary.agent_provider_id))
        .collect();
    assert_eq!(
        hermes,
        [
            ("opencode", zen.0, Some("opencode-zen")),
            ("opencode-zen", zen.0, None),
            ("opencode-go", go.0, None),
        ]
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
        "kimi-code-plan-cn",
        "kimi-coding-global",
        "kimi-code-plan-global",
    ] {
        let provider = mapping
            .provider_mapping(provider_id)
            .expect("collapsed provider id resolves");
        assert!(provider.ids().iter().any(|id| id == provider_id));
    }
}

#[test]
fn models_dev_only_providers_are_opencode_scoped_without_default_env_refs() {
    for provider_id in ["helicone", "deepinfra", "gitlab", "venice"] {
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
        required_env_refs_for_provider_id("azure", "AZURE_OPENAI_API_KEY"),
        ["AZURE_OPENAI_API_KEY", "AZURE_OPENAI_BASE_URL"]
    );
    assert_eq!(
        optional_env_refs_for_provider_id("azure"),
        [
            "AZURE_OPENAI_RESOURCE_NAME",
            "AZURE_OPENAI_API_VERSION",
            "AZURE_OPENAI_DEPLOYMENT_NAME_MAP"
        ]
    );
    assert_eq!(
        base_url_env_for_provider_id("azure"),
        Some("AZURE_OPENAI_BASE_URL")
    );
    assert_eq!(base_url_env_for_provider_id("openai"), None);
    assert!(provider_id_supports_agent("azure", "pi"));
    assert!(provider_id_supports_agent("azure", "opencode"));
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
    assert!(provider_id_supports_agent("kimi-code-plan-cn", "claude"));
    assert!(provider_id_supports_agent("kimi-coding-global", "opencode"));
    assert!(!provider_id_supports_agent("kimi-coding-global", "claude"));
    assert_eq!(
        agent_provider_id_for_provider_id("claude", "kimi-coding"),
        Some("kimi-coding-plan")
    );
    assert_eq!(
        env_var_for_agent_provider_id("claude", "kimi-coding"),
        Some("KIMI_API_KEY")
    );
    assert_eq!(
        env_var_for_agent_provider_id("claude", "kimi-code-plan-cn"),
        Some("KIMI_API_KEY")
    );
    assert_eq!(
        env_var_for_agent_provider_id("opencode", "kimi-code-plan-cn"),
        Some("KIMI_API_KEY")
    );
    assert_eq!(
        env_var_for_agent_provider_id("opencode", "kimi-code-plan-global"),
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
        agent_provider_id_for_provider_id("pi", "kimi-code-plan-cn"),
        Some("kimi-coding")
    );
    assert_eq!(
        agent_provider_id_for_provider_id("opencode", "kimi-coding"),
        Some("kimi-code-plan-cn")
    );
    assert_eq!(
        agent_provider_id_for_provider_id("opencode", "kimi-coding-global"),
        Some("kimi-code-plan-global")
    );
    assert_eq!(
        agent_provider_id_for_provider_id("kimi", "kimi-code-plan-global"),
        Some("kimi-coding-global")
    );
    assert_eq!(
        agent_provider_id_for_provider_id("hermes", "openai"),
        Some("openai-api")
    );
    assert_eq!(
        agent_provider_id_for_provider_id("pi", "openai"),
        Some("openai")
    );
    assert_eq!(
        agent_provider_id_for_provider_id("hermes", "fireworks-ai"),
        Some("fireworks")
    );
    assert_eq!(
        agent_provider_id_for_provider_id("hermes", "kimi"),
        Some("kimi-coding")
    );
    assert_eq!(agent_provider_id_for_provider_id("amp", "openai"), None);
}

#[test]
fn partial_native_mapping_leaves_unmapped_agents_on_primary_id() {
    let mapping = ProviderKeyMapping::from_toml(
        r#"
[[api_keys]]
env_var = "FIRST_API_KEY"
provider_ids = ["known"]

[[providers]]
id = ["known", "alias"]
name = "Known"
agents = ["pi", "opencode"]

[providers.provider_ids]
alias = "opencode"
"#,
    )
    .expect("a mapping may cover only some agents");
    let provider = mapping.provider_mapping("known").expect("provider");

    assert_eq!(provider.agent_native_provider_id("opencode"), Some("alias"));
    assert_eq!(provider.agent_native_provider_id("pi"), Some("known"));
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
    // Codex's built-in openai lane is key-driven.
    assert!(!provider_uses_agent_native_auth("codex", "openai"));
    assert_eq!(
        env_var_for_agent_provider_id("codex", "openai"),
        Some("OPENAI_API_KEY")
    );

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
fn invalid_mapping_rejects_base_url_env_outside_the_companions() {
    let err = ProviderKeyMapping::from_toml(
        r#"
[[api_keys]]
env_var = "RESOURCE_API_KEY"
provider_ids = ["resource"]
companion_env_vars = ["RESOURCE_BASE_URL"]

[[providers]]
id = ["resource"]
name = "Resource"
agents = ["pi", "opencode"]
base_url_env = "OTHER_BASE_URL"
"#,
    )
    .expect_err("an endpoint env that is not a companion fails");

    assert!(err.to_string().contains(
        "provider `resource` base_url_env `OTHER_BASE_URL` is not one of its companion env vars"
    ));
}

#[test]
fn invalid_mapping_rejects_base_url_env_beside_a_vendor_base() {
    let err = ProviderKeyMapping::from_toml(
        r#"
[[api_keys]]
env_var = "RESOURCE_API_KEY"
provider_ids = ["resource"]
companion_env_vars = ["RESOURCE_BASE_URL"]

[[providers]]
id = ["resource"]
name = "Resource"
agents = ["pi", "opencode"]
base_url = "https://api.resource.example/v1"
base_url_env = "RESOURCE_BASE_URL"
"#,
    )
    .expect_err("a fixed base beside an endpoint env fails");

    assert!(
        err.to_string()
            .contains("provider `resource` declares both a vendor base URL and base_url_env")
    );
}

#[test]
fn invalid_mapping_rejects_unclaimed_ids_and_row_fields_beside_endpoints() {
    let row = |ids: &str, row_fields: &str| {
        format!(
            r#"
[[providers]]
id = {ids}
name = "Vendor"
agents = ["pi"]
base_url = "https://api.vendor.example/v1"
{row_fields}
[[providers.endpoints]]
id = "vendor"
name = "Vendor Zen"
base_url = "https://api.vendor.example/v1"
"#
        )
    };
    let err = ProviderKeyMapping::from_toml(&row(r#"["vendor", "vendor-go"]"#, ""))
        .expect_err("an id no endpoint claims fails");
    assert!(
        err.to_string()
            .contains("id `vendor-go` is claimed by 0 endpoints"),
        "{err}"
    );

    let models_url = r#"models_url = "https://api.vendor.example/v1/models""#;
    let err = ProviderKeyMapping::from_toml(&row(r#"["vendor"]"#, models_url))
        .expect_err("a row-level models_url beside endpoints fails");
    assert!(err.to_string().contains("beside endpoints"), "{err}");
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
    let optional = optional_env_refs_for_provider_id("azure");
    assert!(
        !optional.is_empty(),
        "fixture provider must have optional env vars"
    );
    let primary = env_var_for_provider_id("azure").expect("azure has a canonical env var");
    let mut values = BTreeMap::from([(primary.to_owned(), "az-key".to_owned())]);
    for companion in companion_env_refs_for_provider_id("azure") {
        values.insert(companion.to_owned(), "companion-value".to_owned());
    }
    values.insert(optional[0].to_owned(), "optional-value".to_owned());
    validate_env_keyed_credential_values("azure", &values, "test.values")
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
fn vendor_base_urls_prefer_the_agent_entry() {
    assert_eq!(
        vendor_base_url_for_agent_provider_id("opencode", "anthropic"),
        Some("https://api.anthropic.com/v1")
    );
    assert_eq!(
        vendor_base_url_for_agent_provider_id("kimi", "anthropic"),
        Some("https://api.anthropic.com")
    );
    assert_eq!(
        vendor_base_url_for_agent_provider_id("kimi", "mistral"),
        None
    );
    assert_eq!(
        vendor_base_url_for_agent_provider_id("kimi", "minimax"),
        Some("https://api.minimax.io/anthropic")
    );
    assert_eq!(
        vendor_base_url_for_agent_provider_id("opencode", "deepinfra"),
        None
    );
}

#[test]
fn hermes_launch_bases_come_from_the_row() {
    assert_eq!(
        hermes_launch_base_url("kimi-code"),
        Some(("KIMI_BASE_URL", "https://api.kimi.com/coding"))
    );
    assert_eq!(
        hermes_launch_base_url("stepfun"),
        Some(("STEPFUN_BASE_URL", "https://api.stepfun.com/v1"))
    );
    assert_eq!(hermes_launch_base_url("openrouter"), None);
}

#[test]
fn invalid_mapping_rejects_hermes_base_url_env_without_a_literal_base() {
    for base_urls in [
        "",
        "[providers.base_urls]\nhermes = \"https://{SOLO_HOST}/v1\"\n",
    ] {
        let err = ProviderKeyMapping::from_toml(&format!(
            r#"
[[providers]]
id = ["solo"]
name = "Solo"
agents = ["hermes"]
companion_env_vars = ["SOLO_HOST"]
hermes_base_url_env = "SOLO_BASE_URL"

{base_urls}"#
        ))
        .expect_err("the env var needs a literal base");

        assert!(err.to_string().contains("hermes_base_url_env"), "{err}");
    }
}

/// `opencode.rs` writes only a row's own entry, leaving every other provider on its default.
#[test]
fn agent_base_urls_leave_out_the_row_default() {
    assert_eq!(
        agent_base_url_for_provider_id("opencode", "togetherai"),
        Some("https://api.together.ai/v1")
    );
    assert_eq!(
        agent_base_url_for_provider_id("opencode", "anthropic"),
        None
    );
    assert_eq!(agent_base_url_for_provider_id("opencode", "unknown"), None);
}

#[test]
fn vendor_base_url_templates_name_their_placeholders() {
    assert_eq!(
        base_url_template_placeholders(
            "https://gateway.ai.cloudflare.com/v1/{CLOUDFLARE_ACCOUNT_ID}/{CLOUDFLARE_GATEWAY_ID}/compat"
        ),
        ["CLOUDFLARE_ACCOUNT_ID", "CLOUDFLARE_GATEWAY_ID"]
    );
    assert!(base_url_template_placeholders("https://api.openai.com/v1").is_empty());
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
    }
    assert_eq!(kimi_profile_for_provider_id("mistral"), None);
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
fn provider_id_for_agent_vendor_base_url_matches_rows_by_base() {
    assert_eq!(
        provider_id_for_agent_vendor_base_url("kimi", "https://openrouter.ai/api/v1"),
        Some("openrouter")
    );
    assert_eq!(
        provider_id_for_agent_vendor_base_url("kimi", "https://openrouter.ai/api/v1/"),
        Some("openrouter")
    );
    assert_eq!(
        provider_id_for_agent_vendor_base_url("kimi", "https://api.kimi.com/coding/v1"),
        Some("kimi-coding")
    );
    assert_eq!(
        provider_id_for_agent_vendor_base_url("kimi", "https://api.kimi.ai/coding/v1"),
        Some("kimi-coding-global")
    );
    assert_eq!(
        provider_id_for_agent_vendor_base_url("kimi", "https://api.anthropic.com"),
        Some("anthropic")
    );
    // The row base is per agent: Kimi's Anthropic base is the origin, OpenCode's keeps /v1.
    assert_eq!(
        provider_id_for_agent_vendor_base_url("opencode", "https://api.anthropic.com"),
        None
    );
    assert_eq!(
        provider_id_for_agent_vendor_base_url("kimi", "https://api.mistral.ai/v1"),
        None
    );
    assert_eq!(provider_id_for_agent_vendor_base_url("kimi", ""), None);
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
