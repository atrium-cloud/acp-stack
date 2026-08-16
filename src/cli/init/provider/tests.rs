use super::*;

fn summary_for(agent_id: &str, provider_id: &str) -> AgentProviderSummary {
    providers_for_agent(agent_id)
        .into_iter()
        .find(|summary| summary.id == provider_id)
        .unwrap_or_else(|| panic!("{agent_id}/{provider_id} summary should exist"))
}

fn readiness_config(agent_id: &str) -> Config {
    config::load_config_from_str(&format!(
        r#"
[api]
bind = "127.0.0.1:7700"
public_url = "http://127.0.0.1:7700"
max_request_bytes = 104857600

[security.http]
max_request_bytes = 104857600
rate_limit_per_minute = 120
burst = 30
auth_failures_per_minute = 5
auth_block_duration = "15m"
allowed_origins = []
trust_proxy_headers = false

[workspace]
root = "/workspace"
uploads = "/workspace/uploads"
default_shell = "/bin/bash"
runtime_user = "acp"
max_file_bytes = 8388608

[logging]
level = "info"
local_retention_days = 30

[logging.supabase]
enabled = false
url = "https://example.supabase.co"
api_key_ref = "SUPABASE_SECRET_KEY"
schema = "acp_stack"

[agent]
id = "{agent_id}"
name = "Test Agent"
command = "{agent_id}"
args = []
cwd = "/workspace"
env = []
restart = "on-crash"
"#
    ))
    .expect("config parses")
}

#[test]
fn provider_readiness_reports_missing_default_secret_ref() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let secret_store = SecretStore::open_or_create(tempdir.path()).expect("secret store");
    let summary = summary_for("opencode", "openai");

    assert_eq!(
        provider_readiness_label(&readiness_config("opencode"), &summary, &secret_store),
        "missing OPENAI_API_KEY"
    );
}

#[test]
fn provider_readiness_reports_present_default_secret_ref() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let mut secret_store = SecretStore::open_or_create(tempdir.path()).expect("secret store");
    secret_store
        .set_many([("OPENAI_API_KEY", "test-openai-key")])
        .expect("secret should be stored");
    let summary = summary_for("opencode", "openai");

    assert_eq!(
        provider_readiness_label(&readiness_config("opencode"), &summary, &secret_store),
        "ready"
    );
    assert!(provider_has_available_secret_refs(
        &readiness_config("opencode"),
        &summary,
        &secret_store
    ));
}

#[test]
fn provider_readiness_reports_missing_companion_secret_refs() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let mut secret_store = SecretStore::open_or_create(tempdir.path()).expect("secret store");
    secret_store
        .set_many([("CLOUDFLARE_API_TOKEN", "test-cloudflare-token")])
        .expect("secret should be stored");
    let summary = summary_for("opencode", "cloudflare-ai-gateway");

    assert_eq!(
        provider_readiness_label(&readiness_config("opencode"), &summary, &secret_store),
        "missing CLOUDFLARE_ACCOUNT_ID, CLOUDFLARE_GATEWAY_ID"
    );
    assert!(!provider_has_available_secret_refs(
        &readiness_config("opencode"),
        &summary,
        &secret_store
    ));
}

#[test]
fn provider_readiness_reports_a_distinct_custom_id_for_provider_without_default_ref() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let secret_store = SecretStore::open_or_create(tempdir.path()).expect("secret store");
    let summary = summary_for("opencode", "helicone");

    assert_eq!(
        provider_readiness_label(&readiness_config("opencode"), &summary, &secret_store),
        "needs a distinct custom id"
    );
}

#[test]
fn provider_readiness_reports_native_auth_only_for_known_native_auth_provider() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let secret_store = SecretStore::open_or_create(tempdir.path()).expect("secret store");
    let summary = summary_for("codex", "openai");

    assert_eq!(
        provider_readiness_label(&readiness_config("codex"), &summary, &secret_store),
        "agent-native auth"
    );
}

#[test]
fn provider_readiness_reports_claude_code_vertex_companion_refs() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let secret_store = SecretStore::open_or_create(tempdir.path()).expect("secret store");
    let summary = summary_for("claude-code", "google-vertex-anthropic");

    assert_eq!(
        provider_readiness_label(&readiness_config("claude-code"), &summary, &secret_store),
        "missing ANTHROPIC_VERTEX_PROJECT_ID, CLOUD_ML_REGION"
    );
    assert!(!provider_has_available_secret_refs(
        &readiness_config("claude-code"),
        &summary,
        &secret_store
    ));
}

#[test]
fn claude_code_custom_provider_defaults_to_anthropic_messages() {
    assert_eq!(
        default_init_custom_provider_api("claude-code"),
        CustomProviderApi::AnthropicMessages
    );
    assert_eq!(
        parse_init_custom_provider_api(None, default_init_custom_provider_api("claude-code"))
            .expect("default parses"),
        CustomProviderApi::AnthropicMessages
    );
    assert_eq!(
        parse_init_custom_provider_api(
            Some("anthropic-messages"),
            default_init_custom_provider_api("opencode"),
        )
        .expect("explicit Anthropic Messages API parses"),
        CustomProviderApi::AnthropicMessages
    );
}

