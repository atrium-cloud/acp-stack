use super::super::*;

#[test]
fn embedded_registry_parses() {
    let catalog = RegistryCatalog::load_embedded().expect("embedded registry must parse");
    assert!(
        !catalog.entries().is_empty(),
        "embedded registry must have at least one entry"
    );
}

/// Kimi's default mode parks every tool call on an operator permission decision.
#[test]
fn kimi_declares_yolo_as_the_unattended_default_mode() {
    let catalog = RegistryCatalog::load_embedded().expect("registry");
    let kimi = catalog.lookup("kimi").expect("kimi entry");
    assert_eq!(kimi.default_mode.as_deref(), Some("yolo"));
}

/// The flag gates whether a managed credential may carry a `base_url`. Every agent that talks
/// plain HTTP to its provider has an endpoint field acp-stack writes; amp reaches its own
/// backend over a websocket and is the sole exception.
#[test]
fn every_http_agent_declares_set_provider_base_url() {
    let catalog = RegistryCatalog::load_embedded().expect("registry");
    for entry in catalog.entries() {
        let expected = entry.id != "amp";
        assert_eq!(
            catalog.supports_provider_base_url(&entry.id),
            expected,
            "`{}` set_provider_base_url must be {expected}",
            entry.id
        );
    }
    assert!(!catalog.supports_provider_base_url("not-a-registered-agent"));
}

