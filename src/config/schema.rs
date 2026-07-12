//! TOML schema types for `acps-config.toml`.
//!
//! Every nested config struct lives here. The root `Config` aggregator and the
//! `RawConfig` deserialization shim stay in `src/config.rs` because they own
//! load-time orchestration. Default impls and small `default_*` helper
//! functions used by `#[serde(default = "...")]` annotations are co-located
//! with the struct they belong to so the schema is self-contained.

use serde::{Deserialize, Serialize};

// CONSTANTS

/// Fallback custom-model limits used when an operator does not provide agent
/// config values. They match the documented defaults for the custom provider
/// setup flow and keep the literals centralized across CLI and init paths.
pub const DEFAULT_CUSTOM_MODEL_CONTEXT: u64 = 200_000;
pub const DEFAULT_CUSTOM_MODEL_OUTPUT_MAX_TOKENS: u64 = 65_536;

pub const DEFAULT_PERMISSION_REQUEST_TIMEOUT: &str = "5m";
pub const DEFAULT_PERMISSION_TIMEOUT_ACTION: &str = "deny";
pub const DEFAULT_AGENT_AUTO_UPDATE_FREQUENCY: &str = "1d";
pub const DEFAULT_STACK_UPDATE_FREQUENCY: &str = "1d";
pub const DEFAULT_STACK_UPDATE_POLICY: StackUpdatePolicy = StackUpdatePolicy::SecurityCritical;

// API / SECURITY

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

// EDGE

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
    pub api_token_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id_ref: Option<String>,
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

// SELF-UPDATE

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdatesConfig {
    #[serde(default = "default_stack_update_config")]
    pub acp_stack: StackUpdateConfig,
}

impl Default for UpdatesConfig {
    fn default() -> Self {
        Self {
            acp_stack: default_stack_update_config(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StackUpdateConfig {
    #[serde(default = "default_stack_update_policy")]
    pub policy: StackUpdatePolicy,
    #[serde(default = "default_stack_update_frequency")]
    pub frequency: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StackUpdatePolicy {
    Compatible,
    SecurityCritical,
    Manual,
}

fn default_stack_update_config() -> StackUpdateConfig {
    StackUpdateConfig {
        policy: default_stack_update_policy(),
        frequency: default_stack_update_frequency(),
    }
}

fn default_stack_update_policy() -> StackUpdatePolicy {
    DEFAULT_STACK_UPDATE_POLICY
}

fn default_stack_update_frequency() -> String {
    DEFAULT_STACK_UPDATE_FREQUENCY.to_owned()
}

// WORKSPACE / SOURCES

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    pub root: String,
    pub uploads: String,
    pub default_shell: String,
    pub runtime_user: String,
    pub max_file_bytes: u64,
    /// Isolation backend that the agent harness and mediated shells run inside.
    /// Default `off` preserves single-process behavior; other modes wrap each
    /// spawn so the workload cannot read the daemon's secrets/state or reach its
    /// control socket. See [`SandboxConfig`].
    #[serde(default, skip_serializing_if = "SandboxConfig::is_off")]
    pub sandbox: SandboxConfig,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub code_sources: Vec<CodeSourceConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub data_sources: Vec<DataSourceConfig>,
}

/// Selects how the agent harness and mediated shells are isolated from the
/// daemon. The daemon always derives the set of its own sensitive paths to mask
/// (config dir, state dir) from its path helpers, so an operator cannot forget
/// to protect them; the fields below only add to or parameterize that.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    #[serde(default)]
    pub mode: SandboxMode,
    /// Wrapper argv for `mode = "custom"`: prepended to the harness command
    /// (e.g. `["systemd-run", "--scope", "-p", "..."]`). Required for `custom`,
    /// ignored otherwise.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub wrapper: Vec<String>,
    /// Extra absolute paths to mask (read-deny) on top of the daemon's own.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mask_paths: Vec<String>,
    /// Extra absolute paths the workload may read+write (e.g. bwrap binds)
    /// beyond the workspace root.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_paths: Vec<String>,
    /// Network isolation for each wrapped spawn. Omitted block means `host`
    /// networking, which preserves the pre-network wrapper byte for byte.
    /// Only valid with `mode = "unshare"`. See [`SandboxNetworkConfig`].
    #[serde(default, skip_serializing_if = "SandboxNetworkConfig::is_host")]
    pub network: SandboxNetworkConfig,
}

impl SandboxConfig {
    pub fn is_off(&self) -> bool {
        *self == SandboxConfig::default()
    }
}

/// Isolation mechanism. `unshare` requires the daemon to hold `CAP_SYS_ADMIN`
/// (privileged container); `bwrap` requires unprivileged user namespaces;
/// `custom` delegates to an operator-supplied [`SandboxConfig::wrapper`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SandboxMode {
    #[default]
    Off,
    Unshare,
    Bwrap,
    Custom,
}

pub const DEFAULT_SANDBOX_NETWORK_PROVIDER_TIMEOUT: &str = "30s";

/// Per-spawn network isolation for the `unshare` backend. `isolated` gives each
/// wrapped spawn a fresh network namespace; with no `provider` the namespace is
/// deny-all (not even loopback is configured). An operator-supplied provider
/// argv is invoked as `<exe> setup|teardown <args...>` to attach veth devices,
/// routes, DNS, or proxies — acp-stack itself never configures interfaces or
/// inspects traffic.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxNetworkConfig {
    #[serde(default)]
    pub mode: SandboxNetworkMode,
    /// Lifecycle provider argv. Empty means no provider: deny-all networking.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider: Vec<String>,
    /// Duration string applied independently to provider setup and teardown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_timeout: Option<String>,
    /// Where provider stderr goes: the daemon's stderr diagnostic channel, or
    /// discarded. Provider stdout is always discarded.
    #[serde(default, skip_serializing_if = "SandboxProviderStderr::is_default")]
    pub provider_stderr: SandboxProviderStderr,
}

impl SandboxNetworkConfig {
    pub fn is_host(&self) -> bool {
        *self == SandboxNetworkConfig::default()
    }

