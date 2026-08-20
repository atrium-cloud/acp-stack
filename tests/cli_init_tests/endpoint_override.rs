use crate::common::cli::*;
use acp_stack::secrets::{ManagedCredentialSelection, SecretStore};
use std::collections::BTreeMap;
use std::fs;

/// Stage an externally-owned credential carrying an endpoint override, the way
/// a managed-state apply would leave it, so init's agent-apply guard has a
/// live routing decision to protect.
fn stage_endpoint_override(home: &std::path::Path, provider_id: &str) {
    let mut store = SecretStore::open_or_create(home).expect("secret store should open");
    store
        .apply_managed_state_credential(
            "platform-state",
            "provider-credential",
            1,
            Some(ManagedCredentialSelection {
                provider_id: provider_id.to_owned(),
                values: BTreeMap::from([("TEST_API_KEY".to_owned(), "sk-test".to_owned())]),
                source_refs: BTreeMap::new(),
                base_url: Some(format!("http://127.0.0.1:3129/{provider_id}")),
            }),
        )
        .expect("override should be staged");
}

#[test]
fn init_rejects_an_agent_without_an_endpoint_field_while_an_override_is_stored() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_path = tempdir.path().join(".config/acp-stack/acps-config.toml");

    // Lay down a config before the override exists.
    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();
    let config_before = fs::read_to_string(&config_path).expect("config should be readable");

    stage_endpoint_override(tempdir.path(), "openrouter");

    // The placebo agent declares no `set_provider_base_url`, so re-applying it
    // would strand the stored override: rejected, config untouched.
    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "agent `placebo` cannot route a provider through a custom endpoint",
        ));
    let config_after = fs::read_to_string(&config_path).expect("config should be readable");
    assert_eq!(
        config_after, config_before,
        "a rejected init must leave the existing config untouched"
    );
}

#[test]
fn init_reconfirm_of_an_endpoint_capable_agent_succeeds_with_a_stored_override() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    // A placebo-backed registry override entry that DOES declare the endpoint
    // field; the harness points at the same fixture binary the dev registry
    // injects for every embedded agent.
    let placebo_path = env!("CARGO_BIN_EXE_placebo-agent");
    fs::write(
        config_dir.join("agents.toml"),
        format!(
            r#"[[agents]]
id = "placebo-base-url"
name = "Placebo Base URL Agent"
kind = "native"
headless_compatible = true
set_provider_base_url = true
support_doc = "src/bin/placebo_agent/main.rs"

[agents.harness]
id = "{placebo_path}"

[agents.harness.install.shell]
script = "test -x '{placebo_path}'"
creates = "{placebo_path}"
"#
        ),
    )
    .expect("registry override should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo-base-url",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    stage_endpoint_override(tempdir.path(), "openrouter");

    // Re-confirming the same supporting agent keeps the override writable, so
    // the re-run is allowed.
    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo-base-url",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();
}
