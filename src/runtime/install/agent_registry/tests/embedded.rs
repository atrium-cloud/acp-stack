use super::super::*;

#[test]
fn embedded_registry_parses() {
    let catalog = RegistryCatalog::load_embedded().expect("embedded registry must parse");
    assert!(
        !catalog.entries().is_empty(),
        "embedded registry must have at least one entry"
    );
}

/// The flag gates whether a managed credential may carry a `base_url`, so an
/// agent listed here without a native endpoint field would accept a routing
/// instruction it silently never applies.
#[test]
fn only_agents_with_a_native_endpoint_field_declare_set_provider_base_url() {
    let catalog = RegistryCatalog::load_embedded().expect("registry");
    for id in ["opencode", "pi", "codex", "claude-code"] {
        assert!(
            catalog.supports_provider_base_url(id),
            "`{id}` writes a per-provider endpoint and must declare set_provider_base_url"
        );
    }
    for id in ["goose", "amp", "kimi", "hermes"] {
        assert!(
            !catalog.supports_provider_base_url(id),
            "`{id}` has no per-provider endpoint field and must not declare set_provider_base_url"
        );
    }
    // An agent outside the registry has no acps-managed native config at all.
    assert!(!catalog.supports_provider_base_url("not-a-registered-agent"));
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
            "amp",
            "pi",
            "goose",
            "codex",
            "claude-code",
            "kimi",
            "hermes"
        ]
    );
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
    // The recipe drives an upstream installer that provisions a Python
    // toolchain, a virtualenv and a source checkout under one budget, and all
    // of it fits the shared default: measured at 185s end to end on a fresh
    // 8-core host. An earlier override here was fitted to a host whose
    // `python3` was a wrapper script, which made the upstream installer's
    // node-gyp step unbounded — a defect no budget could have absorbed.
    assert_eq!(hermes_shell.timeout_secs, None);
    for entry in catalog.entries() {
        for shell in [
            entry
                .harness
                .as_ref()
                .and_then(|harness| harness.install.shell.as_ref()),
            entry
                .adapter
                .as_ref()
                .and_then(|adapter| adapter.install.shell.as_ref()),
        ]
        .into_iter()
        .flatten()
        {
            assert_eq!(
                shell.timeout_secs, None,
                "the budget override is per-recipe and no shipped recipe needs one; `{}` must not \
                 carry a value on a harness or adapter shell recipe without a measurement behind it",
                entry.id
            );
        }
    }
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
