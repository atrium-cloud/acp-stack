use super::super::*;
use crate::config::{
    AgentAdapterOverrideConfig, AgentAdapterOverrideInstall, AgentAdapterOverrideNpmInstall,
    AgentConfig,
};
use std::borrow::Cow;

fn agent_with_override(
    id: &str,
    override_config: Option<AgentAdapterOverrideConfig>,
) -> AgentConfig {
    AgentConfig {
        id: id.to_owned(),
        name: id.to_owned(),
        command: id.to_owned(),
        args: Vec::new(),
        cwd: None,
        env: Vec::new(),
        expected_sha256: None,
        restart: "on-crash".to_owned(),
        mode: None,
        model: None,
        effort: None,
        config_options: Default::default(),
        harness_version: None,
        adapter: None,
        adapter_override: override_config,
        provider: None,
        providers: None,
        subagent: None,
        auto_update: None,
        install: None,
    }
}

fn npm_override(command: &str, package: &str) -> AgentAdapterOverrideConfig {
    AgentAdapterOverrideConfig {
        command: command.to_owned(),
        args: Vec::new(),
        github: None,
        install: AgentAdapterOverrideInstall {
            shell: None,
            npm: Some(AgentAdapterOverrideNpmInstall {
                package: package.to_owned(),
                creates: command.to_owned(),
            }),
            github: None,
        },
        update: Default::default(),
    }
}

#[test]
fn native_entry_with_override_becomes_adapter_kind_with_harness_untouched() {
    let catalog = RegistryCatalog::load_embedded().expect("registry");
    let entry = catalog.lookup("goose").expect("goose entry");
    let agent = agent_with_override("goose", Some(npm_override("custom-acp", "custom-acp")));

    let effective = effective_registry_entry(entry, &agent).expect("override converts");
    assert!(matches!(effective, Cow::Owned(_)));
    assert_eq!(effective.kind, RegistryKind::Adapter);
    let adapter = effective.adapter.as_ref().expect("adapter spec");
    assert_eq!(adapter.id, "custom-acp");
    assert_eq!(
        adapter.install.npm.as_ref().map(|npm| npm.package.as_str()),
        Some("custom-acp")
    );
    // Harness and support metadata stay registry-managed.
    let harness = effective.harness.as_ref().expect("harness");
    assert_eq!(harness.id, "goose");
    assert_eq!(harness.acp_args, vec!["acp".to_owned()]);
    assert!(harness.install.shell.is_some());
    assert_eq!(effective.agent_skills_link_dir, entry.agent_skills_link_dir);
    assert_eq!(effective.set_provider, entry.set_provider);
}

#[test]
fn adapter_entry_with_override_replaces_adapter_spec() {
    let catalog = RegistryCatalog::load_embedded().expect("registry");
    let entry = catalog.lookup("codex").expect("codex entry");
    let agent = agent_with_override(
        "codex",
        Some(npm_override("codex-acp-fork", "@fork/codex-acp")),
    );

    let effective = effective_registry_entry(entry, &agent).expect("override converts");
    assert_eq!(effective.kind, RegistryKind::Adapter);
    let adapter = effective.adapter.as_ref().expect("adapter spec");
    assert_eq!(adapter.id, "codex-acp-fork");
    assert_eq!(
        effective
            .harness
            .as_ref()
            .map(|harness| harness.id.as_str()),
        Some("codex")
    );
}

#[test]
fn entry_without_override_is_borrowed() {
    let catalog = RegistryCatalog::load_embedded().expect("registry");
    let entry = catalog.lookup("goose").expect("goose entry");
    let agent = agent_with_override("goose", None);
    let effective = effective_registry_entry(entry, &agent).expect("passthrough");
    assert!(matches!(effective, Cow::Borrowed(_)));
}

#[test]
fn override_for_a_different_agent_id_is_ignored() {
    let catalog = RegistryCatalog::load_embedded().expect("registry");
    let entry = catalog.lookup("goose").expect("goose entry");
    // An array target's agent block must not leak its override onto lookups
    // for other entries.
    let agent = agent_with_override("opencode", Some(npm_override("custom-acp", "custom-acp")));
    let effective = effective_registry_entry(entry, &agent).expect("passthrough");
    assert!(matches!(effective, Cow::Borrowed(_)));
    assert_eq!(effective.kind, RegistryKind::Native);
}

#[test]
fn github_install_without_repo_is_rejected() {
    let catalog = RegistryCatalog::load_embedded().expect("registry");
    let entry = catalog.lookup("goose").expect("goose entry");
    let mut override_config = npm_override("custom-acp", "custom-acp");
    override_config.install.npm = None;
    override_config.install.github = Some(crate::config::AgentAdapterOverrideGithubInstall {
        asset_pattern: "custom-acp-linux-{arch}.tar.gz".to_owned(),
        archive: crate::config::AgentAdapterOverrideArchiveKind::TarGz,
        archive_binary_name: None,
        binary_name: "custom-acp".to_owned(),
        checksums_asset: None,
        arch: crate::config::AgentAdapterOverrideArchMap {
            x86_64: Some("x86_64".to_owned()),
            aarch64: Some("aarch64".to_owned()),
        },
    });
    let agent = agent_with_override("goose", Some(override_config));

    let error = effective_registry_entry(entry, &agent).expect_err("github repo required");
    let message = error.to_string();
    assert!(message.contains("[agent.adapter_override]"), "{message}");
    assert!(message.contains("github"), "{message}");
}
