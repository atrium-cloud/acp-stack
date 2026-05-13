use crate::error::{Result, StackError};
use serde::{Deserialize, Serialize};
use std::env;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub api: ApiConfig,
    pub auth: AuthConfig,
    pub security: SecurityConfig,
    pub workspace: WorkspaceConfig,
    pub logging: LoggingConfig,
    pub agent: AgentConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiConfig {
    pub bind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_url: Option<String>,
    pub max_request_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    pub session_key_ref: String,
    pub admin_key_ref: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecurityConfig {
    pub http: SecurityHttpConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecurityHttpConfig {
    pub max_request_bytes: u64,
    pub rate_limit_per_minute: u64,
    pub burst: u64,
    pub auth_failures_per_minute: u64,
    pub auth_block_duration: String,
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    pub trust_proxy_headers: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    pub root: String,
    pub uploads: String,
    pub default_shell: String,
    pub runtime_user: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<WorkspaceSourceConfig>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSourceConfig {
    #[serde(rename = "type")]
    pub source_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bucket: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_key_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret_key_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoggingConfig {
    pub level: String,
    pub local_retention_days: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supabase: Option<SupabaseLoggingConfig>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupabaseLoggingConfig {
    pub enabled: bool,
    pub url: String,
    pub api_key_ref: String,
    pub schema: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    pub id: String,
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_sha256: Option<String>,
    pub restart: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub install: Option<AgentInstallConfig>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentInstallConfig {
    #[serde(rename = "type")]
    pub install_type: String,
    pub shell: String,
    pub creates: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    api: Option<ApiConfig>,
    auth: Option<AuthConfig>,
    security: Option<RawSecurityConfig>,
    workspace: Option<WorkspaceConfig>,
    logging: Option<LoggingConfig>,
    agent: Option<AgentConfig>,
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

    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path).map_err(|source| StackError::ConfigRead {
            path: path.to_path_buf(),
            source,
        })?;

        load_config_from_str(&content)
    }

    pub fn to_canonical_toml(&self) -> Result<String> {
        Ok(toml::to_string_pretty(self)?)
    }
}

pub fn default_config_path() -> Result<PathBuf> {
    let home = env::var_os("HOME").ok_or(StackError::HomeNotSet)?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("acp-stack")
        .join("acp-stack.toml"))
}

pub fn load_config_from_str(input: &str) -> Result<Config> {
    let raw: RawConfig = toml::from_str(input)?;
    let security = raw.security.ok_or(StackError::MissingSection {
        section: "security",
    })?;

    let config = Config {
        api: raw
            .api
            .ok_or(StackError::MissingSection { section: "api" })?,
        auth: raw
            .auth
            .ok_or(StackError::MissingSection { section: "auth" })?,
        security: SecurityConfig {
            http: security.http.ok_or(StackError::MissingSection {
                section: "security.http",
            })?,
        },
        workspace: raw.workspace.ok_or(StackError::MissingSection {
            section: "workspace",
        })?,
        logging: raw
            .logging
            .ok_or(StackError::MissingSection { section: "logging" })?,
        agent: raw
            .agent
            .ok_or(StackError::MissingSection { section: "agent" })?,
    };

    config.validate()?;
    Ok(config)
}

impl Config {
    fn validate(&self) -> Result<()> {
        validate_socket_address("api.bind", &self.api.bind)?;
        validate_nonzero("api.max_request_bytes", self.api.max_request_bytes)?;
        validate_nonzero(
            "security.http.max_request_bytes",
            self.security.http.max_request_bytes,
        )?;
        validate_nonzero(
            "security.http.rate_limit_per_minute",
            self.security.http.rate_limit_per_minute,
        )?;
        validate_nonzero("security.http.burst", self.security.http.burst)?;
        validate_nonzero(
            "security.http.auth_failures_per_minute",
            self.security.http.auth_failures_per_minute,
        )?;
        validate_absolute_path("workspace.root", &self.workspace.root)?;
        validate_absolute_path("workspace.uploads", &self.workspace.uploads)?;
        validate_absolute_path("workspace.default_shell", &self.workspace.default_shell)?;
        let source = self
            .workspace
            .source
            .as_ref()
            .ok_or(StackError::MissingSection {
                section: "workspace.source",
            })?;
        validate_workspace_source(source)?;
        if let Some(cwd) = &self.agent.cwd {
            validate_absolute_path("agent.cwd", cwd)?;
        }
        validate_agent_restart(&self.agent.restart)?;

        Ok(())
    }
}

fn validate_socket_address(field: &'static str, value: &str) -> Result<()> {
    value
        .parse::<SocketAddr>()
        .map(|_| ())
        .map_err(|_| StackError::InvalidSocketAddress { field })
}

fn validate_nonzero(field: &'static str, value: u64) -> Result<()> {
    if value == 0 {
        return Err(StackError::NonZeroRequired { field });
    }

    Ok(())
}

fn validate_absolute_path(field: &'static str, value: &str) -> Result<()> {
    if !Path::new(value).is_absolute() {
        return Err(StackError::PathMustBeAbsolute { field });
    }

    Ok(())
}

fn validate_workspace_source(source: &WorkspaceSourceConfig) -> Result<()> {
    match source.source_type.as_str() {
        "none" => {
            reject_present("workspace.source.repo", source.repo.as_deref(), "none")?;
            reject_present("workspace.source.branch", source.branch.as_deref(), "none")?;
            reject_present("workspace.source.bucket", source.bucket.as_deref(), "none")?;
            reject_present("workspace.source.prefix", source.prefix.as_deref(), "none")?;
            reject_present("workspace.source.dest", source.dest.as_deref(), "none")?;
            reject_present(
                "workspace.source.credential_ref",
                source.credential_ref.as_deref(),
                "none",
            )?;
            reject_present(
                "workspace.source.access_key_ref",
                source.access_key_ref.as_deref(),
                "none",
            )?;
            reject_present(
                "workspace.source.secret_key_ref",
                source.secret_key_ref.as_deref(),
                "none",
            )?;
            reject_present("workspace.source.region", source.region.as_deref(), "none")?;
            Ok(())
        }
        "git" => {
            reject_present("workspace.source.bucket", source.bucket.as_deref(), "git")?;
            reject_present("workspace.source.prefix", source.prefix.as_deref(), "git")?;
            reject_present(
                "workspace.source.access_key_ref",
                source.access_key_ref.as_deref(),
                "git",
            )?;
            reject_present(
                "workspace.source.secret_key_ref",
                source.secret_key_ref.as_deref(),
                "git",
            )?;
            reject_present("workspace.source.region", source.region.as_deref(), "git")?;
            require_present("workspace.source.repo", source.repo.as_deref())?;
            let dest = require_present("workspace.source.dest", source.dest.as_deref())?;
            validate_absolute_path("workspace.source.dest", dest)
        }
        "s3" => {
            reject_present("workspace.source.repo", source.repo.as_deref(), "s3")?;
            reject_present("workspace.source.branch", source.branch.as_deref(), "s3")?;
            reject_present(
                "workspace.source.credential_ref",
                source.credential_ref.as_deref(),
                "s3",
            )?;
            require_present("workspace.source.bucket", source.bucket.as_deref())?;
            let dest = require_present("workspace.source.dest", source.dest.as_deref())?;
            require_present(
                "workspace.source.access_key_ref",
                source.access_key_ref.as_deref(),
            )?;
            require_present(
                "workspace.source.secret_key_ref",
                source.secret_key_ref.as_deref(),
            )?;
            require_present("workspace.source.region", source.region.as_deref())?;
            validate_absolute_path("workspace.source.dest", dest)
        }
        _ => Err(StackError::InvalidWorkspaceSourceType),
    }
}

fn require_present<'a>(field: &'static str, value: Option<&'a str>) -> Result<&'a str> {
    match value {
        Some(value) if !value.trim().is_empty() => Ok(value),
        _ => Err(StackError::MissingField { field }),
    }
}

fn reject_present(
    field: &'static str,
    value: Option<&str>,
    source_type: &'static str,
) -> Result<()> {
    match value {
        Some(value) if !value.trim().is_empty() => {
            Err(StackError::InvalidWorkspaceSourceField { field, source_type })
        }
        _ => Ok(()),
    }
}

fn validate_agent_restart(value: &str) -> Result<()> {
    match value {
        "never" | "on-crash" => Ok(()),
        _ => Err(StackError::InvalidAgentRestart),
    }
}
