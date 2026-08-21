use super::super::*;

#[test]
fn validate_rejects_legacy_registry_fields() {
    let body = r#"
[[agents]]
id = "bad"
name = "Bad"
kind = "native"
homepage = "https://example.com"
headless_doc = "docs/agents/bad.md"
source_url = "https://example.com/install"
upstream_id = "bad-upstream"
adapter_install = { type = "npx", package = "bad" }

[agents.harness]
id = "bad"

[agents.harness.install.npm]
package = "bad"
creates = "bad"
"#;
    let err = RegistryCatalog::from_toml(body).expect_err("must reject old fields");
    match err {
        StackError::RegistryLoad { reason } => {
            assert!(
                reason.contains("unknown field") || reason.contains("unexpected keys"),
                "reason: {reason}"
            );
        }
        other => panic!("expected RegistryLoad, got {other:?}"),
    }
}

#[test]
fn validate_rejects_removed_supports_mcp_field() {
    // MCP support is determined by the post-install capability probe, never
    // declared in the registry; the old field must not silently round-trip.
    let body = r#"
[[agents]]
id = "bad"
name = "Bad"
kind = "native"
headless_compatible = true
support_doc = "docs/agents/bad.md"
supports_mcp = true

[agents.harness]
id = "bad"

[agents.harness.install.npm]
package = "bad"
creates = "bad"
"#;
    let err = RegistryCatalog::from_toml(body).expect_err("must reject supports_mcp");
    match err {
        StackError::RegistryLoad { reason } => {
            assert!(reason.contains("unknown field"), "reason: {reason}");
        }
        other => panic!("expected RegistryLoad, got {other:?}"),
    }
}

#[test]
fn validate_rejects_required_tool_paths() {
    let body = r#"
[[agents]]
id = "bad"
name = "Bad"
kind = "native"
headless_compatible = true
support_doc = "docs/agents/bad.md"

[agents.harness]
id = "bad"

[agents.harness.install.shell]
script = "true"
creates = "bad"
required_tools = ["/usr/bin/curl"]
"#;
    let err = RegistryCatalog::from_toml(body).expect_err("must reject tool path");
    match err {
        StackError::RegistryLoad { reason } => {
            assert!(
                reason.contains("must be a command name"),
                "reason: {reason}"
            );
        }
        other => panic!("expected RegistryLoad, got {other:?}"),
    }
}

#[test]
fn shell_install_carries_an_optional_timeout_override() {
    let body = r#"
[[agents]]
id = "slow"
name = "Slow"
kind = "native"
headless_compatible = true
support_doc = "docs/agents/slow.md"

[agents.harness]
id = "slow"

[agents.harness.install.shell]
script = "true"
creates = "slow"
timeout_secs = 2700
"#;
    let catalog = RegistryCatalog::from_toml(body).expect("timeout_secs must parse");
    let shell = catalog
        .lookup("slow")
        .and_then(|entry| entry.harness.as_ref())
        .and_then(|harness| harness.install.shell.as_ref())
        .expect("shell install");
    assert_eq!(shell.timeout_secs, Some(2700));
}

#[test]
fn validate_rejects_zero_shell_timeout() {
    let body = r#"
[[agents]]
id = "bad"
name = "Bad"
kind = "native"
headless_compatible = true
support_doc = "docs/agents/bad.md"

[agents.harness]
id = "bad"

[agents.harness.install.shell]
script = "true"
creates = "bad"
timeout_secs = 0
"#;
    let err = RegistryCatalog::from_toml(body).expect_err("must reject a zero budget");
    match err {
        StackError::RegistryLoad { reason } => {
            assert!(reason.contains("timeout_secs = 0"), "reason: {reason}");
        }
        other => panic!("expected RegistryLoad, got {other:?}"),
    }
}

#[test]
fn validate_rejects_shell_timeout_past_the_cap() {
    // A near-u64::MAX budget would overflow the `Instant::now() + timeout`
    // deadline arithmetic in `run_captured`; the 24h cap rejects it at parse.
    let body = r#"
[[agents]]
id = "bad"
name = "Bad"
kind = "native"
headless_compatible = true
support_doc = "docs/agents/bad.md"

[agents.harness]
id = "bad"

[agents.harness.install.shell]
script = "true"
creates = "bad"
timeout_secs = 9999999999
"#;
    let err = RegistryCatalog::from_toml(body).expect_err("must reject an over-cap budget");
    match err {
        StackError::RegistryLoad { reason } => {
            assert!(
                reason.contains("timeout_secs = 9999999999 exceeds the 86400-second"),
                "reason: {reason}"
            );
        }
        other => panic!("expected RegistryLoad, got {other:?}"),
    }
}

