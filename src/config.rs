//! Config root: the aggregator `Config` struct, top-level constants, the raw
//! deserialization shim, and the public load entry points.

mod schema;
mod secret_template;
mod validate;

use crate::error::{Result, StackError};
use serde::Deserialize;
use std::path::{Path, PathBuf};

pub use self::schema::{
    AcpPromptAction, AgentAdapterConfig, AgentAdapterOverrideArchMap,
    AgentAdapterOverrideArchiveKind, AgentAdapterOverrideConfig, AgentAdapterOverrideGithubInstall,
    AgentAdapterOverrideInstall, AgentAdapterOverrideNpmInstall, AgentAdapterOverrideShellInstall,
    AgentAdapterOverrideUpdate, AgentAutoUpdateConfig, AgentConfig, AgentConfigOptionValue,
    AgentCustomProviderConfig, AgentInstallConfig, AgentProviderConfig, AgentProvidersConfig,
    AgentSubagentConfig, ApiConfig, ArrayConfig, ArrayTargetConfig, CloudflareEdgeConfig,
    CodeSourceConfig, CommandsConfig, CustomProviderApi, DEFAULT_ACP_PROMPT_ACTION,
    DEFAULT_AGENT_AUTO_UPDATE_FREQUENCY, DEFAULT_COMMAND_PROGRESS_INTERVAL,
    DEFAULT_CUSTOM_MODEL_CONTEXT, DEFAULT_CUSTOM_MODEL_OUTPUT_MAX_TOKENS,
    DEFAULT_NETWORK_PROVIDER_TIMEOUT, DEFAULT_PERMISSION_REQUEST_TIMEOUT,
    DEFAULT_PERMISSION_TIMEOUT_ACTION, DEFAULT_PROMPTS_STALE_THRESHOLD,
    DEFAULT_PROMPTS_SWEEP_INTERVAL, DEFAULT_SKILL_SOURCE_BRANCH, DEFAULT_STACK_UPDATE_FREQUENCY,
    DEFAULT_STACK_UPDATE_POLICY, DataSourceConfig, DependenciesConfig, DependencyEntry,
    DependencyInstallAction, DependencyInstallScope, EdgeConfig, ExtensionConfig, ExtensionType,
    HeaderValueSource, HttpHeaderRef, LocalConfig, LocalSessionAuth, LoggingConfig, McpConfig,
    McpHttpServer, McpServerConfig, McpStdioServer, PermissionTimeoutAction, PermissionsConfig,
    PromptsConfig, SandboxConfig, SandboxMode, SandboxProviderStderr, SecurityConfig,
    SecurityHttpConfig, SkillsConfig, StackUpdateConfig, StackUpdatePolicy, SupabaseLoggingBackend,
    SupabaseLoggingConfig, UpdatesConfig, UserSkillSource, WorkspaceConfig,
};
pub use self::secret_template::{
    EnvEntry, SecretTemplate, TemplateSegment, agent_env_declares, env_entry_ref_names_lossy,
    env_entry_var_name, parse_env_entry, ref_names_lossy, resolve_env_entry, screen_env_entry,
    screen_ref_name, screen_template, template_pieces_lossy,
};
pub(crate) use self::validate::agent::{
    AGENT_UPDATE_FREQUENCY_LIMITS, validate_agent_config_options,
};
pub(crate) use self::validate::mcp::validate_mcp_http_url;
pub(crate) use self::validate::primitives::{
    DurationLimits, EndpointUrlProblem, MAX_ENDPOINT_URL_BYTES, check_endpoint_url,
    normalize_duration, validate_secret_ref_name_value,
};
pub use self::validate::primitives::{is_valid_secret_ref_name, parse_duration_string};
pub(crate) use self::validate::skills::{is_valid_github_owner, is_valid_github_repo};
pub(crate) use self::validate::sources::{derive_code_source_name, derive_data_source_name};
pub(crate) use self::validate::{STACK_UPDATE_FREQUENCY_LIMITS, validate_supabase_identifiers};

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = relax_agent_array_requirement)]
pub struct Config {
    /// Config schema version. `1` is the only supported value; every other
    /// value is rejected at load.
    #[serde(default = "default_config_version")]
    #[schemars(extend("const" = 1))]
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
    #[serde(default, skip_serializing_if = "SkillsConfig::is_empty")]
    pub skills: SkillsConfig,
    #[serde(default)]
    pub local: LocalConfig,
    /// Operator-declared extension instances, keyed by operator-chosen name.
    /// See [`ExtensionConfig`].
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub extensions: std::collections::BTreeMap<String, ExtensionConfig>,
}

