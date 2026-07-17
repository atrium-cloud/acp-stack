mod check;
mod config;
mod default;
mod install;
mod set;
mod status;
mod switch;
mod test;
mod update;

use clap::{Args, Subcommand};

use super::core::OutputFormatChoice;
use crate::error::Result;

pub(in crate::cli) use self::install::operator_registry_override;
pub(in crate::cli) use self::install::run_agent_restart;
pub(in crate::cli) use self::set::{
    agent_model_is_explicit_without_discovery, default_api_key_ref_for_agent_provider,
    default_custom_provider_api, model_values_for_cli_display, parse_custom_provider_api,
    parse_custom_token_limit, print_agent_set_effective_notice_for, required_custom_arg,
    resolve_agent_model_value, validate_agent_session_config_value,
    validate_custom_provider_api_for_agent,
};
pub(in crate::cli) use self::test::run_init_testflight;

pub(super) const DEFAULT_AGENT_TEST_PROMPT: &str =
    "Reply with exactly this text and nothing else: acp-stack test ok";
pub(super) const DEFAULT_AGENT_TEST_TIMEOUT: &str = "60s";
pub(super) const DEFAULT_AGENT_TEST_PROGRESS_TIMEOUT: &str = "30s";

#[derive(Debug, Subcommand)]
pub enum AgentCommand {
    /// Install the configured ACP agent or adapter.
    Install(AgentInstallArgs),
    /// Ask the running daemon to start the configured agent.
    Start(AgentDaemonArgs),
    /// Ask the running daemon to stop the configured agent.
    Stop(AgentDaemonArgs),
    /// Ask the running daemon to restart the configured agent.
    Restart(AgentRestartArgs),
    /// Print the latest persisted agent state from SQLite.
    Status,
    /// Report whether the installed managed harness/adapter is stale against upstream.
    Check,
    /// Update the configured agent or manage automatic update settings.
    Update(AgentUpdateArgs),
    /// Start the configured agent and send a real ACP prompt.
    Test(AgentTestArgs),
    /// Set the provider id, model, and API-key ref used by generated agent config.
    Set(AgentSetArgs),
    /// Inspect or import the configured harness's native global config.
    Config(AgentConfigArgs),
    /// Switch to another supported agent harness.
    Switch(AgentSwitchArgs),
    /// Select the default Array target for unqualified agent and session commands.
    Default(AgentDefaultArgs),
}

#[derive(Debug, Args)]
pub struct AgentConfigArgs {
    #[command(subcommand)]
    pub(super) command: AgentConfigCommand,
}

#[derive(Debug, Subcommand)]
pub enum AgentConfigCommand {
    /// Inspect a native global config without returning any field values.
    Inspect(AgentConfigInspectArgs),
    /// Inspect and semantically replace the native global config.
    Import(AgentConfigImportArgs),
}

#[derive(Debug, Args)]
pub struct AgentConfigInspectArgs {
    pub(super) path: std::path::PathBuf,
    /// Admin API key. If omitted on a TTY, prompts without echo.
    #[arg(long = "admin-key")]
    pub(super) admin_key: Option<String>,
}

#[derive(Debug, Args)]
pub struct AgentConfigImportArgs {
    pub(super) path: std::path::PathBuf,
    /// Import one compatible managed field id from the inspection. Repeatable.
    #[arg(long = "managed-field", value_name = "ID")]
    pub(super) managed_fields: Vec<String>,
    /// Acknowledge unmanaged settings that can run commands or load code.
    #[arg(long = "ack-executable-settings")]
    pub(super) acknowledge_executable_settings: bool,
    /// Admin API key. If omitted on a TTY, prompts without echo.
    #[arg(long = "admin-key")]
    pub(super) admin_key: Option<String>,
}

#[derive(Debug, Args)]
pub struct AgentDefaultArgs {
    #[command(subcommand)]
    pub(super) command: AgentDefaultCommand,
}

#[derive(Debug, Subcommand)]
pub enum AgentDefaultCommand {
    /// Set the default target by canonical agent id.
    Set(AgentDefaultSetArgs),
}

