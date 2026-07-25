use std::io::IsTerminal;
use std::path::Path;

use http::header::HeaderName;

use crate::config::{
    self, AgentConfig, AgentInstallConfig, ApiConfig, CodeSourceConfig, Config, DataSourceConfig,
    DependencyEntry, DependencyInstallAction, DependencyInstallScope, EdgeConfig, HttpHeaderRef,
    LoggingConfig, McpConfig, McpHttpServer, McpServerConfig, McpStdioServer, SandboxConfig,
    SandboxMode, SecurityConfig, SecurityHttpConfig, StackUpdatePolicy, SupabaseLoggingConfig,
    WorkspaceConfig, is_valid_secret_ref_name, normalize_day_or_week_duration,
};
use crate::error::{Result, StackError};
use crate::runtime::dependencies::deps_apply::{
    DepApplyCandidate, candidate_summary_line, summarize_candidates,
};
use crate::runtime::install::agent_registry::RegistryCatalog;
use crate::runtime::install::skill_registry::SkillCatalog;
use crate::secrets::SecretStore;

use super::super::logging::{
    SUPABASE_DEFAULT_API_KEY_REF, SUPABASE_DEFAULT_SCHEMA, disabled_supabase_config,
    enabled_supabase_config,
};
use super::{
    InitArgs, InitMcpHttpHeader, InitMcpHttpServer, InitMcpStdioServer, STARTER_AGENT_COMMAND,
    STARTER_AGENT_ID, STARTER_AGENT_INSTALL_COMMAND, STARTER_AGENT_INSTALL_CREATES,
    STARTER_AGENT_INSTALL_TYPE, STARTER_AGENT_NAME, STARTER_AGENT_RESTART,
    STARTER_AUTH_BLOCK_DURATION, STARTER_AUTH_FAILURES_PER_MINUTE, STARTER_DEFAULT_SHELL,
    STARTER_LOCAL_RETENTION_DAYS, STARTER_LOG_LEVEL, STARTER_MAX_REQUEST_BYTES,
    STARTER_RATE_LIMIT_BURST, STARTER_RATE_LIMIT_PER_MINUTE, STARTER_WORKSPACE_MAX_FILE_BYTES,
    prompt, prompts_enabled,
};

mod builders;
mod deps;
mod prompts;

// Items consumed by the `init` parent module (its `use self::starter_config::{…}`
// list) escape `starter_config`, so they are declared `pub(crate)` in their
// sibling and re-exported here.
pub(super) use self::builders::{
    reject_starter_only_mcp_args_for_existing_config, starter_config,
    validate_deployment_overrides_match_existing,
};
pub(super) use self::deps::{
    AgentEnvCollection, append_agent_env_refs, apply_agent_env_collection,
    collect_agent_env_refs_for_init, push_args_deps_to_config,
    reject_agent_env_refs_for_existing_config, reject_deps_args_for_existing_config,
    should_apply_deps_for_init,
};
pub(super) use self::prompts::{
    configure_stack_update_for_init, prompt_environment_configuration_if_needed,
    validate_stack_update_args,
};

