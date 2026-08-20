use super::*;
use crate::config::{AgentProviderConfig, AgentProvidersConfig, load_config_from_str};
use crate::secrets::{ProviderCredential, ProviderCredentialSet};
use std::collections::BTreeMap;

fn resolver_config(agent_id: &str) -> Config {
    load_config_from_str(&format!(
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

fn mapped_provider(provider_id: &str, api_key_ref: Option<&str>) -> AgentProviderConfig {
    AgentProviderConfig {
        id: provider_id.to_owned(),
        model: None,
        api_key_ref: api_key_ref.map(str::to_owned),
        custom: None,
    }
}

fn credential(env_name: &str, value: &str) -> ProviderCredential {
    ProviderCredential::new(
        BTreeMap::from([(env_name.to_owned(), value.to_owned())]),
        BTreeMap::new(),
    )
}

fn catalog_store(
    catalog: BTreeMap<String, ProviderCredentialSet>,
) -> (tempfile::TempDir, SecretStore) {
    let home = tempfile::tempdir().expect("home");
    let mut store = SecretStore::open_or_create(home.path()).expect("secret store");
    store
        .replace_provider_credentials(catalog, &[])
        .expect("catalog");
    (home, store)
}

#[test]
fn structured_provider_environment_resolves_selected_aliases() {
    let mut config = resolver_config("opencode");
    config.agent.provider = Some(mapped_provider("opencode-go", None));
    config.agent.providers = Some(AgentProvidersConfig {
        active: vec!["opencode-go".to_owned(), "openrouter".to_owned()],
        selected_aliases: BTreeMap::from([("opencode-go".to_owned(), "go_2".to_owned())]),
    });
    let (_home, store) = catalog_store(BTreeMap::from([
        (
            "opencode-go".to_owned(),
            ProviderCredentialSet::promoted(BTreeMap::from([(
                "go_2".to_owned(),
                credential("OPENCODE_API_KEY", "go-key"),
            )])),
        ),
        (
            "openrouter".to_owned(),
            ProviderCredentialSet::aliasless(credential("OPENROUTER_API_KEY", "router-key")),
        ),
    ]));

    let resolved = resolve_agent_environment(&config, &store).expect("resolve");

    assert_eq!(resolved.env["OPENCODE_API_KEY"], "go-key");
    assert_eq!(resolved.env["OPENROUTER_API_KEY"], "router-key");
    assert_eq!(resolved.providers.len(), 2);
    assert_eq!(resolved.providers[0].alias.as_deref(), Some("go_2"));
    assert!(
        resolved
            .providers
            .iter()
            .all(|provider| provider.revision.is_some())
    );
}

#[test]
fn templated_agent_env_resolves_var_name_and_composed_value() {
    let mut config = resolver_config("opencode");
    config.agent.env = vec![
        "PLAIN_TOKEN".to_owned(),
        "AUTH_HEADER=Bearer ${RELAY_TOKEN}".to_owned(),
    ];
    let home = tempfile::tempdir().expect("home");
    let mut store = SecretStore::open_or_create(home.path()).expect("secret store");
    store
        .set_many([("PLAIN_TOKEN", "plain"), ("RELAY_TOKEN", "tok-1")])
        .expect("set secrets");

    let resolved = resolve_agent_environment(&config, &store).expect("resolve");

    assert_eq!(resolved.env["PLAIN_TOKEN"], "plain");
    assert_eq!(resolved.env["AUTH_HEADER"], "Bearer tok-1");
    assert!(!resolved.env.contains_key("RELAY_TOKEN"));
}

#[test]
fn shared_env_deduplicates_equal_values_and_rejects_different_values() {
    let mut config = resolver_config("opencode");
    config.agent.provider = Some(mapped_provider("opencode", None));
    config.agent.providers = Some(AgentProvidersConfig {
        active: vec!["opencode".to_owned(), "opencode-go".to_owned()],
        selected_aliases: BTreeMap::new(),
    });
    let (_home, store) = catalog_store(BTreeMap::from([
        (
            "opencode".to_owned(),
            ProviderCredentialSet::aliasless(credential("OPENCODE_API_KEY", "shared")),
        ),
        (
            "opencode-go".to_owned(),
            ProviderCredentialSet::aliasless(credential("OPENCODE_API_KEY", "shared")),
        ),
    ]));
    let resolved = resolve_agent_environment(&config, &store).expect("equal values resolve");
    assert_eq!(resolved.env.len(), 1);
    assert_eq!(resolved.providers.len(), 2);

    let (_home, store) = catalog_store(BTreeMap::from([
        (
            "opencode".to_owned(),
            ProviderCredentialSet::aliasless(credential("OPENCODE_API_KEY", "first")),
        ),
        (
            "opencode-go".to_owned(),
            ProviderCredentialSet::aliasless(credential("OPENCODE_API_KEY", "second")),
        ),
    ]));
    let error = resolve_agent_environment(&config, &store).expect_err("collision");
    let message = error.to_string();
    assert!(message.contains("opencode"));
    assert!(message.contains("opencode-go"));
    assert!(message.contains("OPENCODE_API_KEY"));
    assert!(!message.contains("first"));
    assert!(!message.contains("second"));
}

#[test]
fn legacy_flat_ref_remains_the_implicit_single_provider() {
    let mut config = resolver_config("opencode");
    config.agent.env.push("LEGACY_GO_KEY".to_owned());
    config.agent.provider = Some(mapped_provider("opencode-go", Some("LEGACY_GO_KEY")));
    let home = tempfile::tempdir().expect("home");
    let mut store = SecretStore::open_or_create(home.path()).expect("secret store");
    store.set("LEGACY_GO_KEY", "legacy").expect("legacy secret");

    let resolved = resolve_agent_environment(&config, &store).expect("resolve");

    assert_eq!(resolved.env["LEGACY_GO_KEY"], "legacy");
    assert_eq!(resolved.providers.len(), 1);
    assert_eq!(resolved.providers[0].provider_id, "opencode-go");
    assert!(resolved.providers[0].revision.is_none());

    config.agent.env.clear();
    let error = resolve_agent_environment(&config, &store).expect_err("missing legacy ref");
    assert!(error.to_string().contains("LEGACY_GO_KEY"));
}

#[test]
fn secretless_resolution_skips_store_only_for_empty_or_native_auth_envs() {
    let config = resolver_config("amp");
    let resolved = resolve_agent_environment_without_secrets(&config).expect("empty environment");
    assert!(resolved.env.is_empty());
    assert!(resolved.providers.is_empty());

    let mut config = resolver_config("claude-code");
    config.agent.provider = Some(mapped_provider("amazon-bedrock", None));
    let resolved = resolve_agent_environment_without_secrets(&config).expect("native auth");
    assert_eq!(resolved.providers[0].provider_id, "amazon-bedrock");

    // Codex reads `OPENAI_API_KEY` itself, so its OpenAI lane needs the store
    // like any other keyed provider.
    let mut config = resolver_config("codex");
    config.agent.provider = Some(mapped_provider("openai", None));
    assert!(resolve_agent_environment_without_secrets(&config).is_none());

    let mut config = resolver_config("opencode");
    config.agent.provider = Some(mapped_provider("opencode-go", None));
    assert!(resolve_agent_environment_without_secrets(&config).is_none());
}

#[test]
fn native_auth_snapshot_reports_injected_profile_environment() {
    let mut config = resolver_config("claude-code");
    config.agent.provider = Some(mapped_provider("google-vertex-anthropic", None));
    config.agent.env = vec![
        "ANTHROPIC_VERTEX_PROJECT_ID".to_owned(),
        "CLOUD_ML_REGION".to_owned(),
    ];
    let home = tempfile::tempdir().expect("home");
    let mut store = SecretStore::open_or_create(home.path()).expect("secret store");
    store
        .set_many([
            ("ANTHROPIC_VERTEX_PROJECT_ID", "project"),
            ("CLOUD_ML_REGION", "region"),
        ])
        .expect("profile secrets");

    let resolved = resolve_agent_environment(&config, &store).expect("resolve native auth");

    assert_eq!(
        resolved.providers[0].env_names,
        ["ANTHROPIC_VERTEX_PROJECT_ID", "CLOUD_ML_REGION"]
    );
}

#[test]
fn subagent_provider_structured_key_resolves_without_active_block() {
    // Guards the subagent-discovery-auth fix: once the subagent provider is
    // registered (which `configure_mapped_subagent` now does before model
    // discovery), its structured credential resolves into the probe env
    // even with no `[agent.providers]` active block.
    use crate::config::AgentSubagentConfig;
    let mut config = resolver_config("opencode");
    config.agent.provider = Some(mapped_provider("openai", None));
    config.agent.subagent = Some(AgentSubagentConfig {
        disabled: false,
        provider: Some(mapped_provider("opencode-go", None)),
    });
    let (_home, store) = catalog_store(BTreeMap::from([
        (
            "openai".to_owned(),
            ProviderCredentialSet::aliasless(credential("OPENAI_API_KEY", "openai-key")),
        ),
        (
            "opencode-go".to_owned(),
            ProviderCredentialSet::aliasless(credential("OPENCODE_API_KEY", "go-key")),
        ),
    ]));

    let resolved = resolve_agent_environment(&config, &store).expect("resolve");

    assert_eq!(resolved.env["OPENCODE_API_KEY"], "go-key");
    assert_eq!(resolved.env["OPENAI_API_KEY"], "openai-key");
    assert!(
        resolved
            .providers
            .iter()
            .any(|provider| provider.provider_id == "opencode-go")
    );
}

#[test]
fn custom_provider_appears_in_resolved_snapshot() {
    let mut config = resolver_config("opencode");
    config.agent.env = vec!["CUSTOM_KEY".to_owned()];
    config.agent.provider = Some(custom_provider("my-custom", "CUSTOM_KEY"));
    let home = tempfile::tempdir().expect("home");
    let mut store = SecretStore::open_or_create(home.path()).expect("secret store");
    store.set("CUSTOM_KEY", "custom-secret").expect("flat key");

    let resolved = resolve_agent_environment(&config, &store).expect("resolve");

    let snapshot = resolved
        .providers
        .iter()
        .find(|provider| provider.provider_id == "my-custom")
        .expect("custom provider snapshot present");
    assert_eq!(snapshot.env_names, vec!["CUSTOM_KEY".to_owned()]);
    assert!(snapshot.revision.is_none());
    assert!(snapshot.alias.is_none());
}

fn custom_provider(provider_id: &str, api_key_ref: &str) -> AgentProviderConfig {
    use crate::config::{AgentCustomProviderConfig, CustomProviderApi};
    AgentProviderConfig {
        id: provider_id.to_owned(),
        model: None,
        api_key_ref: Some(api_key_ref.to_owned()),
        custom: Some(AgentCustomProviderConfig {
            name: "My Custom".to_owned(),
            base_url: "https://example.test/v1".to_owned(),
            api: CustomProviderApi::default(),
            model_name: None,
            context: 128_000,
            output_max_tokens: 8_192,
        }),
    }
}

#[test]
fn satisfiability_predicate_mirrors_catalog_injection() {
    // opencode emits CLOUDFLARE_API_TOKEN while the canonical catalog key
    // is CLOUDFLARE_API_KEY; the emitted name must satisfy off the
    // canonical value, exactly as resolve injects it.
    let mut config = resolver_config("opencode");
    config.agent.provider = Some(mapped_provider("cloudflare-ai-gateway", None));
    let (_home, store) = catalog_store(BTreeMap::from([(
        "cloudflare-ai-gateway".to_owned(),
        ProviderCredentialSet::aliasless(credential("CLOUDFLARE_API_KEY", "cf-key")),
    )]));
    let emitted =
        env_var_for_agent_provider_id("opencode", "cloudflare-ai-gateway").expect("emitted var");
    assert_eq!(emitted, "CLOUDFLARE_API_TOKEN");
    assert!(env_ref_is_satisfiable_for_config(
        &config,
        &store,
        "cloudflare-ai-gateway",
        emitted
    ));
    // Companion refs are keyed by their own names and absent here.
    assert!(!env_ref_is_satisfiable_for_config(
        &config,
        &store,
        "cloudflare-ai-gateway",
        "CLOUDFLARE_ACCOUNT_ID"
    ));
    assert!(!env_ref_is_satisfiable_for_config(
        &config,
        &store,
        "cloudflare-ai-gateway",
        "UNRELATED_KEY"
    ));

    // A promoted set with no selected alias errors at resolve time, so it
    // must not satisfy the gate.
    let (_home, store) = catalog_store(BTreeMap::from([(
        "opencode-go".to_owned(),
        ProviderCredentialSet::promoted(BTreeMap::from([(
            "go_1".to_owned(),
            credential("OPENCODE_API_KEY", "go-key"),
        )])),
    )]));
    let mut config = resolver_config("opencode");
    config.agent.provider = Some(mapped_provider("opencode-go", None));
    assert!(!env_ref_is_satisfiable_for_config(
        &config,
        &store,
        "opencode-go",
        "OPENCODE_API_KEY"
    ));
}

#[test]
fn satisfiability_predicate_matches_custom_provider_api_key_ref_only() {
    let mut config = resolver_config("opencode");
    config.agent.provider = Some(custom_provider("my-custom", "CUSTOM_KEY"));
    let (_home, store) = catalog_store(BTreeMap::from([(
        "my-custom".to_owned(),
        ProviderCredentialSet::aliasless(credential("CUSTOM_KEY", "custom-secret")),
    )]));
    assert!(env_ref_is_satisfiable_for_config(
        &config,
        &store,
        "my-custom",
        "CUSTOM_KEY"
    ));
    assert!(!env_ref_is_satisfiable_for_config(
        &config,
        &store,
        "my-custom",
        "OTHER_KEY"
    ));

    // Credential keyed by a name other than the configured ref does not
    // satisfy — resolve would not inject it.
    let (_home, store) = catalog_store(BTreeMap::from([(
        "my-custom".to_owned(),
        ProviderCredentialSet::aliasless(credential("OTHER_KEY", "custom-secret")),
    )]));
    assert!(!env_ref_is_satisfiable_for_config(
        &config,
        &store,
        "my-custom",
        "CUSTOM_KEY"
    ));
}

#[test]
fn custom_provider_resolves_catalog_credential_with_revision() {
    let mut config = resolver_config("opencode");
    config.agent.env = vec!["CUSTOM_KEY".to_owned()];
    config.agent.provider = Some(custom_provider("my-custom", "CUSTOM_KEY"));
    let (_home, store) = catalog_store(BTreeMap::from([(
        "my-custom".to_owned(),
        ProviderCredentialSet::aliasless(credential("CUSTOM_KEY", "catalog-secret")),
    )]));

    let resolved = resolve_agent_environment(&config, &store).expect("resolve");

    assert_eq!(resolved.env["CUSTOM_KEY"], "catalog-secret");
    let snapshot = resolved
        .providers
        .iter()
        .find(|provider| provider.provider_id == "my-custom")
        .expect("custom provider snapshot present");
    assert_eq!(snapshot.env_names, vec!["CUSTOM_KEY".to_owned()]);
    assert!(snapshot.revision.is_some());
}

#[test]
fn catalog_credential_shadows_bare_agent_env_ref() {
    // Regression: a catalog-only credential with the ref still declared in
    // `agent.env` (as init writes it) must not fail on the flat store.
    let mut config = resolver_config("opencode");
    config.agent.env = vec!["OPENCODE_API_KEY".to_owned()];
    config.agent.provider = Some(mapped_provider("opencode-go", None));
    let (_home, store) = catalog_store(BTreeMap::from([(
        "opencode-go".to_owned(),
        ProviderCredentialSet::aliasless(credential("OPENCODE_API_KEY", "catalog-key")),
    )]));
    let resolved = resolve_agent_environment(&config, &store).expect("resolve");
    assert_eq!(resolved.env["OPENCODE_API_KEY"], "catalog-key");

    // Precedence: with a differing flat secret present, the catalog value
    // wins and there is no owner conflict.
    let (_home, mut store) = catalog_store(BTreeMap::from([(
        "opencode-go".to_owned(),
        ProviderCredentialSet::aliasless(credential("OPENCODE_API_KEY", "rotated-key")),
    )]));
    store
        .set("OPENCODE_API_KEY", "stale-flat-key")
        .expect("flat");
    let resolved = resolve_agent_environment(&config, &store).expect("resolve");
    assert_eq!(resolved.env["OPENCODE_API_KEY"], "rotated-key");
}

#[test]
fn templated_agent_env_keeps_flat_semantics_when_catalog_covers_var() {
    let mut config = resolver_config("opencode");
    config.agent.env = vec!["OPENCODE_API_KEY=prefix-${RELAY_TOKEN}".to_owned()];
    config.agent.provider = Some(mapped_provider("opencode-go", None));
    let (_home, mut store) = catalog_store(BTreeMap::from([(
        "opencode-go".to_owned(),
        ProviderCredentialSet::aliasless(credential("OPENCODE_API_KEY", "prefix-tok-1")),
    )]));
    store.set("RELAY_TOKEN", "tok-1").expect("flat");
    let resolved = resolve_agent_environment(&config, &store).expect("resolve");
    // The template composed the same value; a differing template value
    // would surface as an owner conflict, which is the intended guard.
    assert_eq!(resolved.env["OPENCODE_API_KEY"], "prefix-tok-1");
}

#[test]
fn custom_provider_without_any_credential_reports_pending_error() {
    let mut config = resolver_config("opencode");
    config.agent.env = vec!["CUSTOM_KEY".to_owned()];
    config.agent.provider = Some(custom_provider("my-custom", "CUSTOM_KEY"));
    let home = tempfile::tempdir().expect("home");
    let store = SecretStore::open_or_create(home.path()).expect("secret store");

    let error = resolve_agent_environment(&config, &store).expect_err("pending");
    let message = error.to_string();
    assert!(message.contains("my-custom"));
    assert!(message.contains("CUSTOM_KEY"));
    assert!(message.contains("managed"));
}
