use super::super::*;

#[test]
fn lookup_returns_matching_entry() {
    let catalog = RegistryCatalog::load_embedded().expect("registry");
    let opencode = catalog
        .lookup("opencode")
        .expect("opencode must be present in the embedded registry");
    assert_eq!(opencode.kind, RegistryKind::Native);
    assert!(opencode.headless_compatible);
    assert!(opencode.set_provider);
    assert!(opencode.multiple_active_providers);
    assert!(opencode.set_model);
    assert!(opencode.allow_custom_provider);
    assert!(opencode.allow_custom_model);
    assert!(opencode.set_mode);
    assert!(opencode.supports_agent_skills);
    assert_eq!(
        opencode.agent_skills_install_dir.as_deref(),
        Some("~/.agents/skills")
    );
    assert!(opencode.subagents);
    assert_eq!(opencode.subagent_alias.as_deref(), Some("small_model"));
    assert_eq!(
        opencode.support_doc.as_deref(),
        Some("docs/agents/opencode.md")
    );
}

#[test]
fn lookup_returns_none_for_unknown_id() {
    let catalog = RegistryCatalog::load_embedded().expect("registry");
    assert!(catalog.lookup("does-not-exist").is_none());
}

#[test]
fn lookup_required_rejects_legacy_placeholder_config() {
    let catalog = RegistryCatalog::load_embedded().expect("registry");
    assert!(matches!(
        catalog.lookup_required(LEGACY_PLACEHOLDER_AGENT_ID),
        Err(StackError::AgentPlaceholderConfigured)
    ));
}
