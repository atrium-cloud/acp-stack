mod args;
mod handoff;
mod headless_snapshot;
mod install;
mod model_mode;
mod native_config;
mod preprocess;
mod prompt;
mod provider;
mod registry_apply;
mod resume;
mod run;
mod serve;
mod skills;
mod starter_config;
mod testflight;

use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand, ValueEnum};
use zeroize::Zeroizing;

use crate::config::{self, AgentSubagentConfig, Config, DataSourceConfig};
use crate::error::{Result, StackError};
use crate::fs_util::{
    acquire_agent_config_mutation_file_lock, atomic_write_owner_only, create_dir_owner_only,
    home_dir, parent_dir, pre_create_owner_only, set_owner_only_file, write_new_file_owner_only,
};
use crate::runtime::agent::agent_headless_config::OPENCODE_AGENT_ID;
use crate::runtime::agent::native_config_import::{
    InspectedNativeConfig, NativeConfigSelection, inspect_native_config,
    validate_native_config_selection,
};
use crate::runtime::dependencies::deps_apply::{
    DepApplyOutcome, apply_dependencies_with_progress, pending_candidates,
};
use crate::runtime::init_runner::{StepDisposition, StepOutcome, record_step, step_kind};
use crate::runtime::install::agent_installer::InstallerOutcome;
use crate::runtime::install::agent_registry::RegistryCatalog;
use crate::runtime::install::skill_installer::SkillInstallReport;
use crate::runtime::install::skill_registry::SkillCatalog;
use crate::secrets::{SecretStore, age_key_path};
use crate::state::{
    INIT_RUN_FAILED, INIT_RUN_SUCCEEDED, INIT_STEP_FAILED, INIT_STEP_PENDING, INIT_STEP_RUNNING,
    INIT_STEP_SKIPPED, INIT_STEP_SUCCEEDED, StateStore, default_state_path,
};

use self::headless_snapshot::{
    capture_dir_listings_for, capture_path_snapshots, headless_config_candidate_paths,
    headless_config_side_dirs, remove_new_files_in_dirs, restore_headless_snapshots,
};
use self::install::{
    MAX_INSTALL_ATTEMPTS, install_configured_agent, local_bin_dir, operator_registry_override,
    run_install_with_retry, should_install_agent,
};
use self::model_mode::{
    ModelModeAction, configure_model_and_mode_for_init, preflight_model_and_mode_for_init,
    verify_agent_acp_connection,
};
use self::provider::{
    apply_provider_to_config, collect_declared_secret_refs_for_init,
    collect_prepared_secret_refs_for_init, configure_provider_for_init,
    configured_provider_refs_satisfied, preflight_provider_for_init,
};
use self::registry_apply::{
    AgentSelection, CustomAgentSpec, apply_custom_agent_to_config, apply_edge_profile_to_config,
    apply_registry_entry_to_config, is_custom_agent, reject_registry_id_for_custom_agent,
    resolve_custom_agent_spec, select_agent_for_init,
};
use self::resume::{
    FreshKeys, KeyPolicy, finalize_with_error, init_complete_event_already_recorded,
    installer_postcondition_holds, perform_auth_init, recorded_init_args, resolve_init_run,
    step_needs_resume, workspace_postcondition_holds,
};
use self::skills::{
    install_init_skills, prompt_init_skills_if_needed, resolve_skill_install_plan,
    skill_install_postcondition_holds,
};
use self::starter_config::{
    AgentEnvCollection, append_agent_env_refs, apply_agent_env_collection,
    collect_agent_env_refs_for_init, configure_stack_update_for_init,
    prompt_environment_configuration_if_needed, push_args_deps_to_config,
    reject_agent_env_refs_for_existing_config, reject_data_source_args_for_existing_config,
    reject_deps_args_for_existing_config, reject_starter_only_mcp_args_for_existing_config,
    should_apply_deps_for_init, starter_config, validate_deployment_overrides_match_existing,
    validate_stack_update_args,
};
use self::testflight::{TestflightDecision, resolve_testflight_decision};
use super::config as cli_config;
use super::logging::{
    SUPABASE_API_KEY_REF_ENV, SUPABASE_DEFAULT_API_KEY_REF, SUPABASE_DEFAULT_SCHEMA,
    SUPABASE_ENABLED_ENV, SUPABASE_SCHEMA_ENV, SUPABASE_URL_ENV, apply_supabase_config,
    disabled_supabase_config, enabled_supabase_config, ensure_supabase_secret,
};

pub(super) use self::args::InitMode;
pub use self::args::{InitArgs, InitCommand};
#[cfg(feature = "dev-tools")]
pub(super) use self::run::run_init;
pub(super) use self::run::run_init_command;

// Cross-seam items keep `pub(super)` visibility in their sibling; re-import them
// here so other siblings can still reach them via `super::` and so this module's
// own body and tests resolve them unqualified.
use self::args::*;
use self::handoff::*;
use self::preprocess::*;
use self::run::{prompts_enabled, run_hosted_init};