#[derive(Debug, Args)]
pub struct AgentDefaultSetArgs {
    /// Configured Array target id, which is the canonical agent id.
    pub(super) agent: String,
}

#[derive(Debug, Args)]
pub struct AgentTestArgs {
    /// Prompt text to send. Defaults to a minimal compatibility prompt.
    #[arg(long)]
    pub(super) prompt: Option<String>,
    /// Maximum time to wait for the prompt request to finish.
    #[arg(long, default_value = DEFAULT_AGENT_TEST_TIMEOUT)]
    pub(super) timeout: String,
    /// Maximum time to wait for either progress or terminal prompt completion.
    #[arg(long = "progress-timeout", default_value = DEFAULT_AGENT_TEST_PROGRESS_TIMEOUT)]
    pub(super) progress_timeout: String,
}

#[derive(Debug, Args)]
pub struct AgentInstallArgs {
    /// Accepted for script consistency; install is already non-interactive.
    #[arg(long)]
    pub(super) yes: bool,
    /// Admin API key. Required when stdin is not a terminal.
    #[arg(long = "admin-key")]
    pub(super) admin_key: Option<String>,
}

#[derive(Debug, Args)]
pub struct AgentDaemonArgs {
    /// Admin API key. Required when stdin is not a terminal.
    #[arg(long = "admin-key")]
    pub(super) admin_key: Option<String>,
}

#[derive(Debug, Args)]
pub struct AgentRestartArgs {
    /// Admin API key. Required when stdin is not a terminal.
    #[arg(long = "admin-key", global = true)]
    pub(super) admin_key: Option<String>,
    #[command(subcommand)]
    pub(super) command: Option<AgentRestartCommand>,
}

#[derive(Debug, Subcommand)]
pub enum AgentRestartCommand {
    /// Queue a restart that runs when active ACP sessions are idle.
    Auto,
}

#[derive(Debug, Args)]
pub struct AgentUpdateArgs {
    /// Run update steps even when recorded versions already match upstream.
    #[arg(long)]
    pub(super) force: bool,
    /// If the daemon has a running agent, stop it before update and start it afterwards.
    #[arg(long)]
    pub(super) restart: bool,
    /// Admin API key. Required with --restart when stdin is not a terminal.
    #[arg(long = "admin-key", global = true)]
    pub(super) admin_key: Option<String>,
    #[command(subcommand)]
    pub(super) command: Option<AgentUpdateSubcommand>,
}

#[derive(Debug, Subcommand)]
pub enum AgentUpdateSubcommand {
    /// Configure automatic update settings without running an update.
    Set(AgentUpdateSetArgs),
}

#[derive(Debug, Args)]
pub struct AgentUpdateSetArgs {
    /// Enable periodic agent auto-update.
    #[arg(long = "auto-on", conflicts_with = "auto_off")]
    pub(super) auto_on: bool,
    /// Disable periodic agent auto-update.
    #[arg(long = "auto-off")]
    pub(super) auto_off: bool,
    /// Set the auto-update frequency, such as 1d, 3d, or 4w.
    #[arg(long)]
    pub(super) frequency: Option<String>,
}

#[derive(Debug, Args)]
pub struct AgentSetArgs {
    /// Configure a provider/model outside the embedded provider mapping.
    #[arg(long)]
    pub(super) custom_provider: bool,
    /// Provider id, such as opencode-go, openai, or anthropic.
    #[arg(long)]
    pub(super) provider: Option<String>,
    /// Display name for a custom provider.
    #[arg(long = "provider-name")]
    pub(super) provider_name: Option<String>,
    /// Base URL for a custom provider.
    #[arg(long = "base-url")]
    pub(super) base_url: Option<String>,
    /// API family for a custom provider: chat-completions, responses, or anthropic-messages.
    #[arg(long = "provider-api")]
    pub(super) provider_api: Option<String>,
    /// Provider-qualified model id or model pattern.
    #[arg(long)]
    pub(super) model: Option<String>,
    /// Display name for a custom model.
    #[arg(long = "model-name")]
    pub(super) model_name: Option<String>,
    /// Context window in tokens for a custom model.
    #[arg(long)]
    pub(super) context: Option<String>,
    /// Maximum output tokens for a custom model.
    #[arg(long = "output-max-tokens")]
    pub(super) output_max_tokens: Option<String>,
    /// Agent session mode for agents that expose mode as an ACP config option.
    #[arg(long)]
    pub(super) mode: Option<String>,
    /// Secret ref to inject for this provider. Defaults from provider metadata.
    #[arg(long)]
    pub(super) api_key_ref: Option<String>,
}

