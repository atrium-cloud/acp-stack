use std::collections::BTreeMap;

use crate::config::{AgentProviderConfig, AgentProvidersConfig, Config, load_config_from_str};
use crate::runtime::agent::provider_keys::apply_catalog_mapped_agent_provider;
use crate::secrets::{ProviderCredential, ProviderCredentialSet, SecretStore};

use super::provider_credentials::collect_credential;
use super::provider_migration::{
    migrate_legacy_provider_credentials, persist_migrated_catalog_then_config,
    prune_migrated_flat_secrets_with_candidates, replace_catalog_then_config,
};
use super::provider_shared::ensure_credential_value_is_new;

fn provider_config() -> Config {
    load_config_from_str(
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
id = "opencode"
name = "OpenCode"
command = "opencode"
args = ["acp"]
cwd = "/workspace"
env = ["GO_KEY_1"]
restart = "on-crash"

[agent.provider]
id = "opencode-go"
api_key_ref = "GO_KEY_1"
"#,
    )
    .expect("config")
}

fn migrated_credential(source_ref: &str, value: &str) -> ProviderCredential {
    let mut credential = ProviderCredential::new(
        BTreeMap::from([("OPENCODE_API_KEY".to_owned(), value.to_owned())]),
        BTreeMap::from([("OPENCODE_API_KEY".to_owned(), source_ref.to_owned())]),
    );
    credential.migrated = true;
    credential
}

#[test]
fn source_ref_credentials_copy_values_and_retain_ref_names() {
    let home = tempfile::tempdir().expect("home");
    let mut store = SecretStore::open_or_create(home.path()).expect("secret store");
    store.set("GO_SOURCE", "private-go-key").expect("source");

    let credential = collect_credential(
        "opencode-go",
        &["OPENCODE_API_KEY=GO_SOURCE".to_owned()],
        &store,
    )
    .expect("credential");

    assert_eq!(credential.values["OPENCODE_API_KEY"], "private-go-key");
    assert_eq!(credential.source_refs["OPENCODE_API_KEY"], "GO_SOURCE");
}

#[test]
fn provider_use_replaces_active_set_for_single_provider_harnesses() {
    let mut agent = provider_config().agent;
    agent.providers = Some(AgentProvidersConfig {
        active: vec!["openrouter".to_owned()],
        selected_aliases: BTreeMap::from([
            ("openrouter".to_owned(), "backup".to_owned()),
            ("openai".to_owned(), "primary".to_owned()),
        ]),
    });

    apply_catalog_mapped_agent_provider(&mut agent, "openai", false).expect("provider");

    let providers = agent.providers.expect("provider settings");
    assert_eq!(providers.active, ["openai"]);
    assert_eq!(
        providers.selected_aliases,
        BTreeMap::from([
            ("openrouter".to_owned(), "backup".to_owned()),
            ("openai".to_owned(), "primary".to_owned()),
        ])
    );
}

#[test]
fn provider_use_appends_active_set_for_multi_provider_harnesses() {
    let mut agent = provider_config().agent;
    agent.providers = Some(AgentProvidersConfig {
        active: vec!["opencode-go".to_owned()],
        selected_aliases: BTreeMap::new(),
    });

    apply_catalog_mapped_agent_provider(&mut agent, "openrouter", true).expect("provider");

    assert_eq!(
        agent.providers.expect("provider settings").active,
        ["opencode-go", "openrouter"]
    );
}

#[test]
fn sole_legacy_ref_lazily_migrates_to_aliasless_catalog_entry() {
    let mut config = provider_config();
    let home = tempfile::tempdir().expect("home");
    let mut store = SecretStore::open_or_create(home.path()).expect("secret store");
    store.set("GO_KEY_1", "first").expect("legacy value");

    let migration = migrate_legacy_provider_credentials(&mut config, &mut store).expect("migrate");

    let set = store
        .provider_credential_set("opencode-go")
        .expect("catalog entry");
    assert!(!set.is_promoted());
    assert_eq!(
        set.sole.as_ref().expect("sole").values["OPENCODE_API_KEY"],
        "first"
    );
    let primary = config
        .array
        .target(&config.array.primary_target)
        .expect("primary");
    assert!(
        primary
            .agent
            .provider
            .as_ref()
            .expect("provider")
            .api_key_ref
            .is_none()
    );
    assert!(primary.agent.providers.is_none());
    assert!(!primary.agent.env.iter().any(|name| name == "GO_KEY_1"));
    prune_migrated_flat_secrets_with_candidates(&config, &mut store, &migration.cleanup_candidates)
        .expect("prune");
    assert!(!store.contains("GO_KEY_1"));
}