    pub fn is_isolated(&self) -> bool {
        self.mode == SandboxNetworkMode::Isolated
    }

    pub fn provider_timeout_raw(&self) -> &str {
        self.provider_timeout
            .as_deref()
            .unwrap_or(DEFAULT_SANDBOX_NETWORK_PROVIDER_TIMEOUT)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SandboxNetworkMode {
    #[default]
    Host,
    Isolated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SandboxProviderStderr {
    #[default]
    Daemon,
    Null,
}

impl SandboxProviderStderr {
    fn is_default(&self) -> bool {
        *self == SandboxProviderStderr::default()
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            SandboxProviderStderr::Daemon => "daemon",
            SandboxProviderStderr::Null => "null",
        }
    }
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

// LOGGING

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
    #[serde(default = "default_supabase_backend")]
    pub backend: SupabaseLoggingBackend,
    pub url: String,
    #[serde(default = "default_supabase_table_prefix")]
    pub table_prefix: String,
    #[serde(
        default = "default_supabase_db_url_ref",
        skip_serializing_if = "Option::is_none"
    )]
    pub db_url_ref: Option<String>,
    pub api_key_ref: String,
    pub schema: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SupabaseLoggingBackend {
    Postgrest,
    Postgres,
}

fn default_supabase_backend() -> SupabaseLoggingBackend {
    SupabaseLoggingBackend::Postgrest
}

fn default_supabase_table_prefix() -> String {
    String::new()
}

fn default_supabase_db_url_ref() -> Option<String> {
    None
}

// AGENT

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArrayConfig {
    #[serde(default)]
    pub enabled: bool,
    pub primary_target: String,
    #[serde(default)]
    pub targets: Vec<ArrayTargetConfig>,
}

impl ArrayConfig {
    pub fn from_agent(agent: AgentConfig) -> Self {
        let target_id = agent.id.clone();
        Self {
            enabled: false,
            primary_target: target_id.clone(),
            targets: vec![ArrayTargetConfig {
                id: target_id,
                agent,
            }],
        }
    }

    pub fn primary_target(&self) -> Option<&ArrayTargetConfig> {
        self.targets
            .iter()
            .find(|target| target.id == self.primary_target)
    }

    pub fn primary_target_mut(&mut self) -> Option<&mut ArrayTargetConfig> {
        self.targets
            .iter_mut()
            .find(|target| target.id == self.primary_target)
    }

    pub fn target(&self, target_id: &str) -> Option<&ArrayTargetConfig> {
        self.targets.iter().find(|target| target.id == target_id)
    }

