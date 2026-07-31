use acp_stack::config::{
    AgentAdapterConfig, ArrayTargetConfig, CustomProviderApi, DEFAULT_CUSTOM_MODEL_CONTEXT,
    DEFAULT_CUSTOM_MODEL_OUTPUT_MAX_TOKENS, default_config_path, load_config_from_str,
};

mod common;
use common::config::VALID_CONFIG;

#[test]
fn default_config_path_uses_acps_config_toml() {
    let path = default_config_path().expect("default config path");

    assert!(path.ends_with(".config/acp-stack/acps-config.toml"));
}

#[test]
fn parses_valid_config_and_exports_canonical_toml() {
    let config = load_config_from_str(VALID_CONFIG).expect("valid config should parse");

    assert_eq!(config.api.bind, "127.0.0.1:7700");
    assert_eq!(config.workspace.root, "/workspace");
    assert_eq!(config.agent.restart, "on-crash");

    let canonical = config
        .to_canonical_toml()
        .expect("canonical TOML should serialize");
    let round_tripped =
        load_config_from_str(&canonical).expect("canonical TOML should parse as config");

    assert_eq!(round_tripped.agent.id, "opencode");
    assert!(round_tripped.agent.adapter.is_none());
    assert!(canonical.contains("[security.http]"));
    assert!(!canonical.contains("[agent.adapter]"));
    assert!(canonical.contains("[array]"));
    assert!(canonical.contains("[array.targets.agent.install]"));
}

#[test]
fn legacy_agent_section_loads_as_primary_array_target() {
    let config = load_config_from_str(VALID_CONFIG).expect("legacy config should parse");

    assert!(!config.array.enabled);
    assert_eq!(config.array.primary_target, "opencode");
    assert_eq!(config.array.targets.len(), 1);
    assert_eq!(config.array.targets[0].id, "opencode");
    assert_eq!(config.array.targets[0].agent.id, config.agent.id);
}

#[test]
fn canonical_export_writes_array_shape_without_legacy_agent_section() {
    let mut config = load_config_from_str(VALID_CONFIG).expect("legacy config should parse");
    let mut second_agent = config.agent.clone();
    second_agent.id = "codex".to_owned();
    second_agent.name = "Codex".to_owned();
    second_agent.command = "codex".to_owned();
    config.array.enabled = true;
    config.array.targets.push(ArrayTargetConfig {
        id: "codex".to_owned(),
        agent: second_agent,
    });

    let canonical = config.to_canonical_toml().expect("canonical export");
    let reparsed = load_config_from_str(&canonical).expect("canonical array config parses");

    assert!(canonical.contains("[array]"));
    assert!(!canonical.contains("\n[agent]\n"));
    assert_eq!(reparsed.array.targets.len(), 2);
    assert_eq!(reparsed.array.targets[1].id, "codex");
    assert_eq!(reparsed.array.targets[1].agent.id, "codex");
}

#[test]
fn canonical_export_renames_primary_target_from_agent_mirror() {
    let mut config = load_config_from_str(VALID_CONFIG).expect("legacy config should parse");
    config.agent.id = "placebo".to_owned();
    config.agent.name = "Placebo".to_owned();
    config.agent.command = "placebo-agent".to_owned();

    let canonical = config.to_canonical_toml().expect("canonical export");
    let reparsed = load_config_from_str(&canonical).expect("canonical array config parses");

    assert_eq!(reparsed.array.primary_target, "placebo");
    assert_eq!(reparsed.array.targets.len(), 1);
    assert_eq!(reparsed.array.targets[0].id, "placebo");
    assert_eq!(reparsed.array.targets[0].agent.id, "placebo");
}

#[test]
fn rejects_array_target_id_agent_id_mismatch() {
    let mut config = load_config_from_str(VALID_CONFIG).expect("legacy config should parse");
    let mut second_agent = config.agent.clone();
    second_agent.id = "codex".to_owned();
    second_agent.name = "Codex".to_owned();
    second_agent.command = "codex".to_owned();
    config.array.targets.push(ArrayTargetConfig {
        id: "agent-0".to_owned(),
        agent: second_agent,
    });

    let canonical = config.to_canonical_toml().expect("canonical export");
    let error = load_config_from_str(&canonical).expect_err("mismatched target rejected");

    assert!(error.to_string().contains("must match agent id"), "{error}");
}