/// Relax the derived `Config` JSON Schema to the shape the loader actually
/// accepts. schemars derives `required` from the in-memory `Config`, which
/// always carries both `agent` and `array`; but the on-disk file goes through
/// [`RawConfig`], where either section alone is enough. Canonical export writes
/// `[array]` only (`agent` is `#[serde(skip_serializing)]`), legacy files may
/// write `[agent]` only, and both together are accepted. So neither is
/// individually required, but at least one must be present — an `anyOf` the
/// derive cannot express. The finer per-field cross-checks stay in the loader.
fn relax_agent_array_requirement(schema: &mut schemars::Schema) {
    const OPTIONAL_SECTIONS: [&str; 2] = ["agent", "array"];
    let object = schema.ensure_object();
    if let Some(serde_json::Value::Array(required)) = object.get_mut("required") {
        required.retain(|field| {
            field
                .as_str()
                .is_none_or(|name| !OPTIONAL_SECTIONS.contains(&name))
        });
    }
    object.insert(
        "anyOf".to_owned(),
        serde_json::json!([
            { "required": ["agent"] },
            { "required": ["array"] },
        ]),
    );
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
    skills: Option<SkillsConfig>,
    #[serde(default)]
    local: Option<LocalConfig>,
    #[serde(default)]
    extensions: Option<std::collections::BTreeMap<String, ExtensionConfig>>,
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

    /// Lenient path load for day-2 management surfaces (the skills routes and
    /// CLI). Individually invalid MCP server and skill source declarations are
    /// dropped exactly as the running daemon dropped them at boot, so one bad
    /// hand-edited entry cannot brick the surface that would repair it.
    pub(crate) fn load_lenient_from_path(path: impl AsRef<Path>) -> Result<Self> {
        load_for_runtime_reload(path)
    }

    pub(crate) fn load_lenient_from_default_path() -> Result<Self> {
        Self::load_lenient_from_path(default_config_path()?)
    }

    /// Like [`Config::load_lenient_from_path`], but also reports what was
    /// dropped. Write paths that canonicalize this view back to disk must use
    /// this variant and warn per dropped entry — healing a hand-edited invalid
    /// declaration out of the file silently would be an untraceable mutation.
    pub(crate) fn load_lenient_from_path_reporting(
        path: impl AsRef<Path>,
    ) -> Result<(Self, DroppedDeclarations)> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path).map_err(|source| StackError::ConfigRead {
            path: path.to_path_buf(),
            source,
        })?;
        let (loaded, dropped) = lenient_config_from_str_reporting(&content, false)?;
        Ok((loaded.config, dropped))
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

fn has_removed_sandbox_network_table(input: &str) -> bool {
    input.lines().any(|line| {
        let trimmed = line.trim_start();
        trimmed.starts_with("[workspace.sandbox.network]")
            || trimmed.starts_with("[workspace.sandbox.network.")
    })
}

pub fn default_config_path() -> Result<PathBuf> {
    Ok(crate::fs_util::home_dir()?
        .join(".config")
        .join("acp-stack")
        .join("acps-config.toml"))
}

pub fn load_config_from_str(input: &str) -> Result<Config> {
    Ok(load_config_from_str_with_legacy(input)?.config)
}

pub(crate) fn load_config_from_str_with_legacy(input: &str) -> Result<LoadedConfig> {
    let loaded = parse_config_from_str_with_legacy(input)?;
    loaded.config.validate()?;
    Ok(loaded)
}

/// Daemon-startup load. Like [`Config::load_from_path_with_legacy`], but an
/// MCP server or skill source declaration that fails per-entry validation
/// degrades to a skipped entry plus a startup warning instead of failing the
/// boot: the daemon is long-running and one bad peripheral declaration must
/// not brick it. Config syntax and every other rule still fail fast, as do
/// all candidate-config write paths, which go through the strict loaders.
pub(crate) fn load_for_serve(path: impl AsRef<Path>) -> Result<LoadedConfig> {
    let path = path.as_ref();
    let content = std::fs::read_to_string(path).map_err(|source| StackError::ConfigRead {
        path: path.to_path_buf(),
        source,
    })?;
    load_from_str_for_serve(&content)
}

