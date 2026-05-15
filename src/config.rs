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
    #[serde(default)]
    pub permissions: PermissionsConfig,
    #[serde(default)]
    pub commands: CommandsConfig,
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
    pub max_file_bytes: u64,
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
    pub adapter: Option<AgentAdapterConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub install: Option<AgentInstallConfig>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentAdapterConfig {
    pub id: String,
    pub name: String,
    pub upstream_agent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionsConfig {
    pub mode: String,
    #[serde(default)]
    pub review: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
}

impl Default for PermissionsConfig {
    fn default() -> Self {
        Self {
            mode: "auto".to_owned(),
            review: Vec::new(),
            deny: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandsConfig {
    pub default_timeout: String,
    pub cancel_grace: String,
    #[serde(default)]
    pub env_allowlist: Vec<String>,
    pub max_output_bytes: u64,
}

impl Default for CommandsConfig {
    fn default() -> Self {
        Self {
            default_timeout: "10m".to_owned(),
            cancel_grace: "5s".to_owned(),
            env_allowlist: Vec::new(),
            max_output_bytes: 1_048_576,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentInstallConfig {
    #[serde(rename = "type")]
    pub install_type: String,
    pub creates: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registry_url: Option<String>,
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
    #[serde(default)]
    permissions: Option<PermissionsConfig>,
    #[serde(default)]
    commands: Option<CommandsConfig>,
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
    let home = env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .ok_or(StackError::HomeNotSet)?;
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
        permissions: raw.permissions.unwrap_or_default(),
        commands: raw.commands.unwrap_or_default(),
    };

    config.validate()?;
    Ok(config)
}

impl Config {
    fn validate(&self) -> Result<()> {
        validate_socket_address("api.bind", &self.api.bind)?;
        validate_nonzero("api.max_request_bytes", self.api.max_request_bytes)?;
        validate_auth_refs(&self.auth)?;
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
        validate_nonzero("workspace.max_file_bytes", self.workspace.max_file_bytes)?;
        validate_no_parent_dir_segments("workspace.root", &self.workspace.root)?;
        validate_no_parent_dir_segments("workspace.uploads", &self.workspace.uploads)?;
        // Lexical pre-check: uploads must live under root. With `..` segments
        // already rejected above, `starts_with` is sound. The runtime layer
        // also re-resolves the upload destination against workspace.root, so a
        // symlink inside the workspace that points outside is caught at write
        // time; this check rejects the obvious misconfiguration up front and
        // keeps `workspace_relative_string` from emitting absolute paths.
        if !Path::new(&self.workspace.uploads).starts_with(Path::new(&self.workspace.root)) {
            return Err(StackError::WorkspaceUploadsNotUnderRoot);
        }
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
        if let Some(expected_sha256) = &self.agent.expected_sha256 {
            validate_expected_sha256(expected_sha256)?;
        }
        if let Some(adapter) = &self.agent.adapter {
            validate_agent_adapter(adapter)?;
        }
        if let Some(install) = &self.agent.install {
            validate_agent_install(install)?;
        }
        validate_permissions(&self.permissions)?;
        validate_commands(&self.commands)?;

        Ok(())
    }
}

fn validate_permissions(permissions: &PermissionsConfig) -> Result<()> {
    match permissions.mode.as_str() {
        "auto" | "supervised" | "locked" => Ok(()),
        _ => Err(StackError::InvalidPermissionsMode),
    }
}

fn validate_commands(commands: &CommandsConfig) -> Result<()> {
    let timeout = parse_duration_string(&commands.default_timeout).ok_or(
        StackError::InvalidDurationField {
            field: "commands.default_timeout",
        },
    )?;
    if timeout.is_zero() {
        return Err(StackError::NonZeroRequired {
            field: "commands.default_timeout",
        });
    }
    parse_duration_string(&commands.cancel_grace).ok_or(StackError::InvalidDurationField {
        field: "commands.cancel_grace",
    })?;
    if commands.max_output_bytes == 0 {
        return Err(StackError::NonZeroRequired {
            field: "commands.max_output_bytes",
        });
    }
    for name in &commands.env_allowlist {
        if name.trim().is_empty()
            || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            || name.chars().next().is_some_and(|c| c.is_ascii_digit())
        {
            return Err(StackError::InvalidEnvName { name: name.clone() });
        }
    }
    Ok(())
}

/// Parse a duration string like "10m", "5s", "2h", "750ms". Returns `None` on
/// any invalid input. Empty string and pure-numeric inputs (no suffix) are
/// rejected so config typos surface at load time rather than meaning seconds.
pub fn parse_duration_string(input: &str) -> Option<std::time::Duration> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let (number_part, unit_part) = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .map(|idx| trimmed.split_at(idx))?;
    if number_part.is_empty() {
        return None;
    }
    let value: u64 = number_part.parse().ok()?;
    match unit_part {
        "ms" => Some(std::time::Duration::from_millis(value)),
        "s" => Some(std::time::Duration::from_secs(value)),
        "m" => Some(std::time::Duration::from_secs(value.checked_mul(60)?)),
        "h" => Some(std::time::Duration::from_secs(value.checked_mul(3_600)?)),
        _ => None,
    }
}

fn validate_auth_refs(auth: &AuthConfig) -> Result<()> {
    let session = auth.session_key_ref.trim();
    let admin = auth.admin_key_ref.trim();
    if session.is_empty() {
        return Err(StackError::MissingField {
            field: "auth.session_key_ref",
        });
    }
    if admin.is_empty() {
        return Err(StackError::MissingField {
            field: "auth.admin_key_ref",
        });
    }
    // Distinct refs are a hard invariant: if they alias, generating both keys
    // writes the second over the first, and `acps auth regenerate-session-key`
    // rotates the admin key, collapsing the session/admin boundary.
    if session == admin {
        return Err(StackError::AuthRefsNotDistinct);
    }
    Ok(())
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

/// `Path::starts_with` is purely lexical — `/workspace/../etc/uploads`
/// "starts with" `/workspace` even though it resolves outside. Reject `..`
/// segments in the configured paths up front so the workspace-root/uploads
/// containment check below cannot be tricked, and so request-time path
/// resolution does not have to canonicalize the config paths repeatedly.
fn validate_no_parent_dir_segments(field: &'static str, value: &str) -> Result<()> {
    for component in Path::new(value).components() {
        if matches!(component, std::path::Component::ParentDir) {
            return Err(StackError::PathContainsParentDir { field });
        }
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

fn validate_expected_sha256(value: &str) -> Result<()> {
    if value.len() == 64 && value.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')) {
        Ok(())
    } else {
        Err(StackError::InvalidExpectedSha256)
    }
}

fn validate_agent_adapter(adapter: &AgentAdapterConfig) -> Result<()> {
    validate_nonempty("agent.adapter.id", &adapter.id)?;
    validate_nonempty("agent.adapter.name", &adapter.name)?;
    validate_nonempty("agent.adapter.upstream_agent", &adapter.upstream_agent)?;
    if let Some(source_url) = &adapter.source_url {
        validate_http_url_prefix("agent.adapter.source_url", source_url)?;
    }
    Ok(())
}

fn validate_agent_install(install: &AgentInstallConfig) -> Result<()> {
    validate_nonempty("agent.install.creates", &install.creates)?;
    match install.install_type.as_str() {
        "shell" => {
            require_present("agent.install.shell", install.shell.as_deref())?;
            reject_present_for_type("agent.install.id", install.id.as_deref(), "shell")?;
            reject_present_for_type(
                "agent.install.registry_url",
                install.registry_url.as_deref(),
                "shell",
            )?;
            Ok(())
        }
        "registry" => {
            require_present("agent.install.id", install.id.as_deref())?;
            reject_present_for_type("agent.install.shell", install.shell.as_deref(), "registry")?;
            if let Some(registry_url) = &install.registry_url {
                validate_https_url_prefix("agent.install.registry_url", registry_url)?;
            }
            Ok(())
        }
        _ => Err(StackError::InvalidAgentInstallType),
    }
}

fn validate_nonempty(field: &'static str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(StackError::MissingField { field });
    }
    Ok(())
}

fn reject_present_for_type(
    field: &'static str,
    value: Option<&str>,
    type_value: &'static str,
) -> Result<()> {
    match value {
        Some(value) if !value.trim().is_empty() => Err(StackError::InvalidConfigFieldForType {
            field,
            type_field: "agent.install.type",
            type_value,
        }),
        _ => Ok(()),
    }
}

fn validate_http_url_prefix(field: &'static str, value: &str) -> Result<()> {
    if value.starts_with("http://") || value.starts_with("https://") {
        return Ok(());
    }
    Err(StackError::UrlMustBeHttp { field })
}

fn validate_https_url_prefix(field: &'static str, value: &str) -> Result<()> {
    if value.starts_with("https://") {
        return Ok(());
    }
    Err(StackError::UrlMustBeHttps { field })
}
