use super::*;

#[test]
fn embedded_registry_parses() {
    let catalog = RegistryCatalog::load_embedded().expect("embedded registry must parse");
    assert!(
        !catalog.entries().is_empty(),
        "embedded registry must have at least one entry"
    );
}

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
fn opencode_keeps_an_npm_fallback_behind_its_github_backed_shell_installer() {
    // The shell recipe runs opencode's upstream installer, which resolves its
    // release through the GitHub API and fails on rate-limited hosts. The npm
    // path is what the fallback chain lands on then, so it must stay declared.
    let catalog = RegistryCatalog::load_embedded().expect("registry");
    let harness = catalog
        .lookup("opencode")
        .expect("opencode must be present in the embedded registry")
        .harness
        .as_ref()
        .expect("opencode must declare a harness");
    let shell = harness
        .install
        .shell
        .as_ref()
        .expect("opencode must declare a shell install path");
    assert!(
        !shell
            .script
            .contains("curl -fsSL https://opencode.ai/install |"),
        "the installer must be fetched before it runs so a failed fetch is fatal",
    );
    let npm = harness
        .install
        .npm
        .as_ref()
        .expect("opencode must declare an npm fallback install path");
    assert_eq!(npm.package, "opencode-ai");
    assert_eq!(npm.creates, "opencode");
}