#[test]
fn opencode_keeps_an_npm_fallback_behind_its_github_backed_shell_installer() {
    // The shell recipe resolves its release through the GitHub API and fails on
    // rate-limited hosts, so the npm fallback must stay declared.
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
            "hermes",
            "kilo",
            "antigravity"
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
        // Claude Code and Hermes discover their own dirs, so their skills get
        // symlinked out of the shared one; the rest read `~/.agents/skills` natively.
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
            "hermes",
            "kilo",
            "antigravity"
        ]
    );
    let opencode = catalog.lookup("opencode").expect("opencode entry exists");
    assert!(opencode.set_mode);
    assert!(opencode.set_effort);
    let amp = catalog.lookup("amp").expect("amp entry exists");
    assert_eq!(amp.kind, RegistryKind::Adapter);
    assert!(amp.headless_compatible);
    assert!(!amp.set_provider);
    assert!(amp.set_model);
    assert!(!amp.allow_custom_provider);
    assert!(!amp.allow_custom_model);
    assert!(amp.set_mode);
    assert!(!amp.set_effort);
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
    assert!(pi.set_effort);
    assert_eq!(pi.stdio_framing, RegistryStdioFraming::JsonLines);
    assert!(pi.sync_exempt);
    let pi_adapter = pi.adapter.as_ref().expect("pi adapter");
    assert_eq!(pi_adapter.id, "pi-acp");
    assert_eq!(pi_adapter.github.as_deref(), Some("atrium-cloud/pi-acp"));
    assert!(
        pi_adapter.install.npm.is_none(),
        "the pi-acp release is GitHub-only; the npm `pi-acp` package is another project"
    );
    assert!(pi_adapter.install.github.is_none());
    let pi_adapter_shell = pi_adapter
        .install
        .shell
        .as_ref()
        .expect("pi adapter shell install");
    assert!(pi_adapter_shell.script.contains("until node_ready"));
    assert!(!pi_adapter_shell.script.contains("nodejs.org"));
    assert!(
        pi_adapter_shell
            .script
            .contains("https://github.com/atrium-cloud/pi-acp/releases/latest/download/pi-acp.zip")
    );
    assert!(pi_adapter_shell.script.contains("cmp -s"));
    assert_eq!(pi_adapter_shell.creates, "pi-acp");
    assert!(pi_adapter.update.shell_rerun);
    // The bundle carries no Pi, so the harness install stays and provides Node.
    let pi_harness = pi.harness.as_ref().expect("pi harness");
    assert!(!pi_harness.install.is_provided_by_adapter());
    let pi_harness_shell = pi_harness
        .install
        .shell
        .as_ref()
        .expect("pi harness shell install");
    assert!(
        pi_harness_shell
            .script
            .contains("nodejs.org/dist/latest-v22.x/")
    );
    assert!(
        pi_harness_shell
            .script
            .contains("https://pi.dev/install.sh")
    );
    let goose = catalog.lookup("goose").expect("goose entry exists");
    assert_eq!(goose.kind, RegistryKind::Native);
    assert!(goose.headless_compatible);
    assert!(goose.set_provider);
    assert!(goose.set_model);
    assert!(goose.allow_custom_provider);
    assert!(goose.allow_custom_model);
    assert!(goose.set_mode);
    assert!(goose.set_effort);
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
    assert!(codex.set_effort);
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
    assert!(claude_code.set_effort);
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
    assert!(kimi.set_provider);
    assert!(kimi.set_model);
    assert!(kimi.allow_custom_provider);
    assert!(kimi.allow_custom_model);
    assert!(kimi.set_mode);
    assert!(kimi.set_effort);
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
    assert_eq!(hermes.kind, RegistryKind::Adapter);
    assert!(hermes.headless_compatible);
    assert!(hermes.set_provider);
    assert!(hermes.set_model);
    assert!(hermes.allow_custom_provider);
    assert!(hermes.allow_custom_model);
    assert!(hermes.set_mode);
    assert!(!hermes.set_effort);
    assert!(hermes.supports_agent_skills);
    assert_eq!(
        hermes.agent_skills_install_dir.as_deref(),
        Some("~/.agents/skills")
    );
    assert!(!hermes.subagents);
    assert_eq!(hermes.support_doc.as_deref(), Some("docs/agents/hermes.md"));
    assert!(hermes.sync_exempt);
    let hermes_adapter = hermes.adapter.as_ref().expect("Hermes Agent adapter");
    assert_eq!(hermes_adapter.id, "hermes-agent-acp");
    assert_eq!(
        hermes_adapter.github.as_deref(),
        Some("atrium-cloud/hermes-acp")
    );
    let hermes_adapter_shell = hermes_adapter
        .install
        .shell
        .as_ref()
        .expect("Hermes Agent adapter shell install");
    assert!(
        hermes_adapter_shell
            .script
            .contains("nodejs.org/dist/latest-v22.x/")
    );
    assert!(hermes_adapter_shell.script.contains(
        "https://github.com/atrium-cloud/hermes-acp/releases/latest/download/hermes-agent-acp.zip"
    ));
    assert!(hermes_adapter_shell.script.contains("cmp -s"));
    assert_eq!(hermes_adapter_shell.creates, "hermes-agent-acp");
    assert!(hermes_adapter.install.npm.is_none());
    assert!(hermes_adapter.install.github.is_none());
    assert!(hermes_adapter.update.shell_rerun);
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
    // The adapter drives `hermes serve`; nothing beyond the base binary is installed.
    assert!(!hermes_shell.script.contains("'.[acp]'"));
    // The full Python toolchain + virtualenv + checkout fits the shared default,
    // measured at 185s end to end on a fresh 8-core host.
    assert_eq!(hermes_shell.timeout_secs, None);
    let kilo = catalog.lookup("kilo").expect("Kilo Code entry exists");
    assert_eq!(kilo.name, "Kilo Code");
    assert_eq!(kilo.kind, RegistryKind::Native);
    assert!(kilo.headless_compatible);
    assert!(!kilo.set_provider);
    assert!(kilo.set_model);
    assert!(!kilo.allow_custom_provider);
    assert!(!kilo.allow_custom_model);
    assert!(kilo.set_mode);
    assert!(kilo.set_effort);
    assert!(kilo.supports_agent_skills);
    assert_eq!(
        kilo.agent_skills_install_dir.as_deref(),
        Some("~/.agents/skills")
    );
    assert!(!kilo.subagents);
    assert_eq!(kilo.github.as_deref(), Some("Kilo-Org/kilocode"));
    assert_eq!(kilo.support_doc.as_deref(), Some("docs/agents/kilo.md"));
    let kilo_harness = kilo.harness.as_ref().expect("Kilo Code harness");
    assert_eq!(kilo_harness.id, "kilo");
    assert_eq!(kilo_harness.acp_args, ["acp"]);
    let kilo_npm = kilo_harness
        .install
        .npm
        .as_ref()
        .expect("Kilo Code npm install");
    assert_eq!(kilo_npm.package, "@kilocode/cli");
    assert_eq!(kilo_npm.creates, "kilo");
    let antigravity = catalog
        .lookup("antigravity")
        .expect("Google Antigravity entry exists");
    assert_eq!(antigravity.name, "Google Antigravity");
    assert_eq!(antigravity.kind, RegistryKind::Native);
    assert!(antigravity.headless_compatible);
    assert!(!antigravity.set_provider);
    assert!(antigravity.set_model);
    assert!(!antigravity.allow_custom_provider);
    assert!(!antigravity.allow_custom_model);
    assert!(antigravity.set_mode);
    assert!(!antigravity.set_effort);
    assert!(antigravity.supports_agent_skills);
    assert_eq!(
        antigravity.agent_skills_install_dir.as_deref(),
        Some("~/.agents/skills")
    );
    assert!(!antigravity.subagents);
    assert_eq!(antigravity.sync_id.as_deref(), Some("antigravity-acp"));
    assert_eq!(
        antigravity.support_doc.as_deref(),
        Some("docs/agents/antigravity.md")
    );
    let antigravity_harness = antigravity
        .harness
        .as_ref()
        .expect("Google Antigravity harness");
    assert_eq!(antigravity_harness.id, "antigravity");
    // The literal `--uid=` flag is what upstream declares for the Linux targets;
    // an `acp` subcommand would not start the server.
    assert_eq!(antigravity_harness.acp_args, ["--uid="]);
    assert!(
        antigravity_harness.install.npm.is_none(),
        "upstream distributes prebuilt zips only; there is no npm package"
    );
    let antigravity_shell = antigravity_harness
        .install
        .shell
        .as_ref()
        .expect("Google Antigravity shell install");
    assert!(
        antigravity_shell
            .script
            .contains("https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json"),
        "the archive URL must be resolved from the upstream index, not pinned to a dated build tag"
    );
    assert!(antigravity_shell.script.contains("agy_acp_server.par"));
    assert_eq!(antigravity_shell.creates, "antigravity");
    assert_eq!(antigravity_shell.required_tools, ["curl", "unzip", "jq"]);
    assert!(
        antigravity_harness.update.shell_rerun,
        "the .par server has no update subcommand and no npm/github channel; \
         re-running the recipe is the only update path"
    );
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
            matches!(entry.id.as_str(), "pi" | "hermes"),
            "sync_exempt is a narrow escape hatch; `{}` must not carry it",
            entry.id
        );
    }
    for entry in catalog.entries() {
        assert_eq!(
            entry.sync_id.is_some(),
            entry.id == "antigravity",
            "entry-level sync_id exists only for catalog ids that differ from the upstream \
             registry id; `{}` must not carry it",
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