#[derive(Debug, Args)]
pub struct AgentSwitchArgs {
    /// Target agent id, such as opencode, pi, goose, codex, cursor, amp, or kimi.
    pub(super) agent: String,
    /// Drop source agent-owned config after a successful switch.
    #[arg(long = "drop")]
    pub(super) drop_configs: bool,
    /// Provider id to use instead of attempting compatible reuse.
    #[arg(long)]
    pub(super) provider: Option<String>,
    /// Secret ref to inject for the target provider.
    #[arg(long = "api-key-ref")]
    pub(super) api_key_ref: Option<String>,
    /// Admin API key. Required when stdin is not a terminal.
    #[arg(long = "admin-key")]
    pub(super) admin_key: Option<String>,
}

pub(super) fn run_agent_command(command: AgentCommand, output: OutputFormatChoice) -> Result<()> {
    match command {
        AgentCommand::Install(args) => self::install::run_agent_install(args, output.effective()),
        AgentCommand::Start(args) => self::install::run_agent_daemon_post(
            args,
            "/v1/agent/start",
            "start",
            output.effective(),
        ),
        AgentCommand::Stop(args) => {
            self::install::run_agent_daemon_post(args, "/v1/agent/stop", "stop", output.effective())
        }
        AgentCommand::Restart(args) => self::install::run_agent_restart(args, output.effective()),
        AgentCommand::Status => self::status::run_agent_status(output.effective()),
        AgentCommand::Check => self::check::run_agent_check(output.effective()),
        AgentCommand::Update(args) => self::update::run_agent_update(args, output.effective()),
        AgentCommand::Test(args) => {
            output.reject_json("agent test")?;
            self::test::run_agent_test(args)
        }
        AgentCommand::Set(args) => {
            output.reject_json("agent set")?;
            self::set::run_agent_set(args)
        }
        AgentCommand::Config(args) => self::config::run_agent_config(args, output.effective()),
        AgentCommand::Switch(args) => {
            output.reject_json("agent switch")?;
            self::switch::run_agent_switch(args)
        }
        AgentCommand::Default(args) => self::default::run_agent_default(args, output.effective()),
    }
}

#[cfg(test)]
mod tests {
    use super::check::{
        AgentCheckStatus, LatestVersionResolver, agent_check_has_failure, build_agent_check_report,
        compare_versions,
    };
    use super::set::default_api_key_ref_for_agent_provider;
    use super::test::{prepare_testflight_expect_fs, verify_testflight_expect_fs};

    use crate::error::{Result, StackError};
    use crate::runtime::install::agent_registry::RegistryEntry;
    use tempfile::TempDir;

    #[test]
    fn opencode_cloudflare_gateway_defaults_to_token_ref() {
        assert_eq!(
            default_api_key_ref_for_agent_provider("opencode", "cloudflare-ai-gateway"),
            Some("CLOUDFLARE_API_TOKEN".to_owned())
        );
        assert_eq!(
            default_api_key_ref_for_agent_provider("pi", "cloudflare-ai-gateway"),
            Some("CLOUDFLARE_API_KEY".to_owned())
        );
    }

    struct MockResolver {
        npm: std::collections::HashMap<String, String>,
        github: std::collections::HashMap<String, String>,
    }

    impl MockResolver {
        fn new() -> Self {
            Self {
                npm: std::collections::HashMap::new(),
                github: std::collections::HashMap::new(),
            }
        }
        fn with_npm(mut self, package: &str, version: &str) -> Self {
            self.npm.insert(package.to_owned(), version.to_owned());
            self
        }
        #[allow(dead_code)]
        fn with_github(mut self, repo: &str, version: &str) -> Self {
            self.github.insert(repo.to_owned(), version.to_owned());
            self
        }
    }