/// Runtime reload of the on-disk config after startup. Same degradation as
/// [`load_for_serve`] but quiet: startup already warned about any dropped
/// declaration, and re-warning on every reload would spam the log once per
/// API request.
pub(crate) fn load_for_runtime_reload(path: impl AsRef<Path>) -> Result<Config> {
    let path = path.as_ref();
    let content = std::fs::read_to_string(path).map_err(|source| StackError::ConfigRead {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(lenient_config_from_str(&content, false)?.config)
}

pub(crate) fn load_from_str_for_serve(input: &str) -> Result<LoadedConfig> {
    lenient_config_from_str(input, true)
}

/// Declarations a lenient load dropped, as `(name, reason)` pairs. Read paths
/// may ignore this; a write path that canonicalizes a lenient view back to
/// disk must warn per entry, because the write erases them from the file.
pub(crate) struct DroppedDeclarations {
    pub mcp_servers: Vec<(String, String)>,
    pub skill_sources: Vec<(String, String)>,
}

fn lenient_config_from_str(input: &str, log_drops: bool) -> Result<LoadedConfig> {
    Ok(lenient_config_from_str_reporting(input, log_drops)?.0)
}

fn lenient_config_from_str_reporting(
    input: &str,
    log_drops: bool,
) -> Result<(LoadedConfig, DroppedDeclarations)> {
    let mut loaded = parse_config_from_str_with_legacy(input)?;
    let (kept, dropped_servers) = self::validate::mcp::partition_valid_servers(std::mem::take(
        &mut loaded.config.mcp.servers,
    ));
    if log_drops {
        for (name, reason) in &dropped_servers {
            tracing::warn!(server = %name, %reason, "skipping invalid MCP server declaration");
        }
    }
    loaded.config.mcp.servers = kept;
    let (kept, dropped_sources) = self::validate::skills::partition_valid_sources(std::mem::take(
        &mut loaded.config.skills.sources,
    ));
    if log_drops {
        for (alias, reason) in &dropped_sources {
            tracing::warn!(alias = %alias, %reason, "skipping invalid skill source declaration");
        }
    }
    loaded.config.skills.sources = kept;
    loaded.config.validate()?;
    Ok((
        loaded,
        DroppedDeclarations {
            mcp_servers: dropped_servers,
            skill_sources: dropped_sources,
        },
    ))
}

fn parse_config_from_str_with_legacy(input: &str) -> Result<LoadedConfig> {
    // Surface targeted migration messages for removed tables before serde reports
    // an unhelpful `unknown field`.
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
    if has_removed_sandbox_network_table(input) {
        return Err(StackError::InvalidParam {
            field: "workspace.sandbox.network",
            reason: "`[workspace.sandbox.network]` moved to the extensions framework; declare \
                 `[extensions.<name>]` with `type = \"network-provider\"` instead (see \
                 docs/specs/extensions.md)"
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
        skills: raw.skills.unwrap_or_default(),
        local: raw.local.unwrap_or_default(),
        extensions: raw.extensions.unwrap_or_default(),
    };

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

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_CONFIG: &str = include_str!("../tests/fixtures/valid-opencode-stack.toml");

    fn config_with_mcp(servers_toml: &str) -> String {
        VALID_CONFIG.replace("[agent]", &format!("{servers_toml}\n[agent]"))
    }

    fn server_names(loaded: &LoadedConfig) -> Vec<String> {
        loaded
            .config
            .mcp
            .servers
            .iter()
            .map(|server| server.name().to_owned())
            .collect()
    }

    fn config_with_block(block: &str) -> String {
        VALID_CONFIG.replace("[agent]", &format!("{block}\n[agent]"))
    }

    const SKILLS_SOURCE_BLOCK: &str = concat!(
        "[[skills.sources]]\n",
        "alias = \"my-org\"\n",
        "github = \"my-org/skills\"\n",
        "branch = \"dev\"\n",
        "trusted = true\n\n",
    );

    #[test]
    fn skills_sources_parse_and_round_trip() {
        let config = load_config_from_str(&config_with_block(SKILLS_SOURCE_BLOCK)).expect("load");
        assert_eq!(config.skills.sources.len(), 1);
        let source = &config.skills.sources[0];
        assert_eq!(source.alias, "my-org");
        assert_eq!(source.github, "my-org/skills");
        assert_eq!(source.branch, "dev");
        assert!(source.trusted);

        let canonical = config.to_canonical_toml().expect("canonical");
        let reloaded = load_config_from_str(&canonical).expect("reload");
        assert_eq!(config.skills, reloaded.skills);
    }

    #[test]
    fn skills_source_branch_defaults_to_main() {
        let block = "[[skills.sources]]\nalias = \"my-org\"\ngithub = \"my-org/skills\"\n\n";
        let config = load_config_from_str(&config_with_block(block)).expect("load");
        assert_eq!(config.skills.sources[0].branch, "main");
        assert!(!config.skills.sources[0].trusted);
    }

    #[test]
    fn duplicate_skill_source_alias_rejected() {
        let block = concat!(
            "[[skills.sources]]\nalias = \"dup\"\ngithub = \"a/skills\"\n\n",
            "[[skills.sources]]\nalias = \"dup\"\ngithub = \"b/skills\"\n\n",
        );
        assert!(load_config_from_str(&config_with_block(block)).is_err());
    }

    #[test]
    fn invalid_skill_source_github_rejected() {
        let block = "[[skills.sources]]\nalias = \"my-org\"\ngithub = \"not-a-repo\"\n\n";
        assert!(load_config_from_str(&config_with_block(block)).is_err());
    }

    #[test]
    fn invalid_skill_source_alias_rejected() {
        let block = "[[skills.sources]]\nalias = \"Bad_Alias\"\ngithub = \"a/skills\"\n\n";
        assert!(load_config_from_str(&config_with_block(block)).is_err());
    }

    #[test]
    fn invalid_skill_source_branch_rejected() {
        // A branch is interpolated raw into the archive URL.
        for branch in ["main?x", "a#b", "../evil", "feature\\x", "/leading"] {
            let block = format!(
                "[[skills.sources]]\nalias = \"my-org\"\ngithub = \"my-org/skills\"\nbranch = \"{branch}\"\n\n"
            );
            assert!(
                load_config_from_str(&config_with_block(&block)).is_err(),
                "branch `{branch}` should be rejected"
            );
        }
    }

    #[test]
    fn skill_source_owner_stricter_than_repo() {
        // Config must reject what the installer's fetch path rejects, or a
        // persisted source would be permanently unusable.
        for github in [
            "my_org/skills",
            "my.org/skills",
            &format!("{}/skills", "a".repeat(40)),
        ] {
            let block =
                format!("[[skills.sources]]\nalias = \"my-org\"\ngithub = \"{github}\"\n\n");
            assert!(
                load_config_from_str(&config_with_block(&block)).is_err(),
                "github `{github}` should be rejected"
            );
        }
        let block = "[[skills.sources]]\nalias = \"my-org\"\ngithub = \"my-org/my_repo.rs\"\n\n";
        assert!(load_config_from_str(&config_with_block(block)).is_ok());
    }

    #[test]
    fn dot_skill_source_repo_rejected() {
        for github in ["my-org/.", "my-org/..", "./skills"] {
            let block =
                format!("[[skills.sources]]\nalias = \"my-org\"\ngithub = \"{github}\"\n\n");
            assert!(
                load_config_from_str(&config_with_block(&block)).is_err(),
                "github `{github}` should be rejected"
            );
        }
    }

    #[test]
    fn overlong_skill_source_alias_rejected() {
        let ok_block = format!(
            "[[skills.sources]]\nalias = \"{}\"\ngithub = \"a/skills\"\n\n",
            "a".repeat(64)
        );
        assert!(load_config_from_str(&config_with_block(&ok_block)).is_ok());
        let long_block = format!(
            "[[skills.sources]]\nalias = \"{}\"\ngithub = \"a/skills\"\n\n",
            "a".repeat(65)
        );
        assert!(load_config_from_str(&config_with_block(&long_block)).is_err());
    }

    #[test]
    fn serve_load_drops_invalid_skill_sources() {
        let input = config_with_block(concat!(
            "[[skills.sources]]\nalias = \"good\"\ngithub = \"my-org/skills\"\n\n",
            "[[skills.sources]]\nalias = \"Bad_Alias\"\ngithub = \"a/skills\"\n\n",
            "[[skills.sources]]\nalias = \"good\"\ngithub = \"b/skills\"\n\n",
            "[[skills.sources]]\nalias = \"badbranch\"\ngithub = \"c/skills\"\nbranch = \"../evil\"\n\n",
        ));
        assert!(load_config_from_str(&input).is_err());
        let loaded = load_from_str_for_serve(&input).expect("serve load degrades");
        let aliases: Vec<&str> = loaded
            .config
            .skills
            .sources
            .iter()
            .map(|source| source.alias.as_str())
            .collect();
        assert_eq!(aliases, vec!["good"]);
    }

    #[test]
    fn serve_load_keeps_later_valid_skill_source_when_first_of_alias_is_invalid() {
        let input = config_with_block(concat!(
            "[[skills.sources]]\nalias = \"dup\"\ngithub = \"not-a-repo\"\n\n",
            "[[skills.sources]]\nalias = \"dup\"\ngithub = \"a/skills\"\n\n",
        ));
        let loaded = load_from_str_for_serve(&input).expect("valid later declaration survives");
        assert_eq!(loaded.config.skills.sources.len(), 1);
        assert_eq!(loaded.config.skills.sources[0].github, "a/skills");
    }

    #[test]
    fn serve_load_drops_non_loopback_http_server() {
        let input = config_with_mcp(concat!(
            "[[mcp.servers]]\ntype = \"http\"\nname = \"good\"\nurl = \"https://mcp.example/mcp\"\n\n",
            "[[mcp.servers]]\ntype = \"http\"\nname = \"plain\"\nurl = \"http://mcp.example.com/mcp\"\n\n",
        ));
        assert!(load_config_from_str(&input).is_err());
        let loaded = load_from_str_for_serve(&input).expect("serve load degrades");
        assert_eq!(server_names(&loaded), vec!["good"]);
    }

    #[test]
    fn serve_load_drops_duplicate_and_malformed_servers() {
        let input = config_with_mcp(concat!(
            "[[mcp.servers]]\ntype = \"stdio\"\nname = \"db\"\ncommand = \"db-mcp\"\n\n",
            "[[mcp.servers]]\ntype = \"stdio\"\nname = \"db\"\ncommand = \"other\"\n\n",
            "[[mcp.servers]]\ntype = \"stdio\"\nname = \"broken\"\ncommand = \"\"\n\n",
            "[[mcp.servers]]\ntype = \"stdio\"\nname = \"dupenv\"\ncommand = \"x\"\nenv = [\"A\", \"A\"]\n\n",
        ));
        let loaded = load_from_str_for_serve(&input).expect("serve load degrades");
        assert_eq!(server_names(&loaded), vec!["db"]);
    }

    #[test]
    fn serve_load_keeps_later_valid_server_when_first_of_name_is_invalid() {
        let input = config_with_mcp(concat!(
            "[[mcp.servers]]\ntype = \"stdio\"\nname = \"db\"\ncommand = \"\"\n\n",
            "[[mcp.servers]]\ntype = \"stdio\"\nname = \"db\"\ncommand = \"db-mcp\"\n\n",
        ));
        let loaded = load_from_str_for_serve(&input).expect("valid later declaration survives");
        assert_eq!(server_names(&loaded), vec!["db"]);
    }

    #[test]
    fn serve_load_drops_screening_tripping_server() {
        let input = config_with_mcp(concat!(
            "[[mcp.servers]]\ntype = \"http\"\nname = \"s\"\nurl = \"https://x.example/mcp\"\n",
            "headers = [{ name = \"Authorization\", value_ref = \"sk-livekey-abc-def\" }]\n\n",
            "[[mcp.servers]]\ntype = \"stdio\"\nname = \"ok\"\ncommand = \"sh\"\n\n",
        ));
        let loaded = load_from_str_for_serve(&input).expect("screening drop degrades");
        assert_eq!(server_names(&loaded), vec!["ok"]);
    }

    #[test]
    fn serve_load_keeps_loopback_http_server() {
        let input = config_with_mcp(
            "[[mcp.servers]]\ntype = \"http\"\nname = \"relay\"\nurl = \"http://127.0.0.1:8787/mcp\"\n\n",
        );
        let loaded = load_from_str_for_serve(&input).expect("loopback relay loads");
        assert_eq!(server_names(&loaded), vec!["relay"]);
    }

    #[test]
    fn serve_load_still_fails_on_non_mcp_errors() {
        assert!(load_from_str_for_serve("not toml = [").is_err());
        assert!(load_from_str_for_serve("[agent]\nid = \"x\"\n").is_err());
    }
}
