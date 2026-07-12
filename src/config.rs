//! Config root: aggregator `Config` struct, top-level constants, the raw
//! deserialization shim, and the public entry points (`load_config_from_str`,
//! `default_config_path`, `Config::*`).
//!
//! The TOML schema types live in `config/schema.rs`; per-domain validators
//! live under `config/validate/`. Both are re-exported here so callers can
//! continue to write `use crate::config::{AgentConfig, Config, ...}` as
//! they always have — the split is internal.

mod schema;
mod validate;

use crate::error::{Result, StackError};
use serde::Deserialize;
use std::env;
use std::path::{Path, PathBuf};

pub use self::schema::{
    AgentAdapterConfig, AgentAutoUpdateConfig, AgentConfig, AgentCustomProviderConfig,
    AgentInstallConfig, AgentProviderConfig, AgentSubagentConfig, ApiConfig, ArrayConfig,
    ArrayTargetConfig, CloudflareEdgeConfig, CodeSourceConfig, CommandsConfig, CustomProviderApi,
    DEFAULT_AGENT_AUTO_UPDATE_FREQUENCY, DEFAULT_COMMAND_PROGRESS_INTERVAL,
    DEFAULT_CUSTOM_MODEL_CONTEXT, DEFAULT_CUSTOM_MODEL_OUTPUT_MAX_TOKENS,
    DEFAULT_PERMISSION_REQUEST_TIMEOUT, DEFAULT_PERMISSION_TIMEOUT_ACTION,
    DEFAULT_PROMPTS_STALE_THRESHOLD, DEFAULT_PROMPTS_SWEEP_INTERVAL,
    DEFAULT_STACK_UPDATE_FREQUENCY, DEFAULT_STACK_UPDATE_POLICY, DataSourceConfig,
    DependenciesConfig, DependencyEntry, DependencyInstallAction, DependencyInstallScope,
    EdgeConfig, HttpHeaderRef, LocalConfig, LocalSessionAuth, LoggingConfig, McpConfig,
    McpHttpServer, McpServerConfig, McpStdioServer, PermissionTimeoutAction, PermissionsConfig,
    PromptsConfig, SandboxConfig, SandboxMode, SecurityConfig, SecurityHttpConfig,
    StackUpdateConfig, StackUpdatePolicy, SupabaseLoggingBackend, SupabaseLoggingConfig,
    UpdatesConfig, WorkspaceConfig,
};
pub(crate) use self::validate::primitives::normalize_day_or_week_duration;
pub use self::validate::primitives::{is_valid_secret_ref_name, parse_duration_string};
pub(crate) use self::validate::sources::{derive_code_source_name, derive_data_source_name};
pub(crate) use self::validate::validate_supabase_identifiers;

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_config_version")]
    pub config_version: u64,
    pub api: ApiConfig,
    pub security: SecurityConfig,
    #[serde(default, skip_serializing_if = "EdgeConfig::is_empty")]
    pub edge: EdgeConfig,
    #[serde(default)]
    pub updates: UpdatesConfig,
    pub workspace: WorkspaceConfig,
    pub logging: LoggingConfig,
    #[serde(skip_serializing)]
    pub agent: AgentConfig,
    pub array: ArrayConfig,
    #[serde(default)]
    pub permissions: PermissionsConfig,
    #[serde(default)]
    pub commands: CommandsConfig,
    #[serde(default)]
    pub prompts: PromptsConfig,
    #[serde(default)]
    pub dependencies: DependenciesConfig,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub local: LocalConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LegacyAuthConfig {
    pub(crate) session_key_ref: String,
    pub(crate) admin_key_ref: String,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LoadedConfig {
    pub(crate) config: Config,
    pub(crate) legacy_auth: Option<LegacyAuthConfig>,
}

pub const SUPPORTED_CONFIG_VERSION: u64 = 1;

pub const IMPORT_SIZE_LIMIT: usize = 1_048_576;
/// JSON transport allowance for one 1 MiB config document plus worst-case
/// `\u00xx` escaping and the small typed request envelope.
pub const IMPORT_REQUEST_SIZE_LIMIT: usize = (IMPORT_SIZE_LIMIT * 6) + (16 * 1024);

/// Default loopback API bind shared by starter config and deployment packaging.
pub const DEFAULT_API_BIND: &str = "127.0.0.1:7700";

/// Default workspace root shared by starter config, Docker, and systemd packaging.
pub const DEFAULT_WORKSPACE_ROOT: &str = "/workspace";

/// Default uploads directory under the deployment-managed workspace root.
pub const DEFAULT_WORKSPACE_UPLOADS: &str = "/workspace/uploads";

/// Default unprivileged Linux runtime user for self-hosted deployments.
pub const DEFAULT_RUNTIME_USER: &str = "acp";

fn default_config_version() -> u64 {
    SUPPORTED_CONFIG_VERSION
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    config_version: Option<u64>,
    api: Option<ApiConfig>,
    security: Option<RawSecurityConfig>,
    #[serde(default)]
    edge: Option<EdgeConfig>,
    #[serde(default)]
    updates: Option<UpdatesConfig>,
    workspace: Option<WorkspaceConfig>,
    logging: Option<LoggingConfig>,
    agent: Option<AgentConfig>,
    #[serde(default)]
    array: Option<ArrayConfig>,
    #[serde(default)]
    permissions: Option<PermissionsConfig>,
    #[serde(default)]
    commands: Option<CommandsConfig>,
    #[serde(default)]
    prompts: Option<PromptsConfig>,
    #[serde(default)]
    dependencies: Option<DependenciesConfig>,
    #[serde(default)]
    mcp: Option<McpConfig>,
    #[serde(default)]
    local: Option<LocalConfig>,
    #[serde(default)]
    auth: Option<LegacyAuthConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSecurityConfig {
    http: Option<SecurityHttpConfig>,
}

impl Config {
    pub fn load_from_default_path() -> Result<Self> {
        Self::load_from_path(default_config_path()?)
    }

    pub(crate) fn load_from_default_path_with_legacy() -> Result<LoadedConfig> {
        Self::load_from_path_with_legacy(default_config_path()?)
    }

    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Self> {
        Ok(Self::load_from_path_with_legacy(path)?.config)
    }

    pub(crate) fn load_from_path_with_legacy(path: impl AsRef<Path>) -> Result<LoadedConfig> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path).map_err(|source| StackError::ConfigRead {
            path: path.to_path_buf(),
            source,
        })?;

        load_config_from_str_with_legacy(&content)
    }

    pub fn to_canonical_toml(&self) -> Result<String> {
        let mut canonical = self.clone();
        if let Some(primary_index) = canonical
            .array
            .targets
            .iter()
            .position(|target| target.id == canonical.array.primary_target)
        {
            canonical.array.primary_target = canonical.agent.id.clone();
            canonical.array.targets[primary_index].id = canonical.agent.id.clone();
            let primary = &mut canonical.array.targets[primary_index];
            primary.agent = canonical.agent.clone();
        }
        Ok(toml::to_string_pretty(&canonical)?)
    }

    fn validate(&self) -> Result<()> {
        self::validate::validate_config(self)
    }
}

