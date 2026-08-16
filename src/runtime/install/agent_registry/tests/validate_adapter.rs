use super::super::*;

#[test]
fn validate_rejects_adapter_without_harness() {
    let body = r#"
[[agents]]
id = "bad"
name = "Bad"
kind = "adapter"

[agents.adapter]
id = "bad-adapter"

[agents.adapter.install.npm]
package = "bad"
creates = "bad"
"#;
    let err = RegistryCatalog::from_toml(body).expect_err("must reject adapter without harness");
    match err {
        StackError::RegistryLoad { reason } => {
            assert!(reason.contains("[agents.harness]"), "reason: {reason}");
        }
        other => panic!("expected RegistryLoad, got {other:?}"),
    }
}

#[test]
fn validate_rejects_native_with_adapter_install() {
    let body = r#"
[[agents]]
id = "bad"
name = "Bad"
kind = "native"

[agents.harness]
id = "bad"

[agents.harness.install.npm]
package = "bad"
creates = "bad"

[agents.adapter]
id = "adapter"

[agents.adapter.install.npm]
package = "adapter"
creates = "adapter"
"#;
    let err = RegistryCatalog::from_toml(body).expect_err("must reject native with adapter");
    match err {
        StackError::RegistryLoad { reason } => {
            assert!(reason.contains("[agents.adapter]"), "reason: {reason}");
        }
        other => panic!("expected RegistryLoad, got {other:?}"),
    }
}

#[test]
fn validate_accepts_adapter_harness_provided_by_adapter() {
    let body = r#"
[[agents]]
id = "sdk-backed"
name = "SDK Backed"
kind = "adapter"
headless_compatible = true
support_doc = "docs/agents/sdk-backed.md"

[agents.adapter]
id = "sdk-backed-acp"

[agents.adapter.install.npm]
package = "sdk-backed-acp"
creates = "sdk-backed-acp"

[agents.harness]
id = "sdk-agent-sdk"

[agents.harness.install]
provided_by = "adapter"
"#;
    let catalog = RegistryCatalog::from_toml(body).expect("registry should parse");
    let entry = catalog.lookup("sdk-backed").expect("entry exists");
    assert!(
        entry
            .harness
            .as_ref()
            .expect("harness")
            .install
            .is_provided_by_adapter()
    );
}

#[test]
fn validate_rejects_native_harness_provided_by_adapter() {
    let body = r#"
[[agents]]
id = "bad"
name = "Bad"
kind = "native"

[agents.harness]
id = "bad-sdk"

[agents.harness.install]
provided_by = "adapter"
"#;
    let err = RegistryCatalog::from_toml(body)
        .expect_err("native entries cannot use adapter-provided harnesses");
    match err {
        StackError::RegistryLoad { reason } => {
            assert!(reason.contains("kind=\"native\""), "reason: {reason}");
        }
        other => panic!("expected RegistryLoad, got {other:?}"),
    }
}

#[test]
fn validate_rejects_provided_by_with_install_paths() {
    let body = r#"
[[agents]]
id = "bad"
name = "Bad"
kind = "adapter"

[agents.adapter]
id = "bad-acp"

[agents.adapter.install.npm]
package = "bad-acp"
creates = "bad-acp"

[agents.harness]
id = "bad-sdk"

[agents.harness.install]
provided_by = "adapter"

[agents.harness.install.npm]
package = "bad-sdk"
creates = "bad-sdk"
"#;
    let err = RegistryCatalog::from_toml(body)
        .expect_err("provided_by cannot be combined with install paths");
    match err {
        StackError::RegistryLoad { reason } => {
            assert!(reason.contains("cannot be combined"), "reason: {reason}");
        }
        other => panic!("expected RegistryLoad, got {other:?}"),
    }
}

#[test]
fn validate_rejects_adapter_install_provided_by_adapter() {
    let body = r#"
[[agents]]
id = "bad"
name = "Bad"
kind = "adapter"

[agents.adapter]
id = "bad-acp"

[agents.adapter.install]
provided_by = "adapter"

[agents.harness]
id = "bad"

[agents.harness.install.npm]
package = "bad"
creates = "bad"
"#;
    let err = RegistryCatalog::from_toml(body)
        .expect_err("adapter install cannot use provided_by adapter");
    match err {
        StackError::RegistryLoad { reason } => {
            assert!(reason.contains("only valid"), "reason: {reason}");
        }
        other => panic!("expected RegistryLoad, got {other:?}"),
    }
}