    pub fn target_mut(&mut self, target_id: &str) -> Option<&mut ArrayTargetConfig> {
        self.targets
            .iter_mut()
            .find(|target| target.id == target_id)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArrayTargetConfig {
    pub id: String,
    pub agent: AgentConfig,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent: Option<AgentSubagentConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_update: Option<AgentAutoUpdateConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub install: Option<AgentInstallConfig>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentAutoUpdateConfig {
    #[serde(default, skip_serializing_if = "is_false")]
    pub enabled: bool,
    pub frequency: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSubagentConfig {
    #[serde(default, skip_serializing_if = "is_false")]
    pub disabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<AgentProviderConfig>,
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
    AnthropicMessages,
}

impl CustomProviderApi {
    pub fn as_pi_api(self) -> &'static str {
        match self {
            Self::ChatCompletions => "openai-completions",
            Self::Responses => "openai-responses",
            Self::AnthropicMessages => "anthropic-messages",
        }
    }

    pub fn as_codex_wire_api(self) -> &'static str {
        match self {
            Self::Responses => "responses",
            Self::ChatCompletions => "chat",
            Self::AnthropicMessages => "anthropic",
        }
    }
}

fn default_custom_model_context() -> u64 {
    DEFAULT_CUSTOM_MODEL_CONTEXT
}

fn default_custom_model_output_max_tokens() -> u64 {
    DEFAULT_CUSTOM_MODEL_OUTPUT_MAX_TOKENS
}

fn is_false(value: &bool) -> bool {
    !*value
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

// PERMISSIONS

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

impl PermissionsConfig {
    pub fn effective_request_timeout(&self) -> std::time::Duration {
        let raw = self
            .request_timeout
            .as_deref()
            .unwrap_or(DEFAULT_PERMISSION_REQUEST_TIMEOUT);
        super::validate::primitives::parse_duration_string(raw).unwrap_or_else(|| {
            super::validate::primitives::parse_duration_string(DEFAULT_PERMISSION_REQUEST_TIMEOUT)
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

// COMMANDS

pub const DEFAULT_COMMAND_PROGRESS_INTERVAL: &str = "30s";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandsConfig {
    pub default_timeout: String,
    pub cancel_grace: String,
    #[serde(default = "default_command_progress_interval")]
    pub progress_interval: String,
    #[serde(default)]
    pub env_allowlist: Vec<String>,
    pub max_output_bytes: u64,
}

fn default_command_progress_interval() -> String {
    DEFAULT_COMMAND_PROGRESS_INTERVAL.to_owned()
}

impl Default for CommandsConfig {
    fn default() -> Self {
        Self {
            default_timeout: "10m".to_owned(),
            cancel_grace: "5s".to_owned(),
            progress_interval: default_command_progress_interval(),
            env_allowlist: Vec::new(),
            max_output_bytes: 1_048_576,
        }
    }
}

// PROMPTS

/// Defaults for the stale-prompt sweeper. Tuned for an idle long-running
/// agent: 5 minutes without an ACP `session/update` is well past any
/// reasonable single-token latency, and a 30-second sweep cadence keeps
/// the worst-case "stuck and still listed as running" window bounded
/// without thrashing SQLite. Both values are operator-overridable through
/// `[prompts]` if a deployment streams tokens slowly enough to need a
/// larger threshold.
pub const DEFAULT_PROMPTS_STALE_THRESHOLD: &str = "5m";
pub const DEFAULT_PROMPTS_SWEEP_INTERVAL: &str = "30s";

/// Configuration for the stale-prompt sweeper background task. When no
/// ACP `session/update` notification has touched a `pending`/`running`
/// prompt row for `stale_threshold`, the sweeper flips it to terminal
/// `Stalled` so polling clients always see the row settle. The sweep
/// runs every `sweep_interval` from `acps serve`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptsConfig {
    pub stale_threshold: String,
    pub sweep_interval: String,
}

impl Default for PromptsConfig {
    fn default() -> Self {
        Self {
            stale_threshold: DEFAULT_PROMPTS_STALE_THRESHOLD.to_owned(),
            sweep_interval: DEFAULT_PROMPTS_SWEEP_INTERVAL.to_owned(),
        }
    }
}

impl PromptsConfig {
    /// Parsed `stale_threshold`. Falls back to the schema default rather
    /// than panicking — validation already rejected unparsable values at
    /// load time, so this guard only fires for programmatically
    /// constructed configs that bypass `validate_config`.
    pub fn effective_stale_threshold(&self) -> std::time::Duration {
        super::validate::primitives::parse_duration_string(&self.stale_threshold).unwrap_or_else(
            || {
                super::validate::primitives::parse_duration_string(DEFAULT_PROMPTS_STALE_THRESHOLD)
                    .unwrap_or(std::time::Duration::from_secs(300))
            },
        )
    }

    /// Parsed `sweep_interval`. See `effective_stale_threshold` for the
    /// fallback contract.
    pub fn effective_sweep_interval(&self) -> std::time::Duration {
        super::validate::primitives::parse_duration_string(&self.sweep_interval).unwrap_or_else(
            || {
                super::validate::primitives::parse_duration_string(DEFAULT_PROMPTS_SWEEP_INTERVAL)
                    .unwrap_or(std::time::Duration::from_secs(30))
            },
        )
    }
}

// DEPENDENCIES

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

// LOCAL DAEMON SOCKET

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LocalSessionAuth {
    #[serde(rename = "session-key")]
    #[default]
    SessionKey,
    #[serde(rename = "keyless")]
    Keyless,
}

impl LocalSessionAuth {
    pub fn is_default(value: &Self) -> bool {
        *value == Self::default()
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::SessionKey => "session-key",
            Self::Keyless => "keyless",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalConfig {
    /// Override path for the internal local Unix-domain socket. When unset the
    /// daemon binds `~/.local/share/acp-stack/acps-local.sock`. Override is
    /// intended for integration tests; deployed instances should leave it
    /// unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket_path: Option<String>,
    /// Controls whether local Unix-socket session-tier HTTP routes require an
    /// explicit session key. Public HTTP tiering is unaffected.
    #[serde(default, skip_serializing_if = "LocalSessionAuth::is_default")]
    pub session_auth: LocalSessionAuth,
}

// MCP

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