    impl LatestVersionResolver for MockResolver {
        fn npm(&self, package: &str) -> Result<String> {
            self.npm
                .get(package)
                .cloned()
                .ok_or_else(|| StackError::NpmRegistryEmptyVersion {
                    package: package.to_owned(),
                })
        }
        fn github(&self, repo: &str) -> Result<String> {
            self.github
                .get(repo)
                .cloned()
                .ok_or_else(|| StackError::AgentRegistryMissing {
                    id: repo.to_owned(),
                })
        }
    }

    fn installer_row(step: &str, version: Option<&str>) -> crate::state::InstallerRun {
        crate::state::InstallerRun {
            id: format!("ins_{step}"),
            agent_id: Some("test-agent".to_owned()),
            started_at: "2026-05-22T00:00:00.000000000Z".to_owned(),
            finished_at: Some("2026-05-22T00:00:01.000000000Z".to_owned()),
            status: "ran".to_owned(),
            stdout: String::new(),
            stderr: String::new(),
            exit_status: Some(0),
            step: step.to_owned(),
            version: version.map(str::to_owned),
            operation: crate::state::INSTALLER_OPERATION_INSTALL.to_owned(),
            method: None,
            log_dir: None,
            apply_run_id: None,
        }
    }

    #[test]
    fn compare_versions_normalizes_leading_v() {
        let status = compare_versions("v1.2.3", Some("1.2.3"));
        assert!(matches!(
            status,
            AgentCheckStatus::UpToDate { ref version } if version == "v1.2.3"
        ));
    }

    #[test]
    fn compare_versions_flags_drift() {
        let status = compare_versions("1.0.0", Some("2.0.0"));
        assert!(matches!(
            status,
            AgentCheckStatus::Stale {
                ref installed,
                ref latest,
            } if installed == "1.0.0" && latest == "2.0.0"
        ));
    }

    #[test]
    fn compare_versions_returns_unknown_when_upstream_missing() {
        let status = compare_versions("1.0.0", None);
        assert!(matches!(status, AgentCheckStatus::Unknown { .. }));
    }

    fn embedded_entry(id: &str) -> RegistryEntry {
        crate::runtime::install::agent_registry::RegistryCatalog::load_embedded()
            .expect("registry embeds")
            .lookup(id)
            .expect("entry exists")
            .clone()
    }

    #[test]
    fn build_agent_check_report_returns_stale_for_codex_adapter() {
        // Codex declares npm for both harness (`@openai/codex`) and adapter
        // (`@zed-industries/codex-acp`). The install-path resolver prefers npm
        // when both are present, so the mock provides both.
        let entry = embedded_entry("codex");
        let resolver = MockResolver::new()
            .with_npm("@openai/codex", "rust-v999.0.0")
            .with_npm("@zed-industries/codex-acp", "9.9.9");
        let rows = vec![
            installer_row("harness", Some("rust-v0.50.0")),
            installer_row("adapter", Some("0.1.0")),
        ];
        let report = build_agent_check_report(&entry, &rows, &resolver);
        assert_eq!(report.len(), 2);
        // harness: npm version drift -> stale
        assert!(matches!(
            &report[0],
            (step, AgentCheckStatus::Stale { .. }) if step == "harness"
        ));
        // adapter: npm version drift -> stale
        assert!(matches!(
            &report[1],
            (step, AgentCheckStatus::Stale { .. }) if step == "adapter"
        ));
        assert!(agent_check_has_failure(&report));
    }

    #[test]
    fn build_agent_check_report_returns_up_to_date_when_versions_match() {
        let entry = embedded_entry("codex");
        let resolver = MockResolver::new()
            .with_npm("@openai/codex", "rust-v0.50.0")
            .with_npm("@zed-industries/codex-acp", "0.1.0");
        let rows = vec![
            installer_row("harness", Some("rust-v0.50.0")),
            installer_row("adapter", Some("0.1.0")),
        ];
        let report = build_agent_check_report(&entry, &rows, &resolver);
        assert!(matches!(
            &report[0],
            (step, AgentCheckStatus::UpToDate { .. }) if step == "harness"
        ));
        assert!(matches!(
            &report[1],
            (step, AgentCheckStatus::UpToDate { .. }) if step == "adapter"
        ));
    }