#[test]
fn provider_readiness_reports_ready_from_catalog_only_credential() {
    use crate::secrets::{ProviderCredential, ProviderCredentialSet};
    use std::collections::BTreeMap;
    let tempdir = tempfile::tempdir().expect("tempdir");
    let mut secret_store = SecretStore::open_or_create(tempdir.path()).expect("secret store");
    secret_store
        .replace_provider_credentials(
            BTreeMap::from([(
                "openai".to_owned(),
                ProviderCredentialSet::aliasless(ProviderCredential::new(
                    BTreeMap::from([("OPENAI_API_KEY".to_owned(), "catalog-key".to_owned())]),
                    BTreeMap::new(),
                )),
            )]),
            &[],
        )
        .expect("catalog");
    let summary = summary_for("opencode", "openai");
    let config = readiness_config("opencode");

    assert_eq!(
        provider_readiness_label(&config, &summary, &secret_store),
        "ready"
    );
    assert!(provider_has_available_secret_refs(
        &config,
        &summary,
        &secret_store
    ));
}

fn seed_catalog_credential(
    secret_store: &mut SecretStore,
    provider_id: &str,
    env_name: &str,
    value: &str,
) {
    use crate::secrets::{ProviderCredential, ProviderCredentialSet};
    use std::collections::BTreeMap;
    secret_store
        .replace_provider_credentials(
            BTreeMap::from([(
                provider_id.to_owned(),
                ProviderCredentialSet::aliasless(ProviderCredential::new(
                    BTreeMap::from([(env_name.to_owned(), value.to_owned())]),
                    BTreeMap::new(),
                )),
            )]),
            &[],
        )
        .expect("catalog");
}

fn custom_provider_readiness_config() -> Config {
    let mut config = readiness_config("opencode");
    config.agent.env = vec!["CUSTOM_KEY".to_owned()];
    config.agent.provider = Some(AgentProviderConfig {
        id: "my-custom".to_owned(),
        model: Some("my-model".to_owned()),
        api_key_ref: Some("CUSTOM_KEY".to_owned()),
        custom: Some(AgentCustomProviderConfig {
            name: "My Custom".to_owned(),
            base_url: "https://example.test/v1".to_owned(),
            api: CustomProviderApi::default(),
            model_name: None,
            context: DEFAULT_CUSTOM_MODEL_CONTEXT,
            output_max_tokens: DEFAULT_CUSTOM_MODEL_OUTPUT_MAX_TOKENS,
        }),
    });
    config
}

#[test]
fn hosted_gate_defers_missing_provider_ref_but_not_flat_only_refs() {
    use std::sync::Arc;
    let tempdir = tempfile::tempdir().expect("tempdir");
    let mut secret_store = SecretStore::open_or_create(tempdir.path()).expect("secret store");
    let config = custom_provider_readiness_config();
    let required = vec!["CUSTOM_KEY".to_owned()];

    // Local non-interactive run keeps the hard failure.
    let error = collect_missing_provider_refs(
        false,
        &mut secret_store,
        &config,
        Some("my-custom"),
        &required,
    )
    .expect_err("hard failure without hosted driver");
    assert!(error.to_string().contains("CUSTOM_KEY"));

    // Hosted run with a null answer defers the provider ref to the
    // managed credential push instead of failing.
    let driver = Arc::new(prompt::RecordingPromptDriver::default());
    prompt::with_hosted_driver(driver, || {
        collect_missing_provider_refs(
            true,
            &mut secret_store,
            &config,
            Some("my-custom"),
            &required,
        )
        .expect("hosted soft-pass");
        // Refs without a provider context (MCP refs on the prepared-config
        // path) never soft-pass.
        collect_missing_provider_refs(true, &mut secret_store, &config, None, &required)
            .expect_err("flat-only refs keep the hard failure");
        // Mapped providers keep the hard failure even in hosted runs: their
        // model-discovery spawn would fail on the missing ref anyway.
        let mapped_config = readiness_config("opencode");
        collect_missing_provider_refs(
            true,
            &mut secret_store,
            &mapped_config,
            Some("openai"),
            &["OPENAI_API_KEY".to_owned()],
        )
        .expect_err("mapped providers keep the hard failure under hosted");
    });
}

#[test]
fn provider_gate_and_idempotence_verifier_accept_catalog_only_credential() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let mut secret_store = SecretStore::open_or_create(tempdir.path()).expect("secret store");
    seed_catalog_credential(&mut secret_store, "my-custom", "CUSTOM_KEY", "catalog-key");
    let config = custom_provider_readiness_config();
    let required = vec!["CUSTOM_KEY".to_owned()];

    collect_missing_provider_refs(
        false,
        &mut secret_store,
        &config,
        Some("my-custom"),
        &required,
    )
    .expect("catalog credential satisfies the gate");

    let registry = crate::runtime::install::agent_registry::RegistryCatalog::load_embedded()
        .expect("registry");
    assert!(configured_provider_refs_satisfied(
        &registry,
        &config,
        &secret_store
    ));
}

#[test]
fn provider_readiness_label_reports_ready_with_secret() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let mut secret_store = SecretStore::open_or_create(tempdir.path()).expect("secret store");
    secret_store
        .set_many([("OPENAI_API_KEY", "test-openai-key")])
        .expect("secret should be stored");
    let summary = summary_for("opencode", "openai");

    assert_eq!(
        provider_readiness_label(&readiness_config("opencode"), &summary, &secret_store),
        "ready"
    );
}