#[test]
fn existing_promoted_legacy_refs_preserve_each_target_selection() {
    let mut config = provider_config();
    let mut second = config.array.targets[0].clone();
    second.id = "worker".to_owned();
    second.agent.env = vec!["GO_KEY_2".to_owned()];
    second.agent.provider = Some(AgentProviderConfig {
        id: "opencode-go".to_owned(),
        model: None,
        api_key_ref: Some("GO_KEY_2".to_owned()),
        custom: None,
    });
    config.array.targets.push(second);
    let home = tempfile::tempdir().expect("home");
    let mut store = SecretStore::open_or_create(home.path()).expect("secret store");
    store
        .set_many([("GO_KEY_1", "first"), ("GO_KEY_2", "second")])
        .expect("legacy values");
    store
        .replace_provider_credentials(
            BTreeMap::from([(
                "opencode-go".to_owned(),
                ProviderCredentialSet::promoted(BTreeMap::from([
                    ("go_1".to_owned(), migrated_credential("GO_KEY_1", "first")),
                    ("go_2".to_owned(), migrated_credential("GO_KEY_2", "second")),
                ])),
            )]),
            &[],
        )
        .expect("catalog");

    let migration = migrate_legacy_provider_credentials(&mut config, &mut store).expect("migrate");

    let primary = config
        .array
        .target(&config.array.primary_target)
        .expect("primary");
    assert!(
        primary
            .agent
            .provider
            .as_ref()
            .expect("provider")
            .api_key_ref
            .is_none()
    );
    assert_eq!(
        primary
            .agent
            .providers
            .as_ref()
            .and_then(|providers| providers.selected_aliases.get("opencode-go"))
            .map(String::as_str),
        Some("go_1")
    );
    let worker = config.array.target("worker").expect("worker");
    assert_eq!(
        worker
            .agent
            .providers
            .as_ref()
            .and_then(|providers| providers.selected_aliases.get("opencode-go"))
            .map(String::as_str),
        Some("go_2")
    );
    prune_migrated_flat_secrets_with_candidates(&config, &mut store, &migration.cleanup_candidates)
        .expect("prune");
    assert!(!store.contains("GO_KEY_1"));
    assert!(!store.contains("GO_KEY_2"));
}

#[test]
fn existing_aliasless_catalog_matches_equal_legacy_value_without_switching() {
    let mut config = provider_config();
    let home = tempfile::tempdir().expect("home");
    let mut store = SecretStore::open_or_create(home.path()).expect("secret store");
    store.set("GO_KEY_1", "same").expect("legacy value");
    let existing = ProviderCredentialSet::aliasless(ProviderCredential::new(
        BTreeMap::from([("OPENCODE_API_KEY".to_owned(), "same".to_owned())]),
        BTreeMap::new(),
    ));
    store
        .replace_provider_credentials(BTreeMap::from([("opencode-go".to_owned(), existing)]), &[])
        .expect("catalog");

    let migration = migrate_legacy_provider_credentials(&mut config, &mut store).expect("migrate");

    let credentials = store
        .provider_credential_set("opencode-go")
        .expect("catalog entry");
    assert!(!credentials.is_promoted());
    assert_eq!(
        credentials.sole.as_ref().expect("sole").values["OPENCODE_API_KEY"],
        "same"
    );
    assert!(
        config
            .agent
            .provider
            .as_ref()
            .expect("provider")
            .api_key_ref
            .is_none()
    );
    prune_migrated_flat_secrets_with_candidates(&config, &mut store, &migration.cleanup_candidates)
        .expect("prune");
    assert!(!store.contains("GO_KEY_1"));
}