pub(super) const STARTER_MAX_REQUEST_BYTES: u64 = 104_857_600;
pub(super) const STARTER_RATE_LIMIT_PER_MINUTE: u64 = 120;
pub(super) const STARTER_RATE_LIMIT_BURST: u64 = 30;
pub(super) const STARTER_AUTH_FAILURES_PER_MINUTE: u64 = 5;
pub(super) const STARTER_AUTH_BLOCK_DURATION: &str = "15m";
pub(super) const STARTER_DEFAULT_SHELL: &str = "/bin/bash";
pub(super) const STARTER_WORKSPACE_MAX_FILE_BYTES: u64 = 8_388_608;
pub(super) const STARTER_LOCAL_RETENTION_DAYS: u64 = 30;
pub(super) const STARTER_LOG_LEVEL: &str = "info";
pub(super) const STARTER_AGENT_ID: &str = "placeholder";
pub(super) const STARTER_AGENT_NAME: &str = "Placeholder Agent";
pub(super) const STARTER_AGENT_COMMAND: &str = "acp-agent";
pub(super) const STARTER_AGENT_RESTART: &str = "never";
pub(super) const STARTER_AGENT_INSTALL_CREATES: &str = "acp-agent";
pub(super) const STARTER_AGENT_INSTALL_TYPE: &str = "shell";
pub(super) const STARTER_AGENT_INSTALL_COMMAND: &str = "true";

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

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

    #[test]
    fn native_upload_rejects_incompatible_provider_before_starter_config_write() {
        let home = tempfile::tempdir().expect("home");
        let config_path = home
            .path()
            .join(".config")
            .join("acp-stack")
            .join("acps-config.toml");
        let args = parse_init_args(&[
            "--non-interactive",
            "--agent",
            "opencode",
            "--provider",
            "azure-openai-responses",
        ]);
        let registry = RegistryCatalog::load_embedded().expect("registry");
        let mut config = crate::config::load_config_from_str(include_str!(
            "../../tests/fixtures/valid-opencode-stack.toml"
        ))
        .expect("config");
        let inspected =
            inspect_native_config("opencode", Some("opencode.json"), r#"{"theme":"dark"}"#)
                .expect("inspect");
        let revision = inspected.revision().to_owned();
        let mut pending = PendingInitNativeConfig {
            inspected,
            selection: NativeConfigSelection {
                revision,
                selected_managed_field_ids: Vec::new(),
                executable_settings_acknowledged: false,
            },
            prepared: None,
        };

        let error = prepare_native_config_for_new_init(
            &args,
            &registry,
            &mut pending,
            &mut config,
            &config_path,
            home.path(),
        )
        .expect_err("unsupported provider");

        assert!(matches!(
            error,
            StackError::InvalidParam {
                field: "provider",
                ..
            }
        ));
        assert!(pending.prepared.is_none());
        assert!(!config_path.exists());
    }

    #[test]
    fn starter_config_writes_interactive_mcp_rows() {
        let mut args = parse_init_args(&[]);
        args.prompt_mcp_stdio.push(InitMcpStdioServer {
            name: "local-tool".to_owned(),
            command: "local-tool-mcp".to_owned(),
            args: vec!["serve".to_owned(), "--verbose".to_owned()],
            env: vec!["LOCAL_TOOL_API_KEY".to_owned()],
        });
        args.prompt_mcp_http.push(InitMcpHttpServer {
            name: "remote".to_owned(),
            url: "https://mcp.example.com".to_owned(),
            headers: vec![InitMcpHttpHeader {
                name: "Authorization".to_owned(),
                value_ref: Some("REMOTE_MCP_TOKEN".to_owned()),
                value: None,
            }],
        });

        let toml = starter_config::starter_config(&args).expect("starter config");
        let config = config::load_config_from_str(&toml).expect("config parses");
        assert_eq!(config.mcp.servers.len(), 2);
        match &config.mcp.servers[0] {
            config::McpServerConfig::Stdio(stdio) => {
                assert_eq!(stdio.name, "local-tool");
                assert_eq!(stdio.command, "local-tool-mcp");
                assert_eq!(stdio.args, ["serve", "--verbose"]);
                assert_eq!(stdio.env, ["LOCAL_TOOL_API_KEY"]);
            }
            other => panic!("expected stdio MCP, got {other:?}"),
        }
        match &config.mcp.servers[1] {
            config::McpServerConfig::Http(http) => {
                assert_eq!(http.name, "remote");
                assert_eq!(http.url, "https://mcp.example.com");
                assert_eq!(http.headers.len(), 1);
                assert_eq!(http.headers[0].name, "Authorization");
                assert_eq!(
                    http.headers[0].value_ref.as_deref(),
                    Some("REMOTE_MCP_TOKEN")
                );
            }
            other => panic!("expected HTTP MCP, got {other:?}"),
        }
    }

    #[test]
    fn starter_config_writes_interactive_s3_data_source() {
        let mut args = parse_init_args(&[]);
        args.prompt_data_sources.push(DataSourceConfig {
            source_type: "s3".to_owned(),
            name: None,
            path: None,
            url: None,
            expected_sha256: None,
            max_download_bytes: None,
            max_extracted_bytes: None,
            bucket: Some("acps-fixtures".to_owned()),
            prefix: Some("datasets".to_owned()),
            region: Some("us-east-1".to_owned()),
            access_key_ref: Some("AWS_ACCESS_KEY_ID".to_owned()),
            secret_key_ref: Some("AWS_SECRET_ACCESS_KEY".to_owned()),
        });

        let toml = starter_config::starter_config(&args).expect("starter config");
        let config = config::load_config_from_str(&toml).expect("config parses");
        assert_eq!(config.workspace.data_sources.len(), 1);
        let source = &config.workspace.data_sources[0];
        assert_eq!(source.source_type, "s3");
        assert_eq!(source.bucket.as_deref(), Some("acps-fixtures"));
        assert_eq!(source.prefix.as_deref(), Some("datasets"));
        assert_eq!(source.region.as_deref(), Some("us-east-1"));
        assert_eq!(source.access_key_ref.as_deref(), Some("AWS_ACCESS_KEY_ID"));
        assert_eq!(
            source.secret_key_ref.as_deref(),
            Some("AWS_SECRET_ACCESS_KEY")
        );
    }
}