    #[test]
    fn build_agent_check_report_skips_adapter_provided_harness() {
        let entry = embedded_entry("claude-code");
        let resolver =
            MockResolver::new().with_npm("@agentclientprotocol/claude-agent-acp", "1.2.3");
        let rows = vec![installer_row("adapter", Some("1.2.3"))];
        let report = build_agent_check_report(&entry, &rows, &resolver);

        assert_eq!(report.len(), 1);
        assert!(matches!(
            &report[0],
            (step, AgentCheckStatus::UpToDate { .. }) if step == "adapter"
        ));
    }

    #[test]
    fn build_agent_check_report_marks_resolver_errors_as_unknown() {
        let entry = embedded_entry("codex");
        // No mock entries -> resolver errors -> report should mark each step
        // as Unknown rather than crash the whole report.
        let resolver = MockResolver::new();
        let rows = vec![installer_row("adapter", Some("0.1.0"))];
        let report = build_agent_check_report(&entry, &rows, &resolver);
        let adapter = report
            .iter()
            .find(|(step, _)| step == "adapter")
            .expect("adapter report");
        assert!(matches!(adapter, (_, AgentCheckStatus::Unknown { .. })));
    }

    #[test]
    fn build_agent_check_report_returns_unknown_for_shell_native_without_version() {
        let entry = embedded_entry("cursor");
        let resolver = MockResolver::new();
        let rows = vec![installer_row("install", None)];
        let report = build_agent_check_report(&entry, &rows, &resolver);
        assert_eq!(report.len(), 1);
        assert!(matches!(
            &report[0],
            (step, AgentCheckStatus::Unknown { .. }) if step == "install"
        ));
        assert!(!agent_check_has_failure(&report));
    }

    #[test]
    fn build_agent_check_report_marks_missing_adapter_not_installed() {
        let entry = embedded_entry("amp");
        let resolver = MockResolver::new();
        let rows = vec![installer_row("harness", None)];
        let report = build_agent_check_report(&entry, &rows, &resolver);
        assert_eq!(report.len(), 2);
        assert!(matches!(
            &report[0],
            (step, AgentCheckStatus::Unknown { .. }) if step == "harness"
        ));
        assert!(matches!(
            &report[1],
            (step, AgentCheckStatus::NotInstalled) if step == "adapter"
        ));
        assert!(agent_check_has_failure(&report));
    }

    #[test]
    fn build_agent_check_report_marks_missing_native_install_not_installed() {
        let entry = embedded_entry("cursor");
        let resolver = MockResolver::new();
        let report = build_agent_check_report(&entry, &[], &resolver);
        assert_eq!(report.len(), 1);
        assert!(matches!(
            &report[0],
            (step, AgentCheckStatus::NotInstalled) if step == "install"
        ));
        assert!(agent_check_has_failure(&report));
    }

    #[test]
    fn build_agent_check_report_unknown_when_queryable_version_was_not_recorded() {
        let entry = embedded_entry("codex");
        let resolver = MockResolver::new()
            .with_npm("@openai/codex", "rust-v0.50.0")
            .with_npm("@zed-industries/codex-acp", "0.1.0");
        let rows = vec![
            installer_row("harness", Some("rust-v0.50.0")),
            installer_row("adapter", None),
        ];
        let report = build_agent_check_report(&entry, &rows, &resolver);
        assert!(matches!(
            &report[1],
            (step, AgentCheckStatus::Unknown { reason }) if step == "adapter"
                && reason.contains("installed version was not recorded")
        ));
        assert!(!agent_check_has_failure(&report));
    }