#[test]
fn unmatched_legacy_ref_does_not_clear_config_without_alias_assignment() {
    let mut config = provider_config();
    let home = tempfile::tempdir().expect("home");
    let mut store = SecretStore::open_or_create(home.path()).expect("secret store");
    store.set("GO_KEY_1", "new").expect("legacy value");
    let existing = BTreeMap::from([(
        "opencode-go".to_owned(),
        ProviderCredentialSet::promoted(BTreeMap::from([(
            "existing".to_owned(),
            migrated_credential("OTHER_KEY", "old"),
        )])),
    )]);
    store
        .replace_provider_credentials(existing.clone(), &[])
        .expect("catalog");

    let error = migrate_legacy_provider_credentials(&mut config, &mut store)
        .expect_err("non-interactive migration requires alias");

    assert!(error.to_string().contains("run interactively"));
    assert_eq!(store.provider_credentials(), &existing);
    assert_eq!(
        config
            .agent
            .provider
            .as_ref()
            .and_then(|provider| provider.api_key_ref.as_deref()),
        Some("GO_KEY_1")
    );
}

#[test]
fn rotated_legacy_source_ref_does_not_match_stale_catalog_value() {
    let mut config = provider_config();
    let home = tempfile::tempdir().expect("home");
    let mut store = SecretStore::open_or_create(home.path()).expect("secret store");
    store.set("GO_KEY_1", "rotated").expect("legacy value");
    let existing = BTreeMap::from([(
        "opencode-go".to_owned(),
        ProviderCredentialSet::aliasless(migrated_credential("GO_KEY_1", "stale")),
    )]);
    store
        .replace_provider_credentials(existing.clone(), &[])
        .expect("catalog");

    let error = migrate_legacy_provider_credentials(&mut config, &mut store)
        .expect_err("rotated ref requires alias assignment");

    assert!(error.to_string().contains("run interactively"));
    assert_eq!(store.provider_credentials(), &existing);
    assert_eq!(
        config
            .agent
            .provider
            .as_ref()
            .and_then(|provider| provider.api_key_ref.as_deref()),
        Some("GO_KEY_1")
    );
}

#[test]
fn scripted_credentials_reject_empty_required_secret_values() {
    let home = tempfile::tempdir().expect("home");
    let mut store = SecretStore::open_or_create(home.path()).expect("secret store");
    store.set("EMPTY_KEY", "").expect("empty source");

    let error = collect_credential(
        "opencode-go",
        &["OPENCODE_API_KEY=EMPTY_KEY".to_owned()],
        &store,
    )
    .expect_err("empty required value");

    assert!(
        error
            .to_string()
            .contains("missing or empty required field")
    );
}

#[test]
fn rotated_migrated_credential_still_prunes_legacy_flat_secret() {
    let mut config = provider_config();
    let home = tempfile::tempdir().expect("home");
    let mut store = SecretStore::open_or_create(home.path()).expect("secret store");
    store
        .set_many([("GO_KEY_1", "old"), ("NEW_KEY", "new")])
        .expect("flat values");
    let migration = migrate_legacy_provider_credentials(&mut config, &mut store).expect("migrate");
    let mut catalog = store.provider_credentials().clone();
    catalog
        .get_mut("opencode-go")
        .and_then(|credentials| credentials.sole.as_mut())
        .expect("sole")
        .rotate(
            BTreeMap::from([("OPENCODE_API_KEY".to_owned(), "new".to_owned())]),
            BTreeMap::from([("OPENCODE_API_KEY".to_owned(), "NEW_KEY".to_owned())]),
        );
    store
        .replace_provider_credentials(catalog, &[])
        .expect("rotate catalog");

    prune_migrated_flat_secrets_with_candidates(&config, &mut store, &migration.cleanup_candidates)
        .expect("prune");

    assert!(!store.contains("GO_KEY_1"));
    assert!(store.contains("NEW_KEY"));
}

#[test]
fn deleted_migrated_credential_still_prunes_legacy_flat_secret() {
    let mut config = provider_config();
    let home = tempfile::tempdir().expect("home");
    let mut store = SecretStore::open_or_create(home.path()).expect("secret store");
    store.set("GO_KEY_1", "old").expect("flat value");
    let migration = migrate_legacy_provider_credentials(&mut config, &mut store).expect("migrate");
    store
        .replace_provider_credentials(BTreeMap::new(), &[])
        .expect("delete catalog");

    prune_migrated_flat_secrets_with_candidates(&config, &mut store, &migration.cleanup_candidates)
        .expect("prune");

    assert!(!store.contains("GO_KEY_1"));
}

#[test]
fn promoted_sets_remain_promoted_after_deleting_to_one_alias() {
    let mut set = ProviderCredentialSet::promoted(BTreeMap::from([
        ("go_1".to_owned(), migrated_credential("GO_KEY_1", "first")),
        ("go_2".to_owned(), migrated_credential("GO_KEY_2", "second")),
    ]));

    set.aliases.remove("go_2");

    assert!(set.is_promoted());
    assert!(set.sole.is_none());
    assert_eq!(set.aliases.len(), 1);
}

