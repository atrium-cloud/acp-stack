use super::super::*;

#[test]
fn parses_optional_testflight_fields() {
    let body = r#"
[[agents]]
id = "test-agent"
name = "Test Agent"
kind = "native"
headless_compatible = true
support_doc = "docs/agents/test-agent.md"
testflight_prompt = "Create /workspace/.acp-stack-testflight.txt with text 'ok'"
testflight_expect_fs = ".acp-stack-testflight.txt"

[agents.harness]
id = "test-agent"

[agents.harness.install.npm]
package = "test-agent"
creates = "test-agent"
"#;
    let catalog = RegistryCatalog::from_toml(body).expect("registry should parse");
    let entry = catalog.lookup("test-agent").expect("entry exists");
    assert_eq!(
        entry.testflight_prompt.as_deref(),
        Some("Create /workspace/.acp-stack-testflight.txt with text 'ok'")
    );
    assert_eq!(
        entry.testflight_expect_fs.as_deref(),
        Some(".acp-stack-testflight.txt")
    );
}

#[test]
fn validate_rejects_absolute_testflight_expect_fs() {
    let body = r#"
[[agents]]
id = "bad-expect"
name = "Bad Expect"
kind = "native"
headless_compatible = true
support_doc = "docs/agents/bad-expect.md"
testflight_expect_fs = "/etc/passwd"

[agents.harness]
id = "bad-expect"

[agents.harness.install.npm]
package = "bad-expect"
creates = "bad-expect"
"#;
    let err = RegistryCatalog::from_toml(body)
        .expect_err("absolute testflight_expect_fs must be rejected");
    match err {
        StackError::RegistryLoad { reason } => {
            assert!(
                reason.contains("must be workspace-relative"),
                "reason: {reason}"
            );
        }
        other => panic!("expected RegistryLoad, got {other:?}"),
    }
}

#[test]
fn validate_rejects_testflight_expect_fs_with_parent_segment() {
    let body = r#"
[[agents]]
id = "bad-expect"
name = "Bad Expect"
kind = "native"
headless_compatible = true
support_doc = "docs/agents/bad-expect.md"
testflight_expect_fs = "subdir/../escape.txt"

[agents.harness]
id = "bad-expect"

[agents.harness.install.npm]
package = "bad-expect"
creates = "bad-expect"
"#;
    let err = RegistryCatalog::from_toml(body)
        .expect_err("testflight_expect_fs with `..` must be rejected");
    match err {
        StackError::RegistryLoad { reason } => {
            assert!(reason.contains("`..`"), "reason: {reason}");
        }
        other => panic!("expected RegistryLoad, got {other:?}"),
    }
}

#[test]
fn validate_rejects_empty_testflight_prompt() {
    let body = r#"
[[agents]]
id = "bad-prompt"
name = "Bad Prompt"
kind = "native"
headless_compatible = true
support_doc = "docs/agents/bad-prompt.md"
testflight_prompt = "   "

[agents.harness]
id = "bad-prompt"

[agents.harness.install.npm]
package = "bad-prompt"
creates = "bad-prompt"
"#;
    let err =
        RegistryCatalog::from_toml(body).expect_err("empty testflight_prompt must be rejected");
    match err {
        StackError::RegistryLoad { reason } => {
            assert!(reason.contains("testflight_prompt"), "reason: {reason}");
        }
        other => panic!("expected RegistryLoad, got {other:?}"),
    }
}