    #[test]
    fn verify_testflight_expect_fs_succeeds_for_non_empty_file_under_workspace() {
        let workspace = TempDir::new().expect("tempdir");
        let target = workspace.path().join("marker.txt");
        std::fs::write(&target, b"ok\n").expect("write");
        let outcome =
            verify_testflight_expect_fs(workspace.path(), "marker.txt").expect("verify ok");
        assert_eq!(outcome.path, target);
        assert_eq!(outcome.bytes, 3);
    }

    #[test]
    fn verify_testflight_expect_fs_fails_when_file_missing() {
        let workspace = TempDir::new().expect("tempdir");
        let err = verify_testflight_expect_fs(workspace.path(), "missing.txt")
            .expect_err("missing file must fail");
        match err {
            StackError::AgentTestFailed { stage, reason } => {
                assert_eq!(stage, "fs_check");
                assert!(reason.contains("stat failed"), "reason: {reason}");
            }
            other => panic!("expected AgentTestFailed, got {other:?}"),
        }
    }

    #[test]
    fn verify_testflight_expect_fs_fails_on_empty_file() {
        let workspace = TempDir::new().expect("tempdir");
        let target = workspace.path().join("empty.txt");
        std::fs::write(&target, b"").expect("write");
        let err = verify_testflight_expect_fs(workspace.path(), "empty.txt")
            .expect_err("empty file must fail");
        assert!(matches!(err, StackError::AgentTestFailed { .. }));
    }

    #[test]
    fn verify_testflight_expect_fs_rejects_absolute_path_argument() {
        let workspace = TempDir::new().expect("tempdir");
        let err = verify_testflight_expect_fs(workspace.path(), "/etc/passwd")
            .expect_err("absolute path must be rejected");
        match err {
            StackError::AgentTestFailed { reason, .. } => {
                assert!(reason.contains("workspace-relative"), "reason: {reason}");
            }
            other => panic!("expected AgentTestFailed, got {other:?}"),
        }
    }

    #[test]
    fn verify_testflight_expect_fs_rejects_parent_traversal() {
        let workspace = TempDir::new().expect("tempdir");
        let err = verify_testflight_expect_fs(workspace.path(), "sub/../escape.txt")
            .expect_err("`..` segment must be rejected");
        assert!(matches!(err, StackError::AgentTestFailed { .. }));
    }

    #[test]
    fn prepare_testflight_expect_fs_removes_stale_regular_file() {
        let workspace = TempDir::new().expect("tempdir");
        let target = workspace.path().join("marker.txt");
        std::fs::write(&target, b"old\n").expect("write");
        prepare_testflight_expect_fs(workspace.path(), "marker.txt").expect("prepare ok");
        assert!(!target.exists(), "stale marker should be removed");
    }

    #[cfg(unix)]
    #[test]
    fn prepare_testflight_expect_fs_rejects_preexisting_symlink() {
        let workspace = TempDir::new().expect("tempdir");
        let outside = TempDir::new().expect("outside tempdir");
        let outside_file = outside.path().join("marker.txt");
        std::fs::write(&outside_file, b"outside\n").expect("write outside");
        std::os::unix::fs::symlink(&outside_file, workspace.path().join("marker.txt"))
            .expect("symlink");

        let err = prepare_testflight_expect_fs(workspace.path(), "marker.txt")
            .expect_err("symlink marker must fail");
        match err {
            StackError::AgentTestFailed { reason, .. } => {
                assert!(reason.contains("symlink"), "reason: {reason}");
            }
            other => panic!("expected AgentTestFailed, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn verify_testflight_expect_fs_rejects_parent_symlink_escape() {
        let workspace = TempDir::new().expect("tempdir");
        let outside = TempDir::new().expect("outside tempdir");
        std::fs::write(outside.path().join("marker.txt"), b"outside\n").expect("write outside");
        std::os::unix::fs::symlink(outside.path(), workspace.path().join("linked"))
            .expect("symlink");

        let err = verify_testflight_expect_fs(workspace.path(), "linked/marker.txt")
            .expect_err("canonical escape must fail");
        match err {
            StackError::AgentTestFailed { reason, .. } => {
                assert!(reason.contains("outside workspace"), "reason: {reason}");
            }
            other => panic!("expected AgentTestFailed, got {other:?}"),
        }
    }
}