#[test]
fn rejects_duplicate_array_harnesses() {
    let mut config = load_config_from_str(VALID_CONFIG).expect("legacy config should parse");
    config.array.targets.push(ArrayTargetConfig {
        id: "opencode".to_owned(),
        agent: config.agent.clone(),
    });

    let canonical = config.to_canonical_toml().expect("canonical export");
    let error = load_config_from_str(&canonical).expect_err("duplicate harnesses rejected");

    assert!(
        error.to_string().contains("requires different harnesses"),
        "{error}"
    );
}

#[test]
fn rejects_dangling_primary_target() {
    // The coordination invariant: primary_target must name a real target, or
    // the Array has no distinguished coordinator.
    let mut config = load_config_from_str(VALID_CONFIG).expect("legacy config should parse");
    config.array.primary_target = "does-not-exist".to_owned();

    let canonical = config.to_canonical_toml().expect("canonical export");
    let error = load_config_from_str(&canonical).expect_err("dangling primary rejected");

    assert!(
        error
            .to_string()
            .contains("must reference an entry in array.targets"),
        "{error}"
    );
}

#[test]
fn rejects_invalid_array_target_id() {
    let mut config = load_config_from_str(VALID_CONFIG).expect("legacy config should parse");
    let mut second = config.agent.clone();
    second.id = "bad id".to_owned();
    second.name = "Bad".to_owned();
    second.command = "bad".to_owned();
    config.array.targets.push(ArrayTargetConfig {
        id: "bad id".to_owned(),
        agent: second,
    });

    let canonical = config.to_canonical_toml().expect("canonical export");
    let error = load_config_from_str(&canonical).expect_err("invalid target id rejected");

    assert!(
        error
            .to_string()
            .contains("must start with an ASCII letter or digit"),
        "{error}"
    );
}

#[test]
fn canonical_export_keeps_secret_refs_and_omits_secret_values() {
    let config = load_config_from_str(
        &VALID_CONFIG
            .replace(
                r#"env = ["OPENCODE_API_KEY"]"#,
                r#"env = ["OPENAI_API_KEY"]"#,
            )
            .replace(
                r#"api_key_ref = "SUPABASE_SECRET_KEY""#,
                r#"api_key_ref = "SUPABASE_KEY_REF""#,
            ),
    )
    .expect("config with refs should parse");

    let canonical = config
        .to_canonical_toml()
        .expect("canonical TOML should serialize");
    assert!(canonical.contains("OPENAI_API_KEY"));
    assert!(canonical.contains("SUPABASE_KEY_REF"));
    for secret_value in [
        "sk-proj-exampleinlinevalue",
        "github_pat_exampleinlinevalue",
        "acps_exampleinlinevalue",
    ] {
        assert!(
            !canonical.contains(secret_value),
            "canonical export leaked {secret_value}"
        );
    }
}

#[test]
fn parses_custom_provider_defaults() {
    let config_text = format!(
        "{VALID_CONFIG}\n\
         [agent.provider]\n\
         id = \"myprovider\"\n\
         model = \"my-model\"\n\
         api_key_ref = \"CUSTOM_API_KEY\"\n\n\
         [agent.provider.custom]\n\
         name = \"My Provider\"\n\
         base_url = \"https://api.myprovider.example/v1\"\n"
    );

    let config = load_config_from_str(&config_text).expect("custom provider config should parse");
    let custom = config
        .agent
        .provider
        .as_ref()
        .and_then(|provider| provider.custom.as_ref())
        .expect("custom provider should load");

    assert_eq!(custom.api, CustomProviderApi::ChatCompletions);
    assert_eq!(custom.context, DEFAULT_CUSTOM_MODEL_CONTEXT);
    assert_eq!(
        custom.output_max_tokens,
        DEFAULT_CUSTOM_MODEL_OUTPUT_MAX_TOKENS
    );
}

