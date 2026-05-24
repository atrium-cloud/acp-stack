use crate::error::{Result, StackError};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::env;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_config_version")]
    pub config_version: u64,
    pub api: ApiConfig,
    pub auth: AuthConfig,
    pub security: SecurityConfig,
    #[serde(default, skip_serializing_if = "EdgeConfig::is_empty")]
    pub edge: EdgeConfig,
    pub workspace: WorkspaceConfig,
    pub logging: LoggingConfig,
    pub agent: AgentConfig,
    #[serde(default)]
    pub permissions: PermissionsConfig,
    #[serde(default)]
    pub commands: CommandsConfig,
    #[serde(default)]
    pub dependencies: DependenciesConfig,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub acpctl: AcpctlConfig,
}

pub const SUPPORTED_CONFIG_VERSION: u64 = 1;

pub const IMPORT_SIZE_LIMIT: usize = 1_048_576;

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
    #[serde(default)]
    pub trusted_proxies: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloudflare: Option<CloudflareEdgeConfig>,
}

impl EdgeConfig {
    pub fn is_empty(&self) -> bool {
        self.cloudflare.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloudflareEdgeConfig {
    pub enabled: bool,
    pub mode: String,
    pub exposure: String,
    pub hostname: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tunnel_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tunnel_id: Option<String>,
    #[serde(default = "default_cloudflared_deployment")]
    pub cloudflared_deployment: String,
}

fn default_cloudflared_deployment() -> String {
    "host".to_owned()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    pub root: String,
    pub uploads: String,
    pub default_shell: String,
    pub runtime_user: String,
    pub max_file_bytes: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub code_sources: Vec<CodeSourceConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub data_sources: Vec<DataSourceConfig>,
}

/// Source for code that init should seed under `<workspace.root>/usr/code/`.
///
/// The only `type` value today is `git`. The schema is shaped as an enum so
/// that additional code-source kinds can be added without invalidating
/// existing configs, but loaders reject unknown values fail-fast.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeSourceConfig {
    #[serde(rename = "type")]
    pub source_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_ref: Option<String>,
    /// Override the derived `<repo-name>` directory under
    /// `<workspace.root>/usr/code/`. Defaults to the trailing path segment of
    /// the repository URL with any `.git` suffix stripped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Source for arbitrary data that init should seed under
/// `<workspace.root>/usr/data/`. `type` is one of `local`, `https`, or `s3`;
/// the other fields are required-or-rejected based on the selected type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DataSourceConfig {
    #[serde(rename = "type")]
    pub source_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    // local
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,

    // https
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_download_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_extracted_bytes: Option<u64>,

    // s3
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bucket: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_key_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_key_ref: Option<String>,
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
    pub service_role_key_ref: String,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Pin the harness install to a specific GitHub Release tag (e.g.
    /// `"v0.42.0"`). Only consulted when the resolved registry entry is
    /// adapter-backed and its harness install is `github_release`. Default
    /// (None) installs the latest release at install time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_version: Option<String>,
    /// Adapter metadata is runtime-populated from the embedded registry,
    /// never operator-written. `skip_deserializing` rejects any operator
    /// who carried a `[agent.adapter]` block over from a pre-rework config.
    #[serde(default, skip_deserializing, skip_serializing)]
    pub adapter: Option<AgentAdapterConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<AgentProviderConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub install: Option<AgentInstallConfig>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentProviderConfig {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom: Option<AgentCustomProviderConfig>,
}

/// Fallback custom-model limits used when an operator does not provide agent
/// config values. They match the documented defaults for the custom provider
/// setup flow and keep the literals centralized across CLI and init paths.
pub const DEFAULT_CUSTOM_MODEL_CONTEXT: u64 = 200_000;
pub const DEFAULT_CUSTOM_MODEL_OUTPUT_MAX_TOKENS: u64 = 65_536;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentCustomProviderConfig {
    pub name: String,
    pub base_url: String,
    #[serde(default)]
    pub api: CustomProviderApi,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    #[serde(default = "default_custom_model_context")]
    pub context: u64,
    #[serde(default = "default_custom_model_output_max_tokens")]
    pub output_max_tokens: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CustomProviderApi {
    #[default]
    ChatCompletions,
    Responses,
}

impl CustomProviderApi {
    pub fn as_pi_api(self) -> &'static str {
        match self {
            Self::ChatCompletions => "openai-completions",
            Self::Responses => "openai-responses",
        }
    }

    pub fn as_codex_wire_api(self) -> &'static str {
        match self {
            Self::Responses => "responses",
            Self::ChatCompletions => "chat",
        }
    }
}

fn default_custom_model_context() -> u64 {
    DEFAULT_CUSTOM_MODEL_CONTEXT
}

fn default_custom_model_output_max_tokens() -> u64 {
    DEFAULT_CUSTOM_MODEL_OUTPUT_MAX_TOKENS
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_timeout: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_action: Option<String>,
}

impl Default for PermissionsConfig {
    fn default() -> Self {
        Self {
            mode: "auto".to_owned(),
            review: Vec::new(),
            deny: Vec::new(),
            request_timeout: None,
            timeout_action: None,
        }
    }
}

pub const DEFAULT_PERMISSION_REQUEST_TIMEOUT: &str = "5m";
pub const DEFAULT_PERMISSION_TIMEOUT_ACTION: &str = "deny";

impl PermissionsConfig {
    pub fn effective_request_timeout(&self) -> std::time::Duration {
        let raw = self
            .request_timeout
            .as_deref()
            .unwrap_or(DEFAULT_PERMISSION_REQUEST_TIMEOUT);
        parse_duration_string(raw).unwrap_or_else(|| {
            parse_duration_string(DEFAULT_PERMISSION_REQUEST_TIMEOUT)
                .unwrap_or(std::time::Duration::from_secs(300))
        })
    }

    pub fn effective_timeout_action(&self) -> PermissionTimeoutAction {
        match self
            .timeout_action
            .as_deref()
            .unwrap_or(DEFAULT_PERMISSION_TIMEOUT_ACTION)
        {
            "approve" => PermissionTimeoutAction::Approve,
            _ => PermissionTimeoutAction::Deny,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionTimeoutAction {
    Deny,
    Approve,
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

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DependenciesConfig {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<DependencyEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub packages: Vec<DependencyEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runtimes: Vec<DependencyEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp: Vec<DependencyEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DependencyEntry {
    pub name: String,
    #[serde(default = "default_dependency_required")]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feature: Option<String>,
    /// Optional install action for `acps deps apply`. When absent, the
    /// command is "check-only" and `acps deps apply` will report it as
    /// not actionable rather than guessing a package manager. This
    /// keeps Dependency Apply narrowly scoped per the Phase 4 spec:
    /// no cross-distro reconciliation, no auto-derived package names —
    /// the operator declares each install action explicitly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install: Option<DependencyInstallAction>,
}

fn default_dependency_required() -> bool {
    true
}

/// Operator-declared install action for one dependency. Intentionally
/// minimal: a single shell snippet, an optional `creates` postcheck,
/// and a scope marker that distinguishes "runs as the runtime user"
/// from "needs OS-wide privilege" so the apply runner can refuse to
/// silently execute privileged work behind the operator's back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DependencyInstallAction {
    /// Shell snippet executed via `[workspace].default_shell -c`.
    /// Operator declares it verbatim — no apt/brew/yum derivation in
    /// the runtime.
    pub shell: String,
    /// PATH name that must resolve to an executable after `shell`
    /// completes. Defaults to the dependency entry's `name`. The apply
    /// runner records `available = true` only when this resolves
    /// post-install.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creates: Option<String>,
    /// `user` (default) runs as the runtime user; `system` declares
    /// the action needs OS-wide privilege (typically sudo). The
    /// runner emits a clear distinction in the audit log and a
    /// confirmation prompt for `system` scope so operators don't
    /// invoke `apt-get install` from a stale CLI invocation.
    #[serde(default)]
    pub scope: DependencyInstallScope,
    /// Optional timeout override in seconds. Defaults to 600s
    /// (10 minutes) — same cap as the agent installer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DependencyInstallScope {
    /// Runs as the runtime user. No privilege escalation. Suitable
    /// for npm globals under `~/.local/`, language toolchains in
    /// $HOME, etc.
    #[default]
    User,
    /// Action needs OS-wide privilege (sudo, system package manager).
    /// The apply runner refuses to fall back to user scope; if the
    /// daemon isn't running as root the action fails early with a
    /// clear "privilege required" message.
    System,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcpctlConfig {
    /// Override path for the `acpctl` Unix-domain socket. When unset the
    /// daemon binds `~/.local/share/acp-stack/acpctl.sock`. Override is
    /// intended for integration tests; a deployed instance should leave it
    /// unset so the socket path matches the spec.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpConfig {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub servers: Vec<McpServerConfig>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum McpServerConfig {
    Stdio(McpStdioServer),
    Http(McpHttpServer),
}

impl McpServerConfig {
    pub fn name(&self) -> &str {
        match self {
            McpServerConfig::Stdio(s) => &s.name,
            McpServerConfig::Http(s) => &s.name,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpStdioServer {
    pub name: String,
    pub command: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpHttpServer {
    pub name: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<HttpHeaderRef>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpHeaderRef {
    pub name: String,
    pub value_ref: String,
}

/// Operator-facing escape hatch for installing an agent whose entry is not
/// in the embedded registry (private fork, unreleased build, custom adapter).
/// The runtime resolves registry-listed agents from `data/agents.toml`
/// keyed off `[agent].id`; this struct is consulted only when the operator
/// explicitly writes `[agent.install]` to override that resolution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentInstallConfig {
    #[serde(rename = "type")]
    pub install_type: String,
    pub creates: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    config_version: Option<u64>,
    api: Option<ApiConfig>,
    auth: Option<AuthConfig>,
    security: Option<RawSecurityConfig>,
    #[serde(default)]
    edge: Option<EdgeConfig>,
    workspace: Option<WorkspaceConfig>,
    logging: Option<LoggingConfig>,
    agent: Option<AgentConfig>,
    #[serde(default)]
    permissions: Option<PermissionsConfig>,
    #[serde(default)]
    commands: Option<CommandsConfig>,
    #[serde(default)]
    dependencies: Option<DependenciesConfig>,
    #[serde(default)]
    mcp: Option<McpConfig>,
    #[serde(default)]
    acpctl: Option<AcpctlConfig>,
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

fn has_legacy_workspace_source_table(input: &str) -> bool {
    // Cheap line-prefix scan; a substring match would false-positive on
    // values that happen to contain the literal string. We do not need to
    // be exact — we only want a friendly hint for the common case.
    input.lines().any(|line| {
        let trimmed = line.trim_start();
        trimmed.starts_with("[workspace.source]") || trimmed.starts_with("[workspace.source.")
    })
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
    let raw: RawConfig = toml::from_str(input)?;
    let security = raw.security.ok_or(StackError::MissingSection {
        section: "security",
    })?;

    let config = Config {
        config_version: raw.config_version.unwrap_or(SUPPORTED_CONFIG_VERSION),
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
        edge: raw.edge.unwrap_or_default(),
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
        dependencies: raw.dependencies.unwrap_or_default(),
        mcp: raw.mcp.unwrap_or_default(),
        acpctl: raw.acpctl.unwrap_or_default(),
    };

    config.validate()?;
    Ok(config)
}

impl Config {
    fn validate(&self) -> Result<()> {
        if self.config_version != SUPPORTED_CONFIG_VERSION {
            return Err(StackError::UnsupportedConfigVersion {
                version: self.config_version,
            });
        }
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
        // `acps init` materializes code/data sources beneath
        // `<workspace.root>/usr/code/` and `<workspace.root>/usr/data/`. If
        // operators point `workspace.uploads` at either lane root (or any
        // ancestor that overlaps), upload write paths can collide with
        // source materialization. Reject the overlap at config-load time so
        // the conflict is impossible to hit at runtime.
        let root = Path::new(&self.workspace.root);
        let uploads = Path::new(&self.workspace.uploads);
        for lane in [
            crate::runtime::workspace_init::CODE_LANE_DIR,
            crate::runtime::workspace_init::DATA_LANE_DIR,
        ] {
            let lane_root = root.join(lane);
            if uploads.starts_with(&lane_root) || lane_root.starts_with(uploads) {
                return Err(StackError::InvalidParam {
                    field: "workspace.uploads",
                    reason: format!(
                        "`{}` collides with the workspace-init lane `{}`",
                        self.workspace.uploads,
                        lane_root.display()
                    ),
                });
            }
        }
        if let Some(socket_path) = &self.acpctl.socket_path {
            validate_optional_config_path("acpctl.socket_path", socket_path)?;
        }
        validate_code_sources(&self.workspace.code_sources)?;
        validate_data_sources(&self.workspace.data_sources)?;
        if let Some(cwd) = &self.agent.cwd {
            validate_absolute_path("agent.cwd", cwd)?;
        }
        validate_agent_restart(&self.agent.restart)?;
        if let Some(expected_sha256) = &self.agent.expected_sha256 {
            validate_expected_sha256(expected_sha256)?;
        }
        if let Some(install) = &self.agent.install {
            validate_agent_install(install)?;
        }
        if let Some(provider) = &self.agent.provider {
            validate_agent_provider(provider)?;
        }
        if let Some(mode) = self.agent.mode.as_deref()
            && (mode.trim().is_empty() || mode.len() != mode.trim().len())
        {
            return Err(StackError::MissingField {
                field: "agent.mode",
            });
        }
        if let Some(model) = self.agent.model.as_deref()
            && (model.trim().is_empty() || model.len() != model.trim().len())
        {
            return Err(StackError::MissingField {
                field: "agent.model",
            });
        }
        validate_permissions(&self.permissions)?;
        validate_commands(&self.commands)?;
        validate_trusted_proxies(&self.security.http)?;
        validate_edge(&self.edge)?;
        validate_dependencies(&self.dependencies)?;
        validate_mcp(&self.mcp)?;
        validate_secret_refs_not_looking_like_values(self)?;
        validate_secret_refs(self)?;
        validate_supabase_logging(self.logging.supabase.as_ref())?;

        Ok(())
    }
}

fn validate_agent_provider(provider: &AgentProviderConfig) -> Result<()> {
    if provider.id.trim().is_empty() || provider.id.len() != provider.id.trim().len() {
        return Err(StackError::MissingField {
            field: "agent.provider.id",
        });
    }
    if let Some(model) = provider.model.as_deref()
        && (model.trim().is_empty() || model.len() != model.trim().len())
    {
        return Err(StackError::MissingField {
            field: "agent.provider.model",
        });
    }
    if let Some(api_key_ref) = provider.api_key_ref.as_deref() {
        validate_secret_ref_name_value(api_key_ref)?;
    }
    if let Some(custom) = provider.custom.as_ref() {
        if provider.model.is_none() {
            return Err(StackError::MissingField {
                field: "agent.provider.model",
            });
        }
        if provider.api_key_ref.is_none() {
            return Err(StackError::MissingField {
                field: "agent.provider.api_key_ref",
            });
        }
        validate_agent_custom_provider(custom)?;
    }
    Ok(())
}

fn validate_agent_custom_provider(custom: &AgentCustomProviderConfig) -> Result<()> {
    validate_non_empty_trimmed("agent.provider.custom.name", &custom.name)?;
    validate_non_empty_trimmed("agent.provider.custom.base_url", &custom.base_url)?;
    if !custom.base_url.starts_with("http://") && !custom.base_url.starts_with("https://") {
        return Err(StackError::InvalidParam {
            field: "agent.provider.custom.base_url",
            reason: "must start with http:// or https://".to_owned(),
        });
    }
    if let Some(model_name) = custom.model_name.as_deref() {
        validate_non_empty_trimmed("agent.provider.custom.model_name", model_name)?;
    }
    if custom.context == 0 {
        return Err(StackError::InvalidParam {
            field: "agent.provider.custom.context",
            reason: "must be greater than 0".to_owned(),
        });
    }
    if custom.output_max_tokens == 0 {
        return Err(StackError::InvalidParam {
            field: "agent.provider.custom.output_max_tokens",
            reason: "must be greater than 0".to_owned(),
        });
    }
    Ok(())
}

fn validate_non_empty_trimmed(field: &'static str, value: &str) -> Result<()> {
    if value.trim().is_empty() || value.trim().len() != value.len() {
        return Err(StackError::MissingField { field });
    }
    Ok(())
}

fn validate_supabase_logging(supabase: Option<&SupabaseLoggingConfig>) -> Result<()> {
    let Some(supabase) = supabase else {
        return Ok(());
    };
    if !supabase.enabled {
        return Ok(());
    }
    if !supabase.url.starts_with("https://") {
        return Err(StackError::InvalidSupabaseUrl {
            url: supabase.url.clone(),
        });
    }
    if !is_safe_pg_identifier(&supabase.schema) {
        return Err(StackError::InvalidSupabaseSchema {
            schema: supabase.schema.clone(),
        });
    }
    Ok(())
}

fn validate_edge(edge: &EdgeConfig) -> Result<()> {
    let Some(cloudflare) = &edge.cloudflare else {
        return Ok(());
    };
    if !cloudflare.enabled {
        return Ok(());
    }
    if cloudflare.mode == "managed" {
        return Err(StackError::CloudflareManagedNotImplemented);
    }
    if cloudflare.mode != "generated" {
        return Err(StackError::InvalidCloudflareMode {
            mode: cloudflare.mode.clone(),
        });
    }
    if cloudflare.exposure != "tunnel" {
        return Err(StackError::InvalidCloudflareExposure {
            exposure: cloudflare.exposure.clone(),
        });
    }
    if !matches!(
        cloudflare.cloudflared_deployment.as_str(),
        "host" | "docker" | "external"
    ) {
        return Err(StackError::InvalidCloudflaredDeployment {
            deployment: cloudflare.cloudflared_deployment.clone(),
        });
    }
    validate_cloudflare_hostname(&cloudflare.hostname)?;
    validate_cloudflare_tunnel_name(cloudflare.tunnel_name.as_deref())?;
    validate_cloudflare_tunnel_id(cloudflare.tunnel_id.as_deref())?;
    Ok(())
}

fn validate_cloudflare_hostname(hostname: &str) -> Result<()> {
    let hostname = hostname.trim();
    if hostname.is_empty()
        || hostname.len() > 253
        || hostname.contains('/')
        || hostname.contains(':')
        || hostname.chars().any(char::is_whitespace)
        || !hostname.contains('.')
    {
        return Err(StackError::InvalidCloudflareHostname {
            hostname: hostname.to_owned(),
        });
    }
    for label in hostname.split('.') {
        if label.is_empty()
            || label.len() > 63
            || label.starts_with('-')
            || label.ends_with('-')
            || !label
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
        {
            return Err(StackError::InvalidCloudflareHostname {
                hostname: hostname.to_owned(),
            });
        }
    }
    Ok(())
}

fn validate_cloudflare_tunnel_name(value: Option<&str>) -> Result<()> {
    let Some(value) = value else {
        return Ok(());
    };
    if value.trim().is_empty() {
        return Err(StackError::MissingField {
            field: "edge.cloudflare.tunnel_name",
        });
    }
    if value.len() > 64
        || value.chars().any(|ch| {
            !(ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-')) || ch.is_ascii_control()
        })
    {
        return Err(StackError::InvalidCloudflareTunnelName {
            tunnel_name: value.to_owned(),
        });
    }
    Ok(())
}

fn validate_cloudflare_tunnel_id(value: Option<&str>) -> Result<()> {
    let Some(value) = value else {
        return Ok(());
    };
    if value.trim().is_empty() {
        return Err(StackError::MissingField {
            field: "edge.cloudflare.tunnel_id",
        });
    }
    let bytes = value.as_bytes();
    let uuid_shape = bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        });
    if !uuid_shape {
        return Err(StackError::InvalidCloudflareTunnelId {
            tunnel_id: value.to_owned(),
        });
    }
    Ok(())
}

/// Match Postgres' rules for an unquoted identifier: starts with `a-z` or `_`,
/// followed by `[a-z0-9_]`, up to 63 chars total. We deliberately reject
/// uppercase to keep the `Content-Profile` header lowercase and avoid quoting.
fn is_safe_pg_identifier(s: &str) -> bool {
    if s.is_empty() || s.len() > 63 {
        return false;
    }
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_lowercase() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn validate_permissions(permissions: &PermissionsConfig) -> Result<()> {
    match permissions.mode.as_str() {
        "auto" | "supervised" | "locked" => {}
        _ => return Err(StackError::InvalidPermissionsMode),
    }
    if let Some(value) = permissions.request_timeout.as_deref() {
        let parsed = parse_duration_string(value).ok_or(StackError::InvalidDurationField {
            field: "permissions.request_timeout",
        })?;
        if parsed.is_zero() {
            return Err(StackError::NonZeroRequired {
                field: "permissions.request_timeout",
            });
        }
    }
    if let Some(action) = permissions.timeout_action.as_deref() {
        match action {
            "deny" | "approve" => {}
            _ => return Err(StackError::InvalidTimeoutAction),
        }
    }
    Ok(())
}

fn validate_trusted_proxies(http: &SecurityHttpConfig) -> Result<()> {
    for entry in &http.trusted_proxies {
        if entry.parse::<IpAddr>().is_err() {
            return Err(StackError::InvalidTrustedProxy {
                value: entry.clone(),
            });
        }
    }
    Ok(())
}

fn validate_dependencies(deps: &DependenciesConfig) -> Result<()> {
    fn check(category: &'static str, list: &[DependencyEntry]) -> Result<()> {
        let mut seen = HashSet::new();
        for entry in list {
            if entry.name.trim().is_empty() {
                return Err(StackError::DependencyMissingName { category });
            }
            if !seen.insert(entry.name.clone()) {
                return Err(StackError::DuplicateDependency {
                    category,
                    name: entry.name.clone(),
                });
            }
        }
        Ok(())
    }
    check("commands", &deps.commands)?;
    check("packages", &deps.packages)?;
    check("runtimes", &deps.runtimes)?;
    check("mcp", &deps.mcp)?;
    // The `install` block is only meaningful for command deps —
    // `acps deps apply` runs install actions exclusively against
    // `dependencies.commands`. Reject install metadata on the other
    // categories so the operator doesn't declare it expecting it to
    // do something and silently get nothing (the "narrow supported
    // actions" contract from Phase 4 spec L62/L67).
    for (category, list) in [
        ("packages", &deps.packages),
        ("runtimes", &deps.runtimes),
        ("mcp", &deps.mcp),
    ] {
        for entry in list.iter() {
            if entry.install.is_some() {
                return Err(StackError::InvalidParam {
                    field: "dependencies",
                    reason: format!(
                        "dependency `{name}` under `{category}` declares an [install] block, \
                         but install actions are only supported on `commands` (Phase 4 deps apply)",
                        name = entry.name,
                    ),
                });
            }
        }
    }
    for entry in &deps.commands {
        let Some(install) = entry.install.as_ref() else {
            continue;
        };
        // Catch operator typos at config-load. An empty shell snippet
        // would no-op the install; a blank `creates` would produce an
        // impossible postcheck; `timeout_secs = 0` would surface as
        // an instant timeout on every run.
        if install.shell.trim().is_empty() {
            return Err(StackError::InvalidParam {
                field: "dependencies",
                reason: format!(
                    "dependency `{name}` has [install] with empty `shell`",
                    name = entry.name,
                ),
            });
        }
        if let Some(creates) = install.creates.as_deref()
            && creates.trim().is_empty()
        {
            return Err(StackError::InvalidParam {
                field: "dependencies",
                reason: format!(
                    "dependency `{name}` has [install] with empty `creates`",
                    name = entry.name,
                ),
            });
        }
        if matches!(install.timeout_secs, Some(0)) {
            return Err(StackError::InvalidParam {
                field: "dependencies",
                reason: format!(
                    "dependency `{name}` has [install].timeout_secs = 0; \
                     omit the field to use the 10m default, or set a positive value",
                    name = entry.name,
                ),
            });
        }
    }
    Ok(())
}

fn validate_mcp(mcp: &McpConfig) -> Result<()> {
    let mut seen = HashSet::new();
    for server in &mcp.servers {
        let name = server.name();
        if name.trim().is_empty() {
            return Err(StackError::InvalidMcpServer {
                name: name.to_owned(),
                reason: "name is required",
            });
        }
        if !seen.insert(name.to_owned()) {
            return Err(StackError::DuplicateMcpServer {
                name: name.to_owned(),
            });
        }
        match server {
            McpServerConfig::Stdio(s) => {
                if s.command.trim().is_empty() {
                    return Err(StackError::InvalidMcpServer {
                        name: s.name.clone(),
                        reason: "stdio.command is required",
                    });
                }
                for env_name in &s.env {
                    validate_secret_ref_name_value(env_name)?;
                }
            }
            McpServerConfig::Http(s) => {
                validate_http_url_prefix("mcp.servers.url", &s.url)?;
                for header in &s.headers {
                    if header.name.trim().is_empty() {
                        return Err(StackError::InvalidMcpServer {
                            name: s.name.clone(),
                            reason: "header.name is required",
                        });
                    }
                    validate_secret_ref_name_value(&header.value_ref)?;
                }
            }
        }
    }
    Ok(())
}

fn validate_secret_ref_name_value(name: &str) -> Result<()> {
    if !is_valid_secret_ref_name(name) {
        return Err(StackError::InvalidSecretRefName {
            name: name.to_owned(),
        });
    }
    Ok(())
}

/// Accept identifier-like names: ASCII letters, digits, and underscores; must
/// not be empty and must not start with a digit. Matches the spirit of POSIX
/// env-var names and the auth-key naming used elsewhere in the project.
pub fn is_valid_secret_ref_name(name: &str) -> bool {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.len() != name.len() {
        return false;
    }
    let mut chars = trimmed.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Walk every secret-ref name in the config and ensure:
///   1. The name itself is a syntactically valid identifier.
///   2. No non-auth ref aliases the configured session or admin key ref.
///   3. The same name is not declared twice across the agent env, workspace
///      source refs, supabase ref, MCP envs, and MCP header value_refs.
fn validate_secret_refs(config: &Config) -> Result<()> {
    let auth_session = config.auth.session_key_ref.as_str();
    let auth_admin = config.auth.admin_key_ref.as_str();
    let mut seen: HashSet<String> = HashSet::new();

    let mut record = |name: &str, kind: &'static str| -> Result<()> {
        validate_secret_ref_name_value(name)?;
        if name == auth_session || name == auth_admin {
            return Err(StackError::SecretRefReservedForAuth {
                ref_name: name.to_owned(),
                kind,
            });
        }
        if !seen.insert(name.to_owned()) {
            return Err(StackError::DuplicateSecretRef {
                name: name.to_owned(),
            });
        }
        Ok(())
    };

    for env_ref in &config.agent.env {
        record(env_ref, "agent.env")?;
    }
    if let Some(supabase) = &config.logging.supabase {
        record(&supabase.service_role_key_ref, "logging.supabase")?;
    }
    for source in &config.workspace.code_sources {
        if let Some(value) = source.credential_ref.as_deref() {
            record(value, "workspace.code_sources.credential_ref")?;
        }
    }
    for source in &config.workspace.data_sources {
        if let Some(value) = source.access_key_ref.as_deref() {
            record(value, "workspace.data_sources.access_key_ref")?;
        }
        if let Some(value) = source.secret_key_ref.as_deref() {
            record(value, "workspace.data_sources.secret_key_ref")?;
        }
    }
    for server in &config.mcp.servers {
        match server {
            McpServerConfig::Stdio(s) => {
                for env_ref in &s.env {
                    record(env_ref, "mcp.servers.env")?;
                }
            }
            McpServerConfig::Http(s) => {
                for header in &s.headers {
                    record(&header.value_ref, "mcp.servers.headers")?;
                }
            }
        }
    }
    Ok(())
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

/// Compare two `[auth]` blocks and return an error if either ref name changed.
/// Used by both `acps config import` (CLI) and `POST /v1/config/import` to
/// uphold the "admin key never regenerable in place" + "session key only
/// rotated via `acps auth regenerate-session-key`" invariants.
pub fn compare_auth_refs(current: &AuthConfig, incoming: &AuthConfig) -> Result<()> {
    if current.session_key_ref != incoming.session_key_ref {
        return Err(StackError::ImportChangesAuthRef {
            field: "session_key_ref",
            current: current.session_key_ref.clone(),
            incoming: incoming.session_key_ref.clone(),
        });
    }
    if current.admin_key_ref != incoming.admin_key_ref {
        return Err(StackError::ImportChangesAuthRef {
            field: "admin_key_ref",
            current: current.admin_key_ref.clone(),
            incoming: incoming.admin_key_ref.clone(),
        });
    }
    Ok(())
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
    // Auth refs are themselves stored in the secret store under these names,
    // so they must follow the same identifier rules as every other ref.
    // Otherwise an auth_ref like "weird name" could silently fail to round-
    // trip through the store on init.
    validate_secret_ref_name_value(session)?;
    validate_secret_ref_name_value(admin)?;
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

fn validate_code_sources(sources: &[CodeSourceConfig]) -> Result<()> {
    let mut seen_names: HashSet<String> = HashSet::new();
    for (index, source) in sources.iter().enumerate() {
        validate_code_source(index, source, &mut seen_names)?;
    }
    Ok(())
}

fn validate_code_source(
    index: usize,
    source: &CodeSourceConfig,
    seen_names: &mut HashSet<String>,
) -> Result<()> {
    let invalid = |reason: String| StackError::WorkspaceCodeSourceInvalid { index, reason };

    if source.source_type.as_str() != "git" {
        return Err(invalid(format!(
            "type must be `git`; got `{}`",
            source.source_type
        )));
    }
    let repo = source
        .repo
        .as_deref()
        .ok_or_else(|| invalid("repo is required when type is git".to_owned()))?;
    require_workspace_field("repo", repo, invalid)?;
    // `git+https://` (Cargo-style) is not accepted because the host `git`
    // binary does not understand it; operators must use a bare `https://`
    // URL or an `ssh://`/`git@…:…` reference.
    require_url_with_scheme("repo", repo, &["https", "ssh"], invalid)?;
    if let Some(branch) = source.branch.as_deref() {
        require_nonempty_trimmed("branch", branch, invalid)?;
    }
    if let Some(credential_ref) = source.credential_ref.as_deref() {
        require_nonempty_trimmed("credential_ref", credential_ref, invalid)?;
        validate_secret_ref_name_value(credential_ref).map_err(|err| {
            invalid(format!(
                "credential_ref `{credential_ref}` is not a valid secret reference: {err}"
            ))
        })?;
    }
    let derived = derive_code_source_name(source).map_err(invalid)?;
    if !seen_names.insert(derived.clone()) {
        return Err(invalid(format!(
            "duplicate destination name `{derived}` (override with `name = ...`)"
        )));
    }
    Ok(())
}

pub(crate) fn derive_code_source_name(
    source: &CodeSourceConfig,
) -> std::result::Result<String, String> {
    if let Some(name) = source.name.as_deref() {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err("name must not be blank".to_owned());
        }
        ensure_safe_dir_name(trimmed).map(|s| s.to_owned())
    } else {
        let repo = source
            .repo
            .as_deref()
            .ok_or_else(|| "repo is required to derive a directory name".to_owned())?;
        let derived = derive_repo_name(repo)?;
        ensure_safe_dir_name(&derived).map(|s| s.to_owned())
    }
}

fn derive_repo_name(repo: &str) -> std::result::Result<String, String> {
    let trimmed = repo.trim().trim_end_matches('/');
    let leaf = trimmed.rsplit(['/', ':'].as_slice()).next().unwrap_or("");
    let stem = leaf.strip_suffix(".git").unwrap_or(leaf);
    if stem.is_empty() {
        return Err(format!(
            "could not derive a directory name from repo `{repo}`"
        ));
    }
    Ok(stem.to_owned())
}

fn validate_data_sources(sources: &[DataSourceConfig]) -> Result<()> {
    let mut seen_names: HashSet<String> = HashSet::new();
    for (index, source) in sources.iter().enumerate() {
        validate_data_source(index, source, &mut seen_names)?;
    }
    Ok(())
}

fn validate_data_source(
    index: usize,
    source: &DataSourceConfig,
    seen_names: &mut HashSet<String>,
) -> Result<()> {
    let invalid = |reason: String| StackError::WorkspaceDataSourceInvalid { index, reason };
    let allowed_types = ["local", "https", "s3"];
    if !allowed_types.contains(&source.source_type.as_str()) {
        return Err(invalid(format!(
            "type must be one of {}; got `{}`",
            allowed_types.join(", "),
            source.source_type
        )));
    }

    let reject_for_type = |field: &'static str, value: Option<&str>| -> Result<()> {
        if value.map(|v| !v.trim().is_empty()).unwrap_or(false) {
            Err(invalid(format!(
                "{field} is not valid when type is {}",
                source.source_type
            )))
        } else {
            Ok(())
        }
    };

    // Numeric caps that may be set on the source. We validate non-zero
    // and reject them on source types that ignore them so operators do
    // not write configs that load cleanly but silently no-op the cap.
    let reject_numeric_for_type = |field: &'static str, value: Option<u64>| -> Result<()> {
        match value {
            Some(0) => Err(invalid(format!("{field} must be greater than zero"))),
            Some(_) => Err(invalid(format!(
                "{field} is not valid when type is {}",
                source.source_type
            ))),
            None => Ok(()),
        }
    };
    let require_nonzero = |field: &'static str, value: Option<u64>| -> Result<()> {
        if let Some(0) = value {
            return Err(invalid(format!("{field} must be greater than zero")));
        }
        Ok(())
    };

    match source.source_type.as_str() {
        "local" => {
            let path = source
                .path
                .as_deref()
                .ok_or_else(|| invalid("path is required when type is local".to_owned()))?;
            require_nonempty_trimmed("path", path, invalid)?;
            if !Path::new(path).is_absolute() {
                return Err(invalid(format!("path `{path}` must be absolute")));
            }
            for component in Path::new(path).components() {
                if matches!(component, std::path::Component::ParentDir) {
                    return Err(invalid(format!(
                        "path `{path}` must not contain `..` segments"
                    )));
                }
            }
            reject_for_type("url", source.url.as_deref())?;
            reject_for_type("expected_sha256", source.expected_sha256.as_deref())?;
            reject_for_type("bucket", source.bucket.as_deref())?;
            reject_for_type("prefix", source.prefix.as_deref())?;
            reject_for_type("region", source.region.as_deref())?;
            reject_for_type("access_key_ref", source.access_key_ref.as_deref())?;
            reject_for_type("secret_key_ref", source.secret_key_ref.as_deref())?;
            reject_numeric_for_type("max_download_bytes", source.max_download_bytes)?;
            reject_numeric_for_type("max_extracted_bytes", source.max_extracted_bytes)?;
        }
        "https" => {
            let url = source
                .url
                .as_deref()
                .ok_or_else(|| invalid("url is required when type is https".to_owned()))?;
            require_nonempty_trimmed("url", url, invalid)?;
            if !url.starts_with("https://") {
                return Err(invalid("url must start with https://".to_owned()));
            }
            if let Some(sha) = source.expected_sha256.as_deref() {
                validate_expected_sha256(sha)
                    .map_err(|err| invalid(format!("expected_sha256 is invalid: {err}")))?;
            }
            require_nonzero("max_download_bytes", source.max_download_bytes)?;
            require_nonzero("max_extracted_bytes", source.max_extracted_bytes)?;
            reject_for_type("path", source.path.as_deref())?;
            reject_for_type("bucket", source.bucket.as_deref())?;
            reject_for_type("prefix", source.prefix.as_deref())?;
            reject_for_type("region", source.region.as_deref())?;
            reject_for_type("access_key_ref", source.access_key_ref.as_deref())?;
            reject_for_type("secret_key_ref", source.secret_key_ref.as_deref())?;
        }
        "s3" => {
            let bucket = source
                .bucket
                .as_deref()
                .ok_or_else(|| invalid("bucket is required when type is s3".to_owned()))?;
            require_nonempty_trimmed("bucket", bucket, invalid)?;
            let region = source
                .region
                .as_deref()
                .ok_or_else(|| invalid("region is required when type is s3".to_owned()))?;
            require_nonempty_trimmed("region", region, invalid)?;
            let access = source
                .access_key_ref
                .as_deref()
                .ok_or_else(|| invalid("access_key_ref is required when type is s3".to_owned()))?;
            require_nonempty_trimmed("access_key_ref", access, invalid)?;
            validate_secret_ref_name_value(access)
                .map_err(|err| invalid(format!("access_key_ref `{access}` is not valid: {err}")))?;
            let secret = source
                .secret_key_ref
                .as_deref()
                .ok_or_else(|| invalid("secret_key_ref is required when type is s3".to_owned()))?;
            require_nonempty_trimmed("secret_key_ref", secret, invalid)?;
            validate_secret_ref_name_value(secret)
                .map_err(|err| invalid(format!("secret_key_ref `{secret}` is not valid: {err}")))?;
            if let Some(prefix) = source.prefix.as_deref() {
                require_nonempty_trimmed("prefix", prefix, invalid)?;
            }
            require_nonzero("max_download_bytes", source.max_download_bytes)?;
            // S3 ingest does not extract archives, so the extracted cap
            // would be silently ignored. Reject it explicitly.
            reject_numeric_for_type("max_extracted_bytes", source.max_extracted_bytes)?;
            reject_for_type("path", source.path.as_deref())?;
            reject_for_type("url", source.url.as_deref())?;
            reject_for_type("expected_sha256", source.expected_sha256.as_deref())?;
        }
        _ => unreachable!("source_type already validated"),
    }

    if let Some(name) = source.name.as_deref() {
        require_nonempty_trimmed("name", name, invalid)?;
        ensure_safe_dir_name(name.trim()).map_err(invalid)?;
    }

    let derived = derive_data_source_name(source).map_err(invalid)?;
    if !seen_names.insert(derived.clone()) {
        return Err(invalid(format!(
            "duplicate destination name `{derived}` (override with `name = ...`)"
        )));
    }
    Ok(())
}

pub(crate) fn derive_data_source_name(
    source: &DataSourceConfig,
) -> std::result::Result<String, String> {
    if let Some(name) = source.name.as_deref() {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err("name must not be blank".to_owned());
        }
        return ensure_safe_dir_name(trimmed).map(|s| s.to_owned());
    }
    let derived = match source.source_type.as_str() {
        "local" => {
            // Local paths can point at either a file (`/data/dataset.tar.gz`)
            // or a directory (`/data/reports.v1`). We cannot tell which at
            // validation time, and stripping the extension blindly would
            // mangle directory names like `reports.v1` into `reports`.
            // Preserve the basename as-is; operators who want stripping
            // should set `name = "..."` explicitly.
            let path = source.path.as_deref().unwrap_or("");
            let leaf = Path::new(path)
                .file_name()
                .and_then(|f| f.to_str())
                .unwrap_or("");
            if leaf.is_empty() {
                return Err(format!(
                    "could not derive a directory name from path `{path}`"
                ));
            }
            leaf.to_owned()
        }
        "https" => {
            let url = source.url.as_deref().unwrap_or("");
            let trimmed = url.trim_end_matches('/');
            let leaf = trimmed
                .rsplit('/')
                .next()
                .and_then(|seg| seg.split('?').next())
                .unwrap_or("");
            if leaf.is_empty() {
                return Err(format!(
                    "could not derive a directory name from url `{url}`"
                ));
            }
            strip_archive_extension(leaf).to_owned()
        }
        "s3" => {
            let bucket = source.bucket.as_deref().unwrap_or("");
            let prefix = source.prefix.as_deref().unwrap_or("");
            let trimmed = prefix.trim_end_matches('/');
            if trimmed.is_empty() {
                if bucket.is_empty() {
                    return Err("could not derive a directory name (empty bucket)".to_owned());
                }
                bucket.to_owned()
            } else {
                let leaf = trimmed.rsplit('/').next().unwrap_or(bucket);
                leaf.to_owned()
            }
        }
        _ => unreachable!("source_type already validated"),
    };
    ensure_safe_dir_name(&derived).map(|s| s.to_owned())
}

fn ensure_safe_dir_name(name: &str) -> std::result::Result<&str, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("derived directory name is empty".to_owned());
    }
    if trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed.contains('\0')
        || trimmed == "."
        || trimmed == ".."
    {
        return Err(format!(
            "directory name `{trimmed}` is not safe; override with `name = ...`"
        ));
    }
    Ok(trimmed)
}

fn strip_archive_extension(name: &str) -> &str {
    for ext in [
        ".tar.gz", ".tar.bz2", ".tar.xz", ".tar.zst", ".tgz", ".tbz2", ".txz",
    ] {
        if let Some(stripped) = name.strip_suffix(ext)
            && !stripped.is_empty()
        {
            return stripped;
        }
    }
    // Only strip a trailing extension when the prefix is non-empty;
    // otherwise names like `.tmpXYZ` would collapse to "".
    if let Some(dot) = name.rfind('.')
        && dot > 0
    {
        return &name[..dot];
    }
    name
}

fn require_workspace_field<F>(field: &'static str, value: &str, build: F) -> Result<()>
where
    F: FnOnce(String) -> StackError,
{
    if value.trim().is_empty() {
        Err(build(format!("{field} is required")))
    } else {
        Ok(())
    }
}

fn require_nonempty_trimmed<F>(field: &'static str, value: &str, build: F) -> Result<()>
where
    F: FnOnce(String) -> StackError,
{
    if value.trim().is_empty() || value.len() != value.trim().len() {
        Err(build(format!("{field} must be non-empty and trimmed")))
    } else {
        Ok(())
    }
}

fn require_url_with_scheme<F>(
    field: &'static str,
    value: &str,
    allowed_prefixes: &[&str],
    build: F,
) -> Result<()>
where
    F: FnOnce(String) -> StackError,
{
    let has_known_scheme = allowed_prefixes
        .iter()
        .any(|scheme| value.starts_with(&format!("{scheme}://")));
    let looks_like_git_ssh = value.contains('@')
        && value.contains(':')
        && !value.starts_with("http://")
        && !value.starts_with("https://");
    // Absolute filesystem paths are valid git "URLs" (file:// shorthand) and
    // are useful for tests and on-host mirrors. We accept them only when
    // the caller's allowlist includes a path-shaped scheme — git is the
    // only one today.
    let looks_like_path = allowed_prefixes.contains(&"https")
        && Path::new(value).is_absolute()
        && !value.contains("://");
    if has_known_scheme
        || (allowed_prefixes.contains(&"ssh") && looks_like_git_ssh)
        || looks_like_path
    {
        Ok(())
    } else {
        Err(build(format!(
            "{field} must use one of these schemes: {}",
            allowed_prefixes.join(", ")
        )))
    }
}

fn require_present<'a>(field: &'static str, value: Option<&'a str>) -> Result<&'a str> {
    match value {
        Some(value) if !value.trim().is_empty() => Ok(value),
        _ => Err(StackError::MissingField { field }),
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

fn validate_agent_install(install: &AgentInstallConfig) -> Result<()> {
    validate_nonempty("agent.install.creates", &install.creates)?;
    match install.install_type.as_str() {
        "shell" => {
            require_present("agent.install.shell", install.shell.as_deref())?;
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

fn validate_http_url_prefix(field: &'static str, value: &str) -> Result<()> {
    if value.starts_with("http://") || value.starts_with("https://") {
        return Ok(());
    }
    Err(StackError::UrlMustBeHttp { field })
}

fn validate_optional_config_path(field: &'static str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(StackError::MissingField { field });
    }
    if !Path::new(value).is_absolute() {
        return Err(StackError::PathMustBeAbsolute { field });
    }
    for component in Path::new(value).components() {
        if matches!(component, std::path::Component::ParentDir) {
            return Err(StackError::PathContainsParentDir { field });
        }
    }
    Ok(())
}

fn secret_ref_looks_like_value(name: &str) -> bool {
    if name.len() > 128 {
        return true;
    }
    if name.len() > 40 && name.chars().all(|c| c.is_ascii_hexdigit()) {
        return true;
    }
    if name.starts_with("acps_")
        || name.starts_with("sk-")
        || name.starts_with("ghp_")
        || name.starts_with("github_pat_")
        || name.starts_with("xoxb-")
        || name.starts_with("xoxp-")
        || name.starts_with("xoxa-")
    {
        return true;
    }
    let jwt_parts = name.split('.').collect::<Vec<_>>();
    if jwt_parts.len() == 3
        && jwt_parts
            .iter()
            .all(|part| part.len() >= 10 && part.chars().all(is_base64url_char))
    {
        return true;
    }
    false
}

fn is_base64url_char(value: char) -> bool {
    value.is_ascii_alphanumeric() || value == '_' || value == '-'
}

fn validate_secret_refs_not_looking_like_values(config: &Config) -> Result<()> {
    let check = |name: &str, field: &'static str| -> Result<()> {
        if secret_ref_looks_like_value(name) {
            return Err(StackError::SecretRefLooksLikeValue { field });
        }
        Ok(())
    };

    for env_ref in &config.agent.env {
        check(env_ref, "agent.env")?;
    }
    if let Some(supabase) = &config.logging.supabase {
        check(
            &supabase.service_role_key_ref,
            "logging.supabase.service_role_key_ref",
        )?;
    }
    for source in &config.workspace.code_sources {
        if let Some(value) = source.credential_ref.as_deref() {
            check(value, "workspace.code_sources.credential_ref")?;
        }
    }
    for source in &config.workspace.data_sources {
        if let Some(value) = source.access_key_ref.as_deref() {
            check(value, "workspace.data_sources.access_key_ref")?;
        }
        if let Some(value) = source.secret_key_ref.as_deref() {
            check(value, "workspace.data_sources.secret_key_ref")?;
        }
    }
    for server in &config.mcp.servers {
        match server {
            McpServerConfig::Stdio(s) => {
                for env_ref in &s.env {
                    check(env_ref, "mcp.servers.env")?;
                }
            }
            McpServerConfig::Http(s) => {
                for header in &s.headers {
                    check(&header.value_ref, "mcp.servers.headers")?;
                }
            }
        }
    }
    check(&config.auth.session_key_ref, "auth.session_key_ref")?;
    check(&config.auth.admin_key_ref, "auth.admin_key_ref")?;
    if let Some(provider) = &config.agent.provider
        && let Some(api_key_ref) = provider.api_key_ref.as_deref()
    {
        check(api_key_ref, "agent.provider.api_key_ref")?;
    }
    Ok(())
}