fn has_legacy_workspace_source_table(input: &str) -> bool {
    // Cheap line-prefix scan; a substring match would false-positive on
    // values that happen to contain the literal string. We do not need to
    // be exact — we only want a friendly hint for the common case.
    input.lines().any(|line| {
        let trimmed = line.trim_start();
        trimmed.starts_with("[workspace.source]") || trimmed.starts_with("[workspace.source.")
    })
}

fn has_removed_startup_table(input: &str) -> bool {
    input.lines().any(|line| {
        let trimmed = line.trim_start();
        trimmed.starts_with("[startup]")
            || trimmed.starts_with("[startup.")
            || trimmed.starts_with("[[startup.")
    })
}

pub fn default_config_path() -> Result<PathBuf> {
    let home = env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .ok_or(StackError::HomeNotSet)?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("acp-stack")
        .join("acps-config.toml"))
}

pub fn load_config_from_str(input: &str) -> Result<Config> {
    Ok(load_config_from_str_with_legacy(input)?.config)
}

pub(crate) fn load_config_from_str_with_legacy(input: &str) -> Result<LoadedConfig> {
    // Phase 4 removed the legacy single `[workspace.source]` block in favor
    // of `[[workspace.code_sources]]` / `[[workspace.data_sources]]`. The
    // serde error for an unknown field is correct but unhelpful for
    // operators upgrading an older config. Surface a targeted message
    // pointing at the migration path before serde gives them
    // `unknown field`.
    if has_legacy_workspace_source_table(input) {
        return Err(StackError::InvalidParam {
            field: "workspace.source",
            reason: "`[workspace.source]` was removed in Phase 4; declare \
                 `[[workspace.code_sources]]` for git repositories or \
                 `[[workspace.data_sources]]` for local/https/s3 inputs (see docs/specs/config.md)"
                .to_owned(),
        });
    }
    if has_removed_startup_table(input) {
        return Err(StackError::InvalidParam {
            field: "startup",
            reason: "`[startup]` was removed because startup scripts were never executed; use workspace sources, dependency declarations, or agent install configuration instead"
                .to_owned(),
        });
    }
    let raw: RawConfig = toml::from_str(input)?;
    if let Some(auth) = raw.auth.as_ref() {
        validate_legacy_auth(auth)?;
    }
    let security = raw.security.ok_or(StackError::MissingSection {
        section: "security",
    })?;

    let array = match (raw.array, raw.agent) {
        (Some(array), Some(agent)) => {
            let mut array = array;
            if let Some(primary) = array.primary_target_mut() {
                let primary_target = agent.id.clone();
                primary.id = primary_target.clone();
                primary.agent = agent;
                array.primary_target = primary_target;
            } else {
                return Err(StackError::InvalidParam {
                    field: "array.primary_target",
                    reason: "must reference an entry in array.targets".to_owned(),
                });
            }
            array
        }
        (Some(array), None) => array,
        (None, Some(agent)) => ArrayConfig::from_agent(agent),
        (None, None) => {
            return Err(StackError::MissingSection { section: "agent" });
        }
    };
    let agent = array
        .primary_target()
        .ok_or_else(|| StackError::InvalidParam {
            field: "array.primary_target",
            reason: "must reference an entry in array.targets".to_owned(),
        })?
        .agent
        .clone();
    let config = Config {
        config_version: raw.config_version.unwrap_or(SUPPORTED_CONFIG_VERSION),
        api: raw
            .api
            .ok_or(StackError::MissingSection { section: "api" })?,
        security: SecurityConfig {
            http: security.http.ok_or(StackError::MissingSection {
                section: "security.http",
            })?,
        },
        edge: raw.edge.unwrap_or_default(),
        updates: raw.updates.unwrap_or_default(),
        workspace: raw.workspace.ok_or(StackError::MissingSection {
            section: "workspace",
        })?,
        logging: raw
            .logging
            .ok_or(StackError::MissingSection { section: "logging" })?,
        agent,
        array,
        permissions: raw.permissions.unwrap_or_default(),
        commands: raw.commands.unwrap_or_default(),
        prompts: raw.prompts.unwrap_or_default(),
        dependencies: raw.dependencies.unwrap_or_default(),
        mcp: raw.mcp.unwrap_or_default(),
        local: raw.local.unwrap_or_default(),
    };

    config.validate()?;
    Ok(LoadedConfig {
        config,
        legacy_auth: raw.auth,
    })
}

fn validate_legacy_auth(auth: &LegacyAuthConfig) -> Result<()> {
    validate_legacy_auth_ref("auth.session_key_ref", &auth.session_key_ref)?;
    validate_legacy_auth_ref("auth.admin_key_ref", &auth.admin_key_ref)?;
    if auth.session_key_ref == auth.admin_key_ref {
        return Err(StackError::InvalidParam {
            field: "auth",
            reason: "legacy auth session_key_ref and admin_key_ref must be different".to_owned(),
        });
    }
    Ok(())
}

fn validate_legacy_auth_ref(field: &'static str, value: &str) -> Result<()> {
    if value.trim().is_empty() || value.trim().len() != value.len() {
        return Err(StackError::MissingField { field });
    }
    self::validate::primitives::validate_secret_ref_name_value(value).map_err(|error| {
        StackError::InvalidParam {
            field,
            reason: error.to_string(),
        }
    })
}
