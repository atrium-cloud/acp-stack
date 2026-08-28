use crate::common::cli::*;
use acp_stack::secrets::SecretStore;
use predicates::prelude::PredicateBooleanExt as _;
use std::collections::BTreeMap;
use std::fs;

fn write_opencode_config(config_dir: &std::path::Path) {
    fs::create_dir_all(config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");
}

fn seed_named_secret(home: &std::path::Path, name: &str, value: &str) {
    let mut store = SecretStore::open_or_create(home).expect("secret store should open");
    store.set(name, value).expect("secret should be stored");
}

#[test]
fn agent_provider_credential_add_and_list_never_expose_values() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    write_opencode_config(&config_dir);
    seed_named_secret(tempdir.path(), "OPENCODE_API_KEY", "opencode-agent-key");
    seed_named_secret(
        tempdir.path(),
        "OPENAI_SOURCE",
        "sk-super-secret-openai-value",
    );

    acps_command(tempdir.path())
        .args([
            "agent",
            "provider",
            "credential",
            "add",
            "openai",
            "--from-secret",
            "OPENAI_API_KEY=OPENAI_SOURCE",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("provider credential: added"));

    // Human list surfaces env names but never the credential value.
    acps_command(tempdir.path())
        .args(["agent", "provider", "credential", "list", "openai"])
        .assert()
        .success()
        .stdout(predicates::str::contains("OPENAI_API_KEY"))
        .stdout(predicates::str::contains("sk-super-secret-openai-value").not());

    // JSON list exposes source-ref names but neither values nor revisions.
    acps_command(tempdir.path())
        .args([
            "agent",
            "provider",
            "credential",
            "list",
            "openai",
            "--format",
            "json",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("OPENAI_SOURCE"))
        .stdout(predicates::str::contains("sk-super-secret-openai-value").not())
        .stdout(predicates::str::contains("revision").not());
}

#[test]
fn agent_provider_credential_select_blocks_deleting_selected_alias() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    write_opencode_config(&config_dir);
    seed_named_secret(tempdir.path(), "OPENCODE_API_KEY", "opencode-agent-key");
    seed_provider_credential(tempdir.path(), "openrouter", &["OPENROUTER_API_KEY"]);
    seed_named_secret(tempdir.path(), "OR_BACKUP_SOURCE", "or-backup-secret");

    acps_command(tempdir.path())
        .args(["agent", "provider", "use", "openrouter"])
        .assert()
        .success();
    acps_command(tempdir.path())
        .args([
            "agent",
            "provider",
            "credential",
            "add",
            "openrouter",
            "--existing-alias",
            "primary",
            "--alias",
            "backup",
            "--from-secret",
            "OPENROUTER_API_KEY=OR_BACKUP_SOURCE",
        ])
        .assert()
        .success();

    acps_command(tempdir.path())
        .args([
            "agent",
            "provider",
            "credential",
            "select",
            "openrouter",
            "backup",
        ])
        .assert()
        .success();

    // The selected alias cannot be deleted while a target still points at it.
    acps_command(tempdir.path())
        .args([
            "agent",
            "provider",
            "credential",
            "delete",
            "openrouter",
            "backup",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("selected by target"));
}

#[test]
fn agent_provider_set_active_prunes_selected_alias_for_dropped_provider() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    write_opencode_config(&config_dir);
    seed_named_secret(tempdir.path(), "OPENCODE_API_KEY", "opencode-agent-key");
    seed_provider_credential(tempdir.path(), "openai", &["OPENAI_API_KEY"]);
    seed_provider_credential(tempdir.path(), "openrouter", &["OPENROUTER_API_KEY"]);
    seed_named_secret(tempdir.path(), "OR_BACKUP_SOURCE", "or-backup-secret");

    acps_command(tempdir.path())
        .args(["agent", "provider", "use", "openai"])
        .assert()
        .success();
    acps_command(tempdir.path())
        .args(["agent", "provider", "set-active", "openai,openrouter"])
        .assert()
        .success();
    acps_command(tempdir.path())
        .args([
            "agent",
            "provider",
            "credential",
            "add",
            "openrouter",
            "--existing-alias",
            "primary",
            "--alias",
            "backup",
            "--from-secret",
            "OPENROUTER_API_KEY=OR_BACKUP_SOURCE",
        ])
        .assert()
        .success();

    // Dropping openrouter from the active set must prune its stale alias.
    acps_command(tempdir.path())
        .args(["agent", "provider", "set-active", "openai"])
        .assert()
        .success();

    let config = fs::read_to_string(config_dir.join("acps-config.toml"))
        .expect("updated config should be readable");
    assert!(!config.contains("openrouter"));

    acps_command(tempdir.path())
        .args([
            "agent",
            "provider",
            "credential",
            "delete",
            "openrouter",
            "primary",
        ])
        .assert()
        .success();
}

#[test]
fn agent_provider_list_active_reports_configured_state_offline() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    write_opencode_config(&config_dir);
    seed_named_secret(tempdir.path(), "OPENCODE_API_KEY", "opencode-agent-key");
    seed_provider_credential(tempdir.path(), "openai", &["OPENAI_API_KEY"]);

    acps_command(tempdir.path())
        .args(["agent", "provider", "use", "openai"])
        .assert()
        .success();

    acps_command(tempdir.path())
        .args(["agent", "provider", "list-active"])
        .assert()
        .success()
        .stdout(predicates::str::contains("configured: provider=openai"))
        .stdout(predicates::str::contains("loaded: unknown"));
}