#[test]
fn duplicate_primary_values_are_rejected_without_exposing_value() {
    let existing = ProviderCredentialSet::aliasless(migrated_credential("GO_KEY_1", "same"));
    let candidate = migrated_credential("GO_KEY_2", "same");

    let error = ensure_credential_value_is_new("opencode-go", &existing, &candidate)
        .expect_err("duplicate");

    assert!(!error.to_string().contains("same"));
}

#[test]
fn catalog_change_rolls_back_when_config_write_fails() {
    let home = tempfile::tempdir().expect("home");
    let mut store = SecretStore::open_or_create(home.path()).expect("secret store");
    store.set("GO_KEY_1", "first").expect("flat key");
    let config = provider_config();
    let previous = BTreeMap::from([(
        "opencode-go".to_owned(),
        ProviderCredentialSet::aliasless(migrated_credential("GO_KEY_1", "first")),
    )]);
    store
        .replace_provider_credentials(previous.clone(), &[])
        .expect("initial catalog");
    let replacement = BTreeMap::from([(
        "opencode-go".to_owned(),
        ProviderCredentialSet::aliasless(migrated_credential("GO_KEY_2", "second")),
    )]);

    replace_catalog_then_config(
        &mut store,
        replacement,
        previous.clone(),
        &config,
        home.path(),
        &home.path().join("missing").join("config.toml"),
        "invalid = false\n",
    )
    .expect_err("config write fails");

    assert_eq!(store.provider_credentials(), &previous);
    let reopened = SecretStore::open(home.path()).expect("reopen");
    assert_eq!(reopened.provider_credentials(), &previous);
}

#[test]
fn staged_migration_rolls_back_when_config_write_fails() {
    let home = tempfile::tempdir().expect("home");
    let mut store = SecretStore::open_or_create(home.path()).expect("secret store");
    let previous = BTreeMap::from([(
        "opencode-go".to_owned(),
        ProviderCredentialSet::aliasless(migrated_credential("GO_KEY_1", "first")),
    )]);
    store
        .replace_provider_credentials(previous.clone(), &[])
        .expect("initial catalog");
    let staged = BTreeMap::from([(
        "opencode-go".to_owned(),
        ProviderCredentialSet::aliasless(migrated_credential("GO_KEY_2", "second")),
    )]);
    store
        .stage_provider_credentials(staged)
        .expect("stage migration");

    persist_migrated_catalog_then_config(
        &mut store,
        previous.clone(),
        true,
        &home.path().join("missing").join("config.toml"),
        "invalid = false\n",
    )
    .expect_err("config write fails");

    assert_eq!(store.provider_credentials(), &previous);
    let reopened = SecretStore::open(home.path()).expect("reopen");
    assert_eq!(reopened.provider_credentials(), &previous);
}

#[test]
fn daemon_state_reads_non_array_payload_directly() {
    let data = serde_json::json!({
        "loaded_providers": [{ "provider_id": "openai", "alias": null, "env_names": ["OPENAI_API_KEY"] }],
        "provider_restart_required": true,
    });
    let (loaded, restart) =
        super::provider_target::extract_daemon_provider_state(&data, false, "opencode");
    assert_eq!(loaded.unwrap()[0]["provider_id"], "openai");
    assert_eq!(restart, Some(true));
}

#[test]
fn daemon_state_selects_matching_array_target() {
    let data = serde_json::json!({
        "targets": [
            { "id": "primary", "loaded_providers": [], "provider_restart_required": false },
            { "id": "secondary", "loaded_providers": [{ "provider_id": "openrouter" }], "provider_restart_required": true },
        ],
    });
    let (loaded, restart) =
        super::provider_target::extract_daemon_provider_state(&data, true, "secondary");
    assert_eq!(loaded.unwrap()[0]["provider_id"], "openrouter");
    assert_eq!(restart, Some(true));
}

#[test]
fn daemon_state_missing_array_target_yields_none() {
    let data = serde_json::json!({
        "targets": [{ "id": "primary", "loaded_providers": [], "provider_restart_required": false }],
    });
    let (loaded, restart) =
        super::provider_target::extract_daemon_provider_state(&data, true, "absent");
    assert!(loaded.is_none());
    assert_eq!(restart, None);
}
