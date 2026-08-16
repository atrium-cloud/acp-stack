use super::super::*;

#[test]
fn validate_rejects_agent_skills_support_without_install_dir() {
    let body = r#"
[[agents]]
id = "bad-skills"
name = "Bad Skills"
kind = "native"
headless_compatible = true
supports_agent_skills = true
support_doc = "docs/agents/bad-skills.md"

[agents.harness]
id = "bad-skills"

[agents.harness.install.npm]
package = "bad-skills"
creates = "bad-skills"
"#;
    let err = RegistryCatalog::from_toml(body)
        .expect_err("skills support without install dir must be rejected");
    match err {
        StackError::RegistryLoad { reason } => {
            assert!(
                reason.contains("agent_skills_install_dir"),
                "reason: {reason}"
            );
        }
        other => panic!("expected RegistryLoad, got {other:?}"),
    }
}

#[test]
fn validate_rejects_invalid_agent_skills_install_dir() {
    let body = r#"
[[agents]]
id = "bad-skills"
name = "Bad Skills"
kind = "native"
headless_compatible = true
supports_agent_skills = true
agent_skills_install_dir = "relative/skills"
support_doc = "docs/agents/bad-skills.md"

[agents.harness]
id = "bad-skills"

[agents.harness.install.npm]
package = "bad-skills"
creates = "bad-skills"
"#;
    let err = RegistryCatalog::from_toml(body).expect_err("relative install dir must be rejected");
    match err {
        StackError::RegistryLoad { reason } => {
            assert!(reason.contains("must be absolute"), "reason: {reason}");
        }
        other => panic!("expected RegistryLoad, got {other:?}"),
    }
}

#[test]
fn validate_rejects_link_dir_without_skills_support() {
    let body = r#"
[[agents]]
id = "bad-skills"
name = "Bad Skills"
kind = "native"
headless_compatible = true
agent_skills_link_dir = "~/.bad/skills"
support_doc = "docs/agents/bad-skills.md"

[agents.harness]
id = "bad-skills"

[agents.harness.install.npm]
package = "bad-skills"
creates = "bad-skills"
"#;
    let err = RegistryCatalog::from_toml(body)
        .expect_err("link dir without skills support must be rejected");
    match err {
        StackError::RegistryLoad { reason } => {
            assert!(
                reason.contains("agent_skills_link_dir without supports_agent_skills"),
                "reason: {reason}"
            );
        }
        other => panic!("expected RegistryLoad, got {other:?}"),
    }
}

#[test]
fn validate_rejects_invalid_agent_skills_link_dir() {
    let body = r#"
[[agents]]
id = "bad-skills"
name = "Bad Skills"
kind = "native"
headless_compatible = true
supports_agent_skills = true
agent_skills_install_dir = "~/.agents/skills"
agent_skills_link_dir = "relative/skills"
support_doc = "docs/agents/bad-skills.md"

[agents.harness]
id = "bad-skills"

[agents.harness.install.npm]
package = "bad-skills"
creates = "bad-skills"
"#;
    let err = RegistryCatalog::from_toml(body).expect_err("relative link dir must be rejected");
    match err {
        StackError::RegistryLoad { reason } => {
            assert!(
                reason.contains("agent_skills_link_dir") && reason.contains("must be absolute"),
                "reason: {reason}"
            );
        }
        other => panic!("expected RegistryLoad, got {other:?}"),
    }
}

#[test]
fn validate_rejects_link_dir_equal_to_install_dir() {
    let body = r#"
[[agents]]
id = "bad-skills"
name = "Bad Skills"
kind = "native"
headless_compatible = true
supports_agent_skills = true
agent_skills_install_dir = "~/.agents/skills"
agent_skills_link_dir = "~/.agents/skills"
support_doc = "docs/agents/bad-skills.md"

[agents.harness]
id = "bad-skills"

[agents.harness.install.npm]
package = "bad-skills"
creates = "bad-skills"
"#;
    let err =
        RegistryCatalog::from_toml(body).expect_err("link dir equal to install dir must fail");
    match err {
        StackError::RegistryLoad { reason } => {
            assert!(
                reason.contains("must differ from agent_skills_install_dir"),
                "reason: {reason}"
            );
        }
        other => panic!("expected RegistryLoad, got {other:?}"),
    }

    let nested = body.replace(
        r#"agent_skills_link_dir = "~/.agents/skills""#,
        r#"agent_skills_link_dir = "~/.agents/skills/claude""#,
    );
    let err = RegistryCatalog::from_toml(&nested)
        .expect_err("link dir nested inside install dir must fail");
    match err {
        StackError::RegistryLoad { reason } => {
            assert!(reason.contains("neither may nest"), "reason: {reason}");
        }
        other => panic!("expected RegistryLoad, got {other:?}"),
    }

    let install_nested = body.replace(
        r#"agent_skills_install_dir = "~/.agents/skills""#,
        r#"agent_skills_install_dir = "~/.agents/skills/managed""#,
    );
    let err = RegistryCatalog::from_toml(&install_nested)
        .expect_err("install dir nested inside link dir must fail");
    match err {
        StackError::RegistryLoad { reason } => {
            assert!(reason.contains("neither may nest"), "reason: {reason}");
        }
        other => panic!("expected RegistryLoad, got {other:?}"),
    }

    let trailing_slash = body.replace(
        r#"agent_skills_link_dir = "~/.agents/skills""#,
        r#"agent_skills_link_dir = "~/.agents/skills/""#,
    );
    let err = RegistryCatalog::from_toml(&trailing_slash)
        .expect_err("trailing-slash alias of the install dir must fail");
    match err {
        StackError::RegistryLoad { reason } => {
            assert!(
                reason.contains("must differ from agent_skills_install_dir"),
                "reason: {reason}"
            );
        }
        other => panic!("expected RegistryLoad, got {other:?}"),
    }

    let double_slash = body.replace(
        r#"agent_skills_link_dir = "~/.agents/skills""#,
        r#"agent_skills_link_dir = "~/.agents//skills""#,
    );
    let err = RegistryCatalog::from_toml(&double_slash)
        .expect_err("double-slash alias of the install dir must fail");
    match err {
        StackError::RegistryLoad { reason } => {
            assert!(
                reason.contains("must differ from agent_skills_install_dir"),
                "reason: {reason}"
            );
        }
        other => panic!("expected RegistryLoad, got {other:?}"),
    }

    let double_slash_nested = body.replace(
        r#"agent_skills_link_dir = "~/.agents/skills""#,
        r#"agent_skills_link_dir = "~/.agents//skills/claude""#,
    );
    let err = RegistryCatalog::from_toml(&double_slash_nested)
        .expect_err("double-slash nested spelling must fail");
    match err {
        StackError::RegistryLoad { reason } => {
            assert!(reason.contains("neither may nest"), "reason: {reason}");
        }
        other => panic!("expected RegistryLoad, got {other:?}"),
    }
}