#[test]
fn github_values_accept_path_shorthand_and_derive_repo() {
    assert_eq!(
        github_repo_from_url(
            "pi",
            "github",
            "earendil-works/pi/tree/main/packages/coding-agent"
        )
        .expect("repo"),
        "earendil-works/pi"
    );
    assert_eq!(
        github_url_from_value(
            "pi",
            "github",
            "earendil-works/pi/tree/main/packages/coding-agent"
        )
        .expect("url"),
        "https://github.com/earendil-works/pi/tree/main/packages/coding-agent"
    );
    assert_eq!(
        github_repo_from_url(
            "amp",
            "adapter.github",
            "https://github.com/tao12345666333/amp-acp"
        )
        .expect("repo"),
        "tao12345666333/amp-acp"
    );
}

#[test]
fn validate_rejects_duplicate_ids() {
    let body = r#"
[[agents]]
id = "dup"
name = "First"
kind = "native"

[agents.harness]
id = "first"

[agents.harness.install.npm]
package = "first"
creates = "first"

[[agents]]
id = "dup"
name = "Second"
kind = "native"

[agents.harness]
id = "second"

[agents.harness.install.npm]
package = "second"
creates = "second"
"#;
    let err = RegistryCatalog::from_toml(body).expect_err("must reject duplicate ids");
    match err {
        StackError::RegistryLoad { reason } => {
            assert!(reason.contains("duplicate"), "reason: {reason}");
        }
        other => panic!("expected RegistryLoad, got {other:?}"),
    }
}

#[test]
fn validate_rejects_headless_entry_without_doc() {
    let body = r#"
[[agents]]
id = "bad"
name = "Bad"
kind = "native"
headless_compatible = true

[agents.harness]
id = "bad"

[agents.harness.install.npm]
package = "bad"
creates = "bad"
"#;
    let err = RegistryCatalog::from_toml(body)
        .expect_err("must reject headless-compatible entry without doc");
    match err {
        StackError::RegistryLoad { reason } => {
            assert!(reason.contains("support_doc"), "reason: {reason}");
        }
        other => panic!("expected RegistryLoad, got {other:?}"),
    }
}

#[test]
fn validate_rejects_empty_acp_args() {
    // An empty override would launch the harness with no ACP entry point.
    let body = r#"
[[agents]]
id = "bad"
name = "Bad"
kind = "native"
headless_compatible = true
support_doc = "docs/agents/bad.md"

[agents.harness]
id = "bad"
acp_args = []

[agents.harness.install.npm]
package = "bad"
creates = "bad"
"#;
    let err = RegistryCatalog::from_toml(body).expect_err("must reject empty acp_args");
    match err {
        StackError::RegistryLoad { reason } => {
            assert!(
                reason.contains("harness.acp_args must not be empty"),
                "reason: {reason}"
            );
        }
        other => panic!("expected RegistryLoad, got {other:?}"),
    }
}

#[test]
fn validate_rejects_shell_rerun_without_a_shell_install() {
    // The flag means "re-run the recipe", so an entry without a recipe has
    // declared an update path that cannot exist.
    let body = r#"
[[agents]]
id = "bad"
name = "Bad"
kind = "native"
headless_compatible = true
support_doc = "docs/agents/bad.md"

[agents.harness]
id = "bad"

[agents.harness.install.npm]
package = "bad"
creates = "bad"

[agents.harness.update]
shell_rerun = true
"#;
    let err = RegistryCatalog::from_toml(body).expect_err("must reject shell_rerun without shell");
    match err {
        StackError::RegistryLoad { reason } => {
            assert!(
                reason.contains("harness.update.shell_rerun requires harness.install.shell"),
                "reason: {reason}"
            );
        }
        other => panic!("expected RegistryLoad, got {other:?}"),
    }
}

#[test]
fn validate_rejects_entry_sync_id_with_surrounding_whitespace() {
    let body = r#"
[[agents]]
id = "bad"
name = "Bad"
kind = "native"
headless_compatible = true
sync_id = " bad-acp "
support_doc = "docs/agents/bad.md"

[agents.harness]
id = "bad"

[agents.harness.install.npm]
package = "bad"
creates = "bad"
"#;
    let err = RegistryCatalog::from_toml(body).expect_err("must reject untrimmed sync_id");
    match err {
        StackError::RegistryLoad { reason } => {
            assert!(
                reason.contains("sync_id is empty or has surrounding whitespace"),
                "reason: {reason}"
            );
        }
        other => panic!("expected RegistryLoad, got {other:?}"),
    }
}

#[test]
fn validate_rejects_blank_acp_arg() {
    // A whitespace-only argument is as unusable as an empty string.
    let body = r#"
[[agents]]
id = "bad"
name = "Bad"
kind = "native"
headless_compatible = true
support_doc = "docs/agents/bad.md"

[agents.harness]
id = "bad"
acp_args = ["  "]

[agents.harness.install.npm]
package = "bad"
creates = "bad"
"#;
    let err = RegistryCatalog::from_toml(body).expect_err("must reject a blank acp_arg");
    match err {
        StackError::RegistryLoad { reason } => {
            assert!(
                reason.contains("harness.acp_args is empty"),
                "reason: {reason}"
            );
        }
        other => panic!("expected RegistryLoad, got {other:?}"),
    }
}