#[test]
fn parses_custom_provider_anthropic_messages_api() {
    let claude_code_config = VALID_CONFIG
        .replace(r#"id = "opencode""#, r#"id = "claude-code""#)
        .replace(r#"name = "OpenCode""#, r#"name = "Claude Code""#)
        .replace(r#"command = "opencode""#, r#"command = "claude-agent-acp""#)
        .replace(r#"args = ["acp"]"#, r#"args = []"#)
        .replace(
            r#"env = ["OPENCODE_API_KEY"]"#,
            r#"env = ["CUSTOM_API_KEY"]"#,
        );
    let config_text = format!(
        "{claude_code_config}\n\
         [agent.provider]\n\
         id = \"myprovider\"\n\
         model = \"my-model\"\n\
         api_key_ref = \"CUSTOM_API_KEY\"\n\n\
         [agent.provider.custom]\n\
         name = \"My Provider\"\n\
         base_url = \"https://api.myprovider.example/anthropic\"\n\
         api = \"anthropic-messages\"\n"
    );

    let config = load_config_from_str(&config_text)
        .expect("Anthropic Messages custom provider config should parse");
    let custom = config
        .agent
        .provider
        .as_ref()
        .and_then(|provider| provider.custom.as_ref())
        .expect("custom provider should load");

    assert_eq!(custom.api, CustomProviderApi::AnthropicMessages);
    let canonical = config.to_canonical_toml().expect("canonical export");
    assert!(canonical.contains(r#"api = "anthropic-messages""#));
}

#[test]
fn rejects_custom_provider_anthropic_messages_for_non_claude_agent() {
    let config_text = format!(
        "{VALID_CONFIG}\n\
         [agent.provider]\n\
         id = \"myprovider\"\n\
         model = \"my-model\"\n\
         api_key_ref = \"CUSTOM_API_KEY\"\n\n\
         [agent.provider.custom]\n\
         name = \"My Provider\"\n\
         base_url = \"https://api.myprovider.example/anthropic\"\n\
         api = \"anthropic-messages\"\n"
    );

    let error =
        load_config_from_str(&config_text).expect_err("non-Claude anthropic custom provider fails");

    assert!(
        error
            .to_string()
            .contains("anthropic-messages custom providers only support Claude Code"),
        "{error}"
    );
}

#[test]
fn parses_subagent_provider_config() {
    let config_text = format!(
        "{VALID_CONFIG}\n\
         [agent.subagent.provider]\n\
         id = \"opencode-go\"\n\
         model = \"opencode-go/deepseek-v4-flash\"\n\
         api_key_ref = \"OPENCODE_API_KEY\"\n"
    );

    let config = load_config_from_str(&config_text).expect("subagent provider config should parse");
    let provider = config
        .agent
        .subagent
        .as_ref()
        .and_then(|subagent| subagent.provider.as_ref())
        .expect("subagent provider should load");

    assert_eq!(provider.id, "opencode-go");
    assert_eq!(
        provider.model.as_deref(),
        Some("opencode-go/deepseek-v4-flash")
    );
    assert_eq!(provider.api_key_ref.as_deref(), Some("OPENCODE_API_KEY"));
}

#[test]
fn parses_and_round_trips_active_provider_alias_config() {
    let config_text = format!(
        "{VALID_CONFIG}\n\
         [agent.provider]\n\
         id = \"opencode-go\"\n\n\
         [agent.providers]\n\
         active = [\"opencode-go\", \"openrouter\"]\n\n\
         [agent.providers.selected_aliases]\n\
         opencode-go = \"go_2\"\n"
    );

    let config = load_config_from_str(&config_text).expect("provider set config parses");
    let providers = config.agent.providers.as_ref().expect("providers");
    assert_eq!(providers.active, ["opencode-go", "openrouter"]);
    assert_eq!(
        providers
            .selected_aliases
            .get("opencode-go")
            .map(String::as_str),
        Some("go_2")
    );

    let canonical = config.to_canonical_toml().expect("canonical");
    let round_trip = load_config_from_str(&canonical).expect("round trip");
    assert_eq!(round_trip.agent.providers, config.agent.providers);
    assert!(canonical.contains("[array.targets.agent.providers]"));
}

#[test]
fn parses_subagent_custom_provider_config() {
    let config_text = format!(
        "{VALID_CONFIG}\n\
         [agent.subagent.provider]\n\
         id = \"myprovider\"\n\
         model = \"my-model\"\n\
         api_key_ref = \"CUSTOM_API_KEY\"\n\n\
         [agent.subagent.provider.custom]\n\
         name = \"My Provider\"\n\
         base_url = \"https://api.myprovider.example/v1\"\n"
    );

    let config =
        load_config_from_str(&config_text).expect("subagent custom provider config should parse");
    let custom = config
        .agent
        .subagent
        .as_ref()
        .and_then(|subagent| subagent.provider.as_ref())
        .and_then(|provider| provider.custom.as_ref())
        .expect("subagent custom provider should load");

    assert_eq!(custom.api, CustomProviderApi::ChatCompletions);
    assert_eq!(custom.context, DEFAULT_CUSTOM_MODEL_CONTEXT);
    assert_eq!(
        custom.output_max_tokens,
        DEFAULT_CUSTOM_MODEL_OUTPUT_MAX_TOKENS
    );
}

#[test]
fn rejects_custom_provider_without_api_key_ref() {
    let config_text = format!(
        "{VALID_CONFIG}\n\
         [agent.provider]\n\
         id = \"myprovider\"\n\
         model = \"my-model\"\n\n\
         [agent.provider.custom]\n\
         name = \"My Provider\"\n\
         base_url = \"https://api.myprovider.example/v1\"\n"
    );

    let error =
        load_config_from_str(&config_text).expect_err("custom provider without ref should fail");

    assert!(
        error
            .to_string()
            .contains("agent.provider.api_key_ref is required")
    );
}

#[test]
fn rejects_operator_written_agent_adapter() {
    // [agent.adapter] is runtime-populated from the embedded registry, not
    // operator-written. A config carrying it over from the pre-rework shape
    // should fail with a clear unknown-field error rather than silently
    // shadowing what the registry would have resolved.
    let config = VALID_CONFIG.replace(
        r#"restart = "on-crash""#,
        r#"restart = "on-crash"

[agent.adapter]
id = "codex-acp"
name = "Codex ACP Adapter"
upstream_agent = "codex-cli"
source_url = "https://github.com/agentclientprotocol/codex-acp""#,
    );
    let error =
        load_config_from_str(&config).expect_err("operator-written adapter must be rejected");
    let message = error.to_string();
    assert!(
        message.contains("unknown field") && message.contains("adapter"),
        "{error}"
    );
}

#[test]
fn canonical_export_omits_runtime_adapter_metadata() {
    let mut config = load_config_from_str(VALID_CONFIG).expect("valid config should parse");
    config.agent.adapter = Some(AgentAdapterConfig {
        id: "codex-acp".to_owned(),
        name: "Codex".to_owned(),
        upstream_agent: "codex-cli".to_owned(),
        source_url: Some("https://github.com/agentclientprotocol/codex-acp".to_owned()),
    });

    let canonical = config
        .to_canonical_toml()
        .expect("canonical TOML should serialize");
    assert!(!canonical.contains("[agent.adapter]"));
    let round_tripped =
        load_config_from_str(&canonical).expect("canonical TOML should parse as config");
    assert!(round_tripped.agent.adapter.is_none());
}

#[test]
fn rejects_invalid_agent_restart_policy() {
    let error = load_config_from_str(
        &VALID_CONFIG.replace(r#"restart = "on-crash""#, r#"restart = "always""#),
    )
    .expect_err("config should be invalid");

    assert!(
        error
            .to_string()
            .contains("agent.restart must be one of never, on-crash")
    );
}

#[test]
fn rejects_blank_agent_mode() {
    let error = load_config_from_str(&VALID_CONFIG.replace(
        r#"restart = "on-crash""#,
        "restart = \"on-crash\"\nmode = \" \"",
    ))
    .expect_err("config should be invalid");

    assert!(error.to_string().contains("agent.mode is required"));
}

#[test]
fn rejects_blank_agent_model() {
    let error = load_config_from_str(&VALID_CONFIG.replace(
        r#"restart = "on-crash""#,
        "restart = \"on-crash\"\nmodel = \" \"",
    ))
    .expect_err("config should be invalid");

    assert!(error.to_string().contains("agent.model is required"));
}

#[test]
fn rejects_root_model_when_provider_model_is_set() {
    let config = VALID_CONFIG.replace(
        r#"restart = "on-crash""#,
        "restart = \"on-crash\"\nmodel = \"root-model\"",
    ) + "\n[agent.provider]\nid = \"openai\"\nmodel = \"openai/gpt-5.5\"\napi_key_ref = \"OPENAI_API_KEY\"\n";

    let error = load_config_from_str(&config).expect_err("dual model config should fail");
    assert!(
        error
            .to_string()
            .contains("must be omitted when agent.provider.model is set")
    );
}

#[test]
fn rejects_empty_expected_sha256() {
    let config = VALID_CONFIG.replace(
        r#"restart = "on-crash""#,
        "expected_sha256 = \"\"\nrestart = \"on-crash\"",
    );

    let error = load_config_from_str(&config).expect_err("empty expected_sha256 should fail");

    assert!(
        error
            .to_string()
            .contains("agent.expected_sha256 must be exactly 64 lowercase hex characters")
    );
}

#[test]
fn rejects_uppercase_expected_sha256() {
    let valid_hash = "a".repeat(64);
    let upper_hash = "A".repeat(64);
    let config = VALID_CONFIG.replace(
        r#"restart = "on-crash""#,
        &format!("expected_sha256 = \"{upper_hash}\"\nrestart = \"on-crash\""),
    );

    let error = load_config_from_str(&config).expect_err("uppercase hex should fail");
    assert!(
        error
            .to_string()
            .contains("agent.expected_sha256 must be exactly 64 lowercase hex characters")
    );

    // sanity: lowercase form parses fine
    let ok = VALID_CONFIG.replace(
        r#"restart = "on-crash""#,
        &format!("expected_sha256 = \"{valid_hash}\"\nrestart = \"on-crash\""),
    );
    let parsed = load_config_from_str(&ok).expect("lowercase 64-hex should parse");
    assert_eq!(
        parsed.agent.expected_sha256.as_deref(),
        Some(valid_hash.as_str())
    );
}

#[test]
fn rejects_non_hex_expected_sha256() {
    let bad = "z".repeat(64);
    let config = VALID_CONFIG.replace(
        r#"restart = "on-crash""#,
        &format!("expected_sha256 = \"{bad}\"\nrestart = \"on-crash\""),
    );

    let error = load_config_from_str(&config).expect_err("non-hex chars should fail");
    assert!(
        error
            .to_string()
            .contains("agent.expected_sha256 must be exactly 64 lowercase hex characters")
    );
}

#[test]
fn rejects_short_expected_sha256() {
    let short = "a".repeat(63);
    let config = VALID_CONFIG.replace(
        r#"restart = "on-crash""#,
        &format!("expected_sha256 = \"{short}\"\nrestart = \"on-crash\""),
    );

    let error = load_config_from_str(&config).expect_err("63-char hex should fail");
    assert!(
        error
            .to_string()
            .contains("agent.expected_sha256 must be exactly 64 lowercase hex characters")
    );
}

#[test]
fn parses_native_agent_without_adapter_metadata() {
    let parsed = load_config_from_str(VALID_CONFIG).expect("native agent config should parse");
    assert!(parsed.agent.adapter.is_none());
}

#[test]
fn rejects_shell_agent_install_without_shell() {
    let config = VALID_CONFIG.replace(
        r#"shell = "curl -fsSL https://opencode.ai/install | bash"
"#,
        "",
    );
    let error = load_config_from_str(&config).expect_err("shell install should require shell");
    assert!(
        error
            .to_string()
            .contains("agent.install.shell is required"),
        "{error}"
    );
}