#[test]
fn agent_set_provider_for_mapped_provider_redirects_to_provider_use() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    write_opencode_config(&config_dir);
    seed_named_secret(tempdir.path(), "OPENCODE_API_KEY", "opencode-agent-key");

    acps_command(tempdir.path())
        .args(["agent", "set", "--provider", "openai"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("acps agent provider use"));
}

#[test]
fn extensions_status_reports_none_declared() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    acps_command(tempdir.path())
        .args(["extensions", "status"])
        .assert()
        .success()
        .stdout(predicates::str::contains("extensions: none declared"));
}

#[test]
fn extensions_status_reports_managed_state_watermark_without_values() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config_text = format!(
        "{VALID_CONFIG}\n\
         [extensions.platform-state]\n\
         type = \"managed-state\"\n\
         capability = \"provider-credential\"\n"
    );
    fs::write(config_dir.join("acps-config.toml"), config_text).expect("config should be written");

    let secret_value = "sk-status-secret";
    {
        let mut store =
            SecretStore::open_or_create(tempdir.path()).expect("secret store should initialize");
        store
            .apply_managed_state_credential(
                "platform-state",
                "provider-credential",
                7,
                Some(acp_stack::secrets::ManagedCredentialSelection {
                    provider_id: "openai".to_owned(),
                    values: BTreeMap::from([(
                        "OPENAI_API_KEY".to_owned(),
                        secret_value.to_owned(),
                    )]),
                    source_refs: BTreeMap::new(),
                    base_url: None,
                }),
            )
            .expect("managed apply should persist");
    }

    acps_command(tempdir.path())
        .args(["extensions", "status"])
        .assert()
        .success()
        .stdout(predicates::str::contains("platform-state: managed-state"))
        .stdout(predicates::str::contains("applied_revision: 7"))
        .stdout(predicates::str::contains("provider: openai"))
        .stdout(predicates::str::contains(secret_value).not());

    acps_command(tempdir.path())
        .args(["extensions", "status", "--format", "json"])
        .assert()
        .success()
        .stdout(predicates::str::contains("\"applied_revision\": 7"))
        .stdout(predicates::str::contains(secret_value).not());
}

#[test]
fn extensions_status_treats_missing_secret_store_as_no_watermark() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config_text = format!(
        "{VALID_CONFIG}\n\
         [extensions.platform-state]\n\
         type = \"managed-state\"\n\
         capability = \"provider-credential\"\n"
    );
    fs::write(config_dir.join("acps-config.toml"), config_text).expect("config should be written");

    acps_command(tempdir.path())
        .args(["extensions", "status"])
        .assert()
        .success()
        .stdout(predicates::str::contains("platform-state: managed-state"))
        .stdout(predicates::str::contains("applied_revision: none"));
}