#[test]
fn multiple_active_provider_capability_is_limited_to_opencode_and_pi() {
    let catalog = RegistryCatalog::load_embedded().expect("registry");
    let capable = catalog
        .entries()
        .iter()
        .filter(|entry| entry.multiple_active_providers)
        .map(|entry| entry.id.as_str())
        .collect::<Vec<_>>();

    assert_eq!(capable, ["opencode", "pi"]);
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

#[test]
fn embedded_registry_advertises_tested_headless_support() {
    let catalog = RegistryCatalog::load_embedded().expect("registry");
    let supported: Vec<_> = catalog
        .entries()
        .iter()
        .filter(|entry| entry.headless_compatible)
        .map(|entry| entry.id.as_str())
        .collect();
    assert_eq!(
        supported,
        [
            "opencode",
            "cursor",
            "amp",
            "pi",
            "goose",
            "codex",
            "claude-code",
            "kimi",
            "hermes"
        ]
    );
    for entry in catalog
        .entries()
        .iter()
        .filter(|entry| entry.headless_compatible)
    {
        assert!(
            entry.supports_agent_skills,
            "{} must advertise Agent Skills support",
            entry.id
        );
        assert!(
            entry.agent_skills_install_dir.as_deref() == Some("~/.agents/skills"),
            "{} must declare the documented Agent Skills install directory",
            entry.id
        );
        // Claude Code only discovers `~/.claude/skills` and Hermes only
        // discovers `~/.hermes/skills`, so they are the agents whose installed
        // skills get symlinked out of the shared dir. Amp reads
        // `~/.agents/skills` natively, so it needs no link dir.
        match entry.id.as_str() {
            "claude-code" => assert_eq!(
                entry.agent_skills_link_dir.as_deref(),
                Some("~/.claude/skills")
            ),
            "hermes" => assert_eq!(
                entry.agent_skills_link_dir.as_deref(),
                Some("~/.hermes/skills")
            ),
            _ => assert!(entry.agent_skills_link_dir.is_none()),
        }
        assert_eq!(
            entry.testflight_expect_fs.as_deref(),
            Some(".acp-stack-testflight.txt"),
            "{} must declare filesystem test output",
            entry.id
        );
        let prompt = entry
            .testflight_prompt
            .as_deref()
            .unwrap_or_else(|| panic!("{} must declare a testflight prompt", entry.id));
        assert!(
            prompt.contains(".acp-stack-testflight.txt"),
            "{} prompt must mention test output path",
            entry.id
        );
    }
}

#[test]
fn embedded_registry_contains_only_curated_examples() {
    let catalog = RegistryCatalog::load_embedded().expect("registry");
    let ids: Vec<_> = catalog
        .entries()
        .iter()
        .map(|entry| entry.id.as_str())
        .collect();
    assert_eq!(
        ids,
        [
            "opencode",
            "cursor",
            "amp",
            "pi",
            "goose",
            "codex",
            "claude-code",
            "kimi",
            "hermes"
        ]
    );
    let cursor = catalog.lookup("cursor").expect("cursor entry exists");
    assert_eq!(cursor.kind, RegistryKind::Native);
    assert!(cursor.headless_compatible);
    assert_eq!(cursor.stdio_framing, RegistryStdioFraming::JsonLines);
    assert!(!cursor.set_provider);
    assert!(cursor.set_model);
    assert!(!cursor.allow_custom_provider);
    assert!(!cursor.allow_custom_model);
    assert!(cursor.set_mode);
    assert_eq!(cursor.support_doc.as_deref(), Some("docs/agents/cursor.md"));
    let amp = catalog.lookup("amp").expect("amp entry exists");
    assert_eq!(amp.kind, RegistryKind::Adapter);
    assert!(amp.headless_compatible);
    assert!(!amp.set_provider);
    assert!(!amp.set_model);
    assert!(!amp.allow_custom_provider);
    assert!(!amp.allow_custom_model);
    assert!(amp.set_mode);
    assert_eq!(
        amp.adapter.as_ref().map(|adapter| adapter.id.as_str()),
        Some("amp-acp")
    );
    assert_eq!(
        amp.adapter
            .as_ref()
            .and_then(|adapter| adapter.github.as_deref()),
        Some("tao12345666333/amp-acp")
    );
    assert_eq!(amp.support_doc.as_deref(), Some("docs/agents/amp.md"));
    let pi = catalog.lookup("pi").expect("pi entry exists");
    assert_eq!(pi.kind, RegistryKind::Adapter);
    assert!(pi.headless_compatible);
    assert!(pi.set_provider);
    assert!(pi.set_model);
    assert!(pi.allow_custom_provider);
    assert!(pi.allow_custom_model);
    assert!(!pi.set_mode);
    assert_eq!(pi.stdio_framing, RegistryStdioFraming::JsonLines);
    let goose = catalog.lookup("goose").expect("goose entry exists");
    assert_eq!(goose.kind, RegistryKind::Native);
    assert!(goose.headless_compatible);
    assert!(goose.set_provider);
    assert!(goose.set_model);
    assert!(goose.allow_custom_provider);
    assert!(goose.allow_custom_model);
    assert!(!goose.set_mode);
    assert_eq!(goose.stdio_framing, RegistryStdioFraming::JsonLines);
    assert_eq!(goose.support_doc.as_deref(), Some("docs/agents/goose.md"));
    let codex = catalog.lookup("codex").expect("codex entry exists");
    assert_eq!(codex.kind, RegistryKind::Adapter);
    assert!(codex.headless_compatible);
    assert!(codex.set_provider);
    assert!(codex.set_model);
    assert!(codex.allow_custom_provider);
    assert!(codex.allow_custom_model);
    assert!(codex.set_mode);
    assert_eq!(
        codex.adapter.as_ref().map(|adapter| adapter.id.as_str()),
        Some("codex-acp")
    );
    let codex_adapter_install = &codex.adapter.as_ref().expect("codex adapter").install;
    assert_eq!(
        codex_adapter_install
            .npm
            .as_ref()
            .map(|install| install.package.as_str()),
        Some("@agentclientprotocol/codex-acp")
    );
    let codex_harness_github = codex
        .harness
        .as_ref()
        .and_then(|harness| harness.install.github.as_ref())
        .expect("codex harness github install");
    assert_eq!(
        codex_harness_github.archive_binary_name.as_deref(),
        Some("codex-{arch}-unknown-linux-musl")
    );
    assert_eq!(codex.support_doc.as_deref(), Some("docs/agents/codex.md"));
    let claude_code = catalog
        .lookup("claude-code")
        .expect("Claude Code entry exists");
    assert_eq!(claude_code.kind, RegistryKind::Adapter);
    assert!(claude_code.headless_compatible);
    assert!(claude_code.set_provider);
    assert!(claude_code.set_model);
    assert!(claude_code.allow_custom_provider);
    assert!(claude_code.allow_custom_model);
    assert!(claude_code.set_mode);
    assert!(claude_code.supports_agent_skills);
    assert_eq!(
        claude_code.agent_skills_install_dir.as_deref(),
        Some("~/.agents/skills")
    );
    assert_eq!(
        claude_code.agent_skills_link_dir.as_deref(),
        Some("~/.claude/skills")
    );
    assert_eq!(
        claude_code
            .adapter
            .as_ref()
            .map(|adapter| adapter.id.as_str()),
        Some("claude-agent-acp")
    );
    assert_eq!(
        claude_code
            .adapter
            .as_ref()
            .and_then(|adapter| adapter.sync_id.as_deref()),
        Some("claude-acp")
    );
    let claude_code_adapter_install = &claude_code
        .adapter
        .as_ref()
        .expect("Claude Code adapter")
        .install;
    assert_eq!(
        claude_code_adapter_install
            .npm
            .as_ref()
            .map(|install| install.package.as_str()),
        Some("@agentclientprotocol/claude-agent-acp")
    );
    let claude_code_harness_install = &claude_code
        .harness
        .as_ref()
        .expect("Claude Code harness")
        .install;
    assert!(claude_code_harness_install.is_provided_by_adapter());
    assert!(claude_code_harness_install.shell.is_none());
    assert!(claude_code_harness_install.npm.is_none());
    assert!(claude_code_harness_install.github.is_none());
    assert_eq!(
        claude_code.support_doc.as_deref(),
        Some("docs/agents/claude-code.md")
    );
    let kimi = catalog.lookup("kimi").expect("Kimi Code entry exists");
    assert_eq!(kimi.name, "Kimi Code");
    assert_eq!(kimi.kind, RegistryKind::Native);
    assert!(kimi.headless_compatible);
    assert!(!kimi.set_provider);
    assert!(kimi.set_model);
    assert!(!kimi.allow_custom_provider);
    assert!(!kimi.allow_custom_model);
    assert!(kimi.set_mode);
    assert!(kimi.supports_agent_skills);
    assert_eq!(
        kimi.agent_skills_install_dir.as_deref(),
        Some("~/.agents/skills")
    );
    assert!(!kimi.subagents);
    assert_eq!(kimi.github.as_deref(), Some("MoonshotAI/kimi-code"));
    assert_eq!(kimi.support_doc.as_deref(), Some("docs/agents/kimi.md"));
    let kimi_harness = kimi.harness.as_ref().expect("Kimi Code harness");
    assert_eq!(kimi_harness.id, "kimi");
    assert!(
        kimi_harness.install.npm.is_none(),
        "shell installer is Kimi Code's only official channel"
    );
    let kimi_shell = kimi_harness
        .install
        .shell
        .as_ref()
        .expect("Kimi Code shell install");
    assert!(
        kimi_shell
            .script
            .contains("https://code.kimi.com/kimi-code/install.sh")
    );
    assert!(kimi_shell.script.contains("KIMI_INSTALL_DIR"));
    assert!(kimi_shell.script.contains("KIMI_NO_MODIFY_PATH=1"));
    let hermes = catalog.lookup("hermes").expect("Hermes Agent entry exists");
    assert_eq!(hermes.name, "Hermes Agent");
    assert_eq!(hermes.kind, RegistryKind::Native);
    assert!(hermes.headless_compatible);
    assert!(hermes.set_provider);
    assert!(hermes.set_model);
    assert!(hermes.allow_custom_provider);
    assert!(hermes.allow_custom_model);
    assert!(!hermes.set_mode);
    assert!(hermes.supports_agent_skills);
    assert_eq!(
        hermes.agent_skills_install_dir.as_deref(),
        Some("~/.agents/skills")
    );
    assert!(!hermes.subagents);
    assert_eq!(hermes.support_doc.as_deref(), Some("docs/agents/hermes.md"));
    assert!(hermes.sync_exempt);
    let hermes_harness = hermes.harness.as_ref().expect("Hermes Agent harness");
    assert_eq!(hermes_harness.id, "hermes");
    assert!(
        hermes_harness.install.npm.is_none(),
        "shell installer is Hermes Agent's only official channel"
    );
    let hermes_shell = hermes_harness
        .install
        .shell
        .as_ref()
        .expect("Hermes Agent shell install");
    assert!(
        hermes_shell
            .script
            .contains("https://hermes-agent.nousresearch.com/install.sh")
    );
    assert!(hermes_shell.script.contains("--skip-browser"));
    assert!(hermes_shell.script.contains("'.[acp]'"));
    for entry in catalog.entries() {
        assert_eq!(
            entry.sync_exempt,
            entry.id == "hermes",
            "sync_exempt is a narrow escape hatch; `{}` must not carry it",
            entry.id
        );
    }
}

#[test]
fn embedded_registry_uses_per_install_arch_maps() {
    let catalog = RegistryCatalog::load_embedded().expect("registry");
    let opencode = catalog.lookup("opencode").expect("opencode entry exists");
    let opencode_github = opencode
        .harness
        .as_ref()
        .and_then(|harness| harness.install.github.as_ref())
        .expect("opencode github install");
    assert_eq!(opencode_github.arch.x86_64.as_deref(), Some("x64"));
    assert_eq!(opencode_github.arch.aarch64.as_deref(), Some("arm64"));

    let amp = catalog.lookup("amp").expect("amp entry exists");
    let amp_github = amp
        .adapter
        .as_ref()
        .and_then(|adapter| adapter.install.github.as_ref())
        .expect("amp-acp github install");
    assert_eq!(amp_github.arch.x86_64.as_deref(), Some("x86_64"));
    assert_eq!(amp_github.arch.aarch64.as_deref(), Some("aarch64"));

    let codex = catalog.lookup("codex").expect("codex entry exists");
    assert!(
        codex
            .adapter
            .as_ref()
            .and_then(|adapter| adapter.install.github.as_ref())
            .is_none(),
        "codex-acp is npm-only since the agentclientprotocol move"
    );
}

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
