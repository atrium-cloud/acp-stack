use std::io::IsTerminal;
use std::path::Path;

use http::header::HeaderName;

use crate::config::{
    self, AGENT_UPDATE_FREQUENCY_LIMITS, AgentAutoUpdateConfig, AgentConfig, AgentInstallConfig,
    ApiConfig, CodeSourceConfig, Config, DEFAULT_AGENT_AUTO_UPDATE_FREQUENCY, DataSourceConfig,
    DependencyEntry, DependencyInstallAction, DependencyInstallScope, DurationLimits, EdgeConfig,
    HttpHeaderRef, LoggingConfig, McpConfig, McpHttpServer, McpServerConfig, McpStdioServer,
    STACK_UPDATE_FREQUENCY_LIMITS, SandboxConfig, SandboxMode, SecurityConfig, SecurityHttpConfig,
    StackUpdatePolicy, SupabaseLoggingConfig, WorkspaceConfig, is_valid_secret_ref_name,
    normalize_duration,
};
use crate::error::{Result, StackError};
use crate::runtime::dependencies::deps_apply::{
    DepApplyCandidate, PrivilegeEscalation, candidate_summary_line, escalation_notice_lines,
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

// Items the `init` parent consumes escape `starter_config`, so the sibling declares
// them `pub(crate)` and they are re-exported here.
pub(super) use self::builders::{
    mcp_servers_from_prompted, merge_prompted_mcp_servers,
    reject_data_source_args_for_existing_config, reject_extensions_args_for_existing_config,
    reject_sandbox_mask_paths_args_for_existing_config,
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
    configure_agent_update_for_init, configure_stack_update_for_init,
    prompt_environment_configuration_if_needed, prompt_mcp_servers, validate_agent_update_args,
    validate_stack_update_args,
};

// Plain globs make each sibling's `pub(super)` items private members here, so the
// other siblings reach them via `super::NAME`.
use self::builders::*;

#[cfg(test)]
mod tests;
