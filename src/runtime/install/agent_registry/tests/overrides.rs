use super::super::*;

#[test]
fn override_replaces_entry_by_id() {
    let base = RegistryCatalog::load_embedded().expect("registry");
    let overlay_body = r#"
[[agents]]
id = "opencode"
name = "OpenCode (private fork)"
kind = "native"
support_doc = "docs/agents/opencode.md"

[agents.harness]
id = "opencode"

[agents.harness.install.npm]
package = "@private/opencode"
creates = "opencode"
"#;
    let overlay = RegistryCatalog::from_toml(overlay_body).expect("overlay parses");
    let mut catalog = base;
    catalog.merge(overlay);
    let entry = catalog.lookup("opencode").expect("entry exists");
    assert_eq!(entry.kind, RegistryKind::Native);
    assert_eq!(entry.name, "OpenCode (private fork)");
}