// Plain (non-re-exporting) globs make each sibling's `pub(super)` items private
// members of this parent module, so the other siblings and the `tests` module
// reach them via `super::NAME` / `super::*`.
use self::builders::*;
// The `deps` `pub(super)` consts are referenced only by the `tests` module
// below (via `use super::*`); this glob re-import makes them reachable there, so
// it is gated to the test build to stay warning-clean.
#[cfg(test)]
use self::deps::*;

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use tempfile::tempdir;

    #[derive(Debug, Parser)]
    struct TestInitArgs {
        #[command(flatten)]
        args: InitArgs,
    }

    fn parse_init_args(args: &[&str]) -> InitArgs {
        let mut argv = vec!["init-test"];
        argv.extend_from_slice(args);
        TestInitArgs::parse_from(argv).args
    }

    fn starter_config_from_args(args: &InitArgs) -> Config {
        let raw = starter_config(args).expect("starter config");
        config::load_config_from_str(&raw).expect("starter config validates")
    }

    fn collection(fresh: &[(&str, &str)]) -> AgentEnvCollection {
        AgentEnvCollection {
            flag_refs: Vec::new(),
            fresh: fresh
                .iter()
                .map(|(name, value)| {
                    (
                        (*name).to_owned(),
                        zeroize::Zeroizing::new((*value).to_owned()),
                    )
                })
                .collect(),
        }
    }

    // A fresh agent-env name that collides with a secret already in the store
    // must be rejected before the upsert, leaving the existing secret untouched.
    #[test]
    fn apply_agent_env_refuses_to_overwrite_existing_secret() {
        let home = tempdir().expect("tempdir");
        let mut store = SecretStore::open_or_create(home.path()).expect("store");
        store
            .set("ADMIN_KEY", "original-admin-secret")
            .expect("seed");

        let error =
            apply_agent_env_collection(&mut store, &collection(&[("ADMIN_KEY", "attacker")]))
                .expect_err("collision with an existing secret must be rejected");
        assert!(error.to_string().contains("already exists"), "got: {error}");
        assert_eq!(
            store.get("ADMIN_KEY").expect("preserved"),
            "original-admin-secret",
            "the existing secret must not be overwritten"
        );
    }

    #[test]
    fn apply_agent_env_rejects_invalid_ref_name() {
        let home = tempdir().expect("tempdir");
        let mut store = SecretStore::open_or_create(home.path()).expect("store");

        let error = apply_agent_env_collection(&mut store, &collection(&[("bad-name", "v")]))
            .expect_err("an invalid ref name must be rejected");
        assert!(
            error.to_string().contains("valid secret ref name"),
            "got: {error}"
        );
    }

    #[test]
    fn apply_agent_env_stores_a_new_secret() {
        let home = tempdir().expect("tempdir");
        let mut store = SecretStore::open_or_create(home.path()).expect("store");

        apply_agent_env_collection(&mut store, &collection(&[("GITHUB_TOKEN", "ghp_value")]))
            .expect("a new, valid ref should be stored");
        assert_eq!(store.get("GITHUB_TOKEN").expect("stored"), "ghp_value");
    }

    // Scripts the hosted-prompt driver so the interactive environment-config flow
    // can be exercised headlessly: `selects`/`confirms` are dequeued in call order,
    // and text/password return None so any add-loop finishes immediately.
    struct ScriptedPromptDriver {
        selects: Mutex<VecDeque<Option<usize>>>,
        confirms: Mutex<VecDeque<bool>>,
    }

    impl ScriptedPromptDriver {
        fn new(selects: Vec<Option<usize>>, confirms: Vec<bool>) -> Self {
            Self {
                selects: Mutex::new(VecDeque::from(selects)),
                confirms: Mutex::new(VecDeque::from(confirms)),
            }
        }
    }

    impl prompt::HostedPromptDriver for ScriptedPromptDriver {
        fn select(
            &self,
            _request: prompt::HostedPromptRequest,
        ) -> Result<prompt::HostedPromptOutcome<Option<usize>>> {
            Ok(prompt::HostedPromptOutcome::Handled(
                self.selects
                    .lock()
                    .expect("selects lock")
                    .pop_front()
                    .expect("scripted select"),
            ))
        }

        fn confirm(
            &self,
            _request: prompt::HostedPromptRequest,
        ) -> Result<prompt::HostedPromptOutcome<bool>> {
            Ok(prompt::HostedPromptOutcome::Handled(
                self.confirms
                    .lock()
                    .expect("confirms lock")
                    .pop_front()
                    .expect("scripted confirm"),
            ))
        }

        fn text(
            &self,
            _request: prompt::HostedPromptRequest,
        ) -> Result<prompt::HostedPromptOutcome<Option<String>>> {
            Ok(prompt::HostedPromptOutcome::Handled(None))
        }

        fn password(
            &self,
            _request: prompt::HostedPromptRequest,
        ) -> Result<prompt::HostedPromptOutcome<Option<String>>> {
            Ok(prompt::HostedPromptOutcome::Handled(None))
        }

        fn progress(&self, _message: String) {}

        fn result(&self, _payload: serde_json::Value) {}
    }

    // Models a hosted driver that leaves the environment-config prompt outside its
    // v1 scope: every prompt is Unhandled, so the flow must skip cleanly.
    struct UnhandledPromptDriver;

    impl prompt::HostedPromptDriver for UnhandledPromptDriver {
        fn select(
            &self,
            _request: prompt::HostedPromptRequest,
        ) -> Result<prompt::HostedPromptOutcome<Option<usize>>> {
            Ok(prompt::HostedPromptOutcome::Unhandled)
        }

        fn confirm(
            &self,
            _request: prompt::HostedPromptRequest,
        ) -> Result<prompt::HostedPromptOutcome<bool>> {
            Ok(prompt::HostedPromptOutcome::Unhandled)
        }

        fn text(
            &self,
            _request: prompt::HostedPromptRequest,
        ) -> Result<prompt::HostedPromptOutcome<Option<String>>> {
            Ok(prompt::HostedPromptOutcome::Unhandled)
        }

        fn password(
            &self,
            _request: prompt::HostedPromptRequest,
        ) -> Result<prompt::HostedPromptOutcome<Option<String>>> {
            Ok(prompt::HostedPromptOutcome::Unhandled)
        }

        fn progress(&self, _message: String) {}

        fn result(&self, _payload: serde_json::Value) {}
    }

    fn run_environment_configuration(
        driver: Arc<dyn prompt::HostedPromptDriver>,
        args: &mut InitArgs,
    ) -> Result<()> {
        let registry = RegistryCatalog::load_embedded().expect("registry");
        let skill_catalog = SkillCatalog::load_embedded().expect("skill catalog");
        prompt::with_hosted_driver(driver, || {
            prompt_environment_configuration_if_needed(args, &registry, &skill_catalog)
        })
    }

    // Standard Setup (path index 0): essential deps + browser-use accepted,
    // skills skipped for a non-skills agent, data declined. It must touch none
    // of the Advanced-only seams and must make exactly one select (the path
    // choice) — extra selects would drain the single-item queue and panic.
    #[test]
    fn standard_setup_enables_essential_deps_and_browser_use() {
        let driver = Arc::new(ScriptedPromptDriver::new(
            vec![Some(0)],
            vec![true, true, false],
        ));
        let mut args = parse_init_args(&["--agent", "placebo"]);

        run_environment_configuration(driver, &mut args).expect("standard setup");

        assert!(args.standard_agent_work_deps);
        assert!(args.browser_use_profile);
        assert!(!args.prompt_skills);
        assert!(!args.prompt_agent_env_refs);
        assert!(args.prompt_data_sources.is_empty());
    }

    // Standard Setup with every prompt declined enables nothing.
    #[test]
    fn standard_setup_decline_all_enables_nothing() {
        let driver = Arc::new(ScriptedPromptDriver::new(
            vec![Some(0)],
            vec![false, false, false],
        ));
        let mut args = parse_init_args(&["--agent", "placebo"]);

        run_environment_configuration(driver, &mut args).expect("standard setup");

        assert!(!args.standard_agent_work_deps);
        assert!(!args.browser_use_profile);
        assert!(!args.prompt_skills);
        assert!(args.prompt_data_sources.is_empty());
    }

    #[test]
    fn standard_setup_adds_essential_skills_for_skills_capable_agent() {
        let driver = Arc::new(ScriptedPromptDriver::new(
            vec![Some(0)],
            vec![false, false, true, false],
        ));
        let mut args = parse_init_args(&["--agent", "opencode"]);

        run_environment_configuration(driver, &mut args).expect("standard setup");

        assert!(args.essential_skills);
        assert!(args.skills_source.is_none());
        assert!(args.skills.is_empty());
        assert!(!args.prompt_skills);
        assert!(args.prompt_data_sources.is_empty());
    }

    #[test]
    fn standard_setup_keeps_explicit_skill_flags() {
        let driver = Arc::new(ScriptedPromptDriver::new(
            vec![Some(0)],
            vec![false, false, false],
        ));
        let mut args = parse_init_args(&[
            "--agent",
            "opencode",
            "--skills-source",
            "anthropic",
            "--skills",
            "docx",
        ]);

        run_environment_configuration(driver, &mut args).expect("standard setup");

        assert_eq!(args.skills_source.as_deref(), Some("anthropic"));
        assert_eq!(args.skills, ["docx"]);
        assert!(!args.essential_skills);
    }

    // Advanced Setup (path index 1) with a non-skills agent: deps off, MCP off,
    // agent env on, data off. `placebo` is absent from the embedded registry, so
    // `agent_supports_skills` is false and the skills prompt is skipped — hence
    // four confirms, not five.
    #[test]
    fn advanced_setup_routes_agent_env_without_standard_fields() {
        let driver = Arc::new(ScriptedPromptDriver::new(
            vec![Some(1)],
            vec![false, false, true, false],
        ));
        let mut args = parse_init_args(&["--agent", "placebo"]);

        run_environment_configuration(driver, &mut args).expect("advanced setup");

        assert!(args.prompt_agent_env_refs);
        assert!(!args.prompt_skills);
        assert!(!args.standard_agent_work_deps);
        assert!(!args.browser_use_profile);
    }

    // Advanced Setup offers the skills step only when the agent supports skills;
    // `opencode` does, so accepting it routes into the skills flow.
    #[test]
    fn advanced_setup_routes_agent_skills_for_skills_capable_agent() {
        let driver = Arc::new(ScriptedPromptDriver::new(
            vec![Some(1)],
            vec![false, true, false, false, false],
        ));
        let mut args = parse_init_args(&["--agent", "opencode"]);

        run_environment_configuration(driver, &mut args).expect("advanced setup");

        assert!(args.prompt_skills);
        assert!(!args.prompt_agent_env_refs);
    }

    // A hosted driver that leaves the path prompt Unhandled skips environment
    // configuration instead of failing, matching non-interactive behavior.
    #[test]
    fn unhandled_hosted_prompt_skips_environment_configuration() {
        let driver = Arc::new(UnhandledPromptDriver);
        let mut args = parse_init_args(&["--agent", "placebo"]);

        run_environment_configuration(driver, &mut args).expect("skip is not an error");

        assert!(!args.standard_agent_work_deps);
        assert!(!args.browser_use_profile);
        assert!(!args.prompt_skills);
        assert!(!args.prompt_agent_env_refs);
    }

    #[test]
    fn sandbox_flag_sets_workspace_sandbox_mode() {
        let args = parse_init_args(&["--agent", "placebo", "--sandbox", "unshare"]);
        let config = starter_config_from_args(&args);
        assert_eq!(config.workspace.sandbox.mode, SandboxMode::Unshare);

        let canonical = config.to_canonical_toml().expect("canonical config");
        assert!(
            canonical.contains("[workspace.sandbox]") && canonical.contains("mode = \"unshare\""),
            "expected the sandbox section in the generated config:\n{canonical}"
        );
    }

    #[test]
    fn sandbox_absent_keeps_off_and_omits_section() {
        let args = parse_init_args(&["--agent", "placebo"]);
        let config = starter_config_from_args(&args);
        assert_eq!(config.workspace.sandbox.mode, SandboxMode::Off);

        let canonical = config.to_canonical_toml().expect("canonical config");
        assert!(
            !canonical.contains("[workspace.sandbox]"),
            "an `off` sandbox must not serialize a section:\n{canonical}"
        );
    }

    #[test]
    fn sandbox_flag_rejects_unknown_mode() {
        let args = parse_init_args(&["--agent", "placebo", "--sandbox", "bogus"]);
        let error = starter_config(&args).expect_err("an unknown sandbox mode must be rejected");
        assert!(
            error
                .to_string()
                .contains("expected off|unshare|bwrap|custom"),
            "got: {error}"
        );
    }

    #[test]
    fn sandbox_override_must_match_existing_config() {
        let existing = starter_config_from_args(&parse_init_args(&[
            "--agent",
            "placebo",
            "--sandbox",
            "unshare",
        ]));

        // Re-running with the same value is a no-op, not a conflict.
        let same = parse_init_args(&["--agent", "placebo", "--sandbox", "unshare"]);
        validate_deployment_overrides_match_existing(&same, &existing)
            .expect("a matching sandbox override is accepted");

        // A different value is rejected rather than silently ignored, so an
        // operator cannot believe they enabled a sandbox that stays off.
        let conflict = parse_init_args(&["--agent", "placebo", "--sandbox", "off"]);
        let error = validate_deployment_overrides_match_existing(&conflict, &existing)
            .expect_err("a conflicting sandbox override must be rejected");
        assert!(error.to_string().contains("unshare"), "got: {error}");
    }

    #[test]
    fn standard_setup_profile_declares_base_dependencies_without_build_toolchain() {
        let mut args = parse_init_args(&["--agent", "placebo"]);
        args.standard_agent_work_deps = true;
        let mut config = starter_config_from_args(&args);

        push_args_deps_to_config(&mut config, &args).expect("push standard deps");

        let bundle = config
            .dependencies
            .commands
            .iter()
            .find(|entry| entry.name == STANDARD_AGENT_WORK_BUNDLE_NAME)
            .expect("standard bundle dependency");
        let install = bundle.install.as_ref().expect("bundle install action");
        assert_eq!(bundle.feature.as_deref(), Some(STANDARD_AGENT_WORK_FEATURE));
        assert_eq!(install.scope, DependencyInstallScope::System);
        assert_eq!(
            install.creates.as_deref(),
            Some(STANDARD_AGENT_WORK_BUNDLE_CREATES)
        );
        assert!(install.shell.contains("apt-get install"));
        assert!(
            install
                .shell
                .contains("UV_PYTHON_INSTALL_DIR=/opt/acp-stack/python UV_PYTHON_BIN_DIR=/usr/local/bin uv python install 3.14"),
            "{}",
            install.shell
        );

        for command in [
            "node",
            "npm",
            "python3",
            "python3.14",
            "uv",
            "git",
            "rg",
            "jq",
        ] {
            assert!(
                config
                    .dependencies
                    .commands
                    .iter()
                    .any(|entry| entry.name == command
                        && entry.feature.as_deref() == Some(STANDARD_AGENT_WORK_FEATURE)
                        && entry.install.is_none()),
                "missing command dependency {command}"
            );
        }
        for package in STANDARD_AGENT_WORK_APT_PACKAGES {
            assert!(
                config
                    .dependencies
                    .packages
                    .iter()
                    .any(|entry| entry.name == *package
                        && entry.feature.as_deref() == Some(STANDARD_AGENT_WORK_FEATURE)),
                "missing package dependency {package}"
            );
        }
        for package in BUILD_HEAVY_APT_PACKAGES {
            assert!(
                !config
                    .dependencies
                    .packages
                    .iter()
                    .any(|entry| entry.name == *package),
                "standard setup must not include {package}"
            );
            assert!(
                !install.shell.contains(package),
                "standard install shell must not include {package}"
            );
        }

        let canonical = config.to_canonical_toml().expect("canonical config");
        config::load_config_from_str(&canonical).expect("canonical config validates");
    }

    #[test]
    fn browser_use_profile_declares_dependency_without_generic_mcp_prompt_config() {
        let mut args = parse_init_args(&["--agent", "placebo"]);
        args.browser_use_profile = true;
        let mut config = starter_config_from_args(&args);

        push_args_deps_to_config(&mut config, &args).expect("push browser deps");

        let browser = config
            .dependencies
            .commands
            .iter()
            .find(|entry| entry.name == BROWSER_USE_MCP_COMMAND)
            .expect("browser-use launcher dependency");
        assert_eq!(browser.feature.as_deref(), Some(BROWSER_USE_FEATURE));
        let install = browser.install.as_ref().expect("browser install action");
        assert_eq!(install.scope, DependencyInstallScope::System);
        assert_eq!(install.creates.as_deref(), Some(BROWSER_USE_MCP_COMMAND));
        for required in [
            "apt-get install",
            "chromium",
            "chromium-browser",
            "uv venv --python 3.14",
            "browser-use[core]",
            BROWSER_USE_PREFIX,
            BROWSER_USE_WRAPPER_PATH,
            BROWSER_USE_LAUNCHER_PATH,
            "FastMCP",
            "BROWSER_USE_API_KEY",
            "BROWSER_USE_VENV=\"${BROWSER_USE_VENV:-/opt/acp-stack/browser-use}\"",
            "BROWSER_USE_MCP_SCRIPT=\"${BROWSER_USE_MCP_SCRIPT:-/usr/local/share/acp-stack/browser-use-mcp.py}\"",
            "exec \"${BROWSER_USE_VENV}/bin/python\"",
        ] {
            assert!(
                install.shell.contains(required),
                "browser install shell must include {required}"
            );
        }
        assert!(config.mcp.servers.is_empty());

        let canonical = config.to_canonical_toml().expect("canonical config");
        config::load_config_from_str(&canonical).expect("canonical config validates");
    }
}
