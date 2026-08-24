//! Top-level config validation orchestrator plus the cross-cutting walkers (the
//! two secret-ref sweeps and the Supabase check); per-domain checks are submodules.

pub mod agent;
pub mod commands;
pub mod deps;
pub mod edge;
pub mod extensions;
pub mod mcp;
pub mod permissions;
pub mod primitives;
pub mod prompts;
pub mod skills;
pub mod sources;

use std::collections::HashSet;
use std::path::Path;

use crate::config::Config;
use crate::config::schema::{
    AgentConfig, HeaderValueSource, McpServerConfig, SupabaseLoggingBackend, SupabaseLoggingConfig,
};
use crate::config::secret_template::{
    EnvEntry, SecretTemplate, env_entry_var_name, parse_env_entry, screen_env_entry,
};
use crate::error::{Result, StackError};

use self::agent::{
    validate_agent_auto_update, validate_agent_install, validate_agent_provider,
    validate_agent_providers, validate_agent_restart, validate_agent_subagent,
};
use self::commands::validate_commands;
use self::deps::validate_dependencies;
use self::edge::validate_edge;
use self::mcp::validate_mcp;
use self::permissions::{validate_permissions, validate_trusted_proxies};
use self::primitives::{
    secret_ref_looks_like_value, validate_absolute_path, validate_expected_sha256,
    validate_no_parent_dir_segments, validate_nonzero, validate_optional_config_path,
    validate_secret_ref_name_value, validate_socket_address,
};
use self::prompts::validate_prompts;
use self::sources::{validate_code_sources, validate_data_sources};

pub(crate) fn validate_config(config: &Config) -> Result<()> {
    if config.config_version != crate::config::SUPPORTED_CONFIG_VERSION {
        return Err(StackError::UnsupportedConfigVersion {
            version: config.config_version,
        });
    }
    validate_socket_address("api.bind", &config.api.bind)?;
    validate_stack_updates(config)?;
    validate_nonzero("api.max_request_bytes", config.api.max_request_bytes)?;
    validate_nonzero(
        "security.http.max_request_bytes",
        config.security.http.max_request_bytes,
    )?;
    validate_nonzero(
        "security.http.rate_limit_per_minute",
        config.security.http.rate_limit_per_minute,
    )?;
    validate_nonzero("security.http.burst", config.security.http.burst)?;
    validate_nonzero(
        "security.http.auth_failures_per_minute",
        config.security.http.auth_failures_per_minute,
    )?;
    self::primitives::validate_duration_field(
        "security.http.auth_block_duration",
        &config.security.http.auth_block_duration,
    )?;
    validate_absolute_path("workspace.root", &config.workspace.root)?;
    validate_absolute_path("workspace.uploads", &config.workspace.uploads)?;
    validate_absolute_path("workspace.default_shell", &config.workspace.default_shell)?;
    validate_nonzero("workspace.max_file_bytes", config.workspace.max_file_bytes)?;
    validate_no_parent_dir_segments("workspace.root", &config.workspace.root)?;
    validate_no_parent_dir_segments("workspace.uploads", &config.workspace.uploads)?;
    // Lexical pre-check only; `..` segments were rejected above, so `starts_with`
    // is sound. Runtime still re-resolves upload destinations against the root.
    if !Path::new(&config.workspace.uploads).starts_with(Path::new(&config.workspace.root)) {
        return Err(StackError::WorkspaceUploadsNotUnderRoot);
    }
    // Reject uploads overlapping the workspace-init source lanes at load time, so
    // upload writes can never collide with source materialization at runtime.
    let root = Path::new(&config.workspace.root);
    let uploads = Path::new(&config.workspace.uploads);
    for lane in [
        crate::runtime::workspace_sources::workspace_init::CODE_LANE_DIR,
        crate::runtime::workspace_sources::workspace_init::DATA_LANE_DIR,
    ] {
        let lane_root = root.join(lane);
        if uploads.starts_with(&lane_root) || lane_root.starts_with(uploads) {
            return Err(StackError::InvalidParam {
                field: "workspace.uploads",
                reason: format!(
                    "`{}` collides with the workspace-init lane `{}`",
                    config.workspace.uploads,
                    lane_root.display()
                ),
            });
        }
    }
    // Fail closed at load: a custom backend with no wrapper would otherwise only
    // fail at the first agent spawn, and mask/allow paths are bind/mount targets.
    let sandbox = &config.workspace.sandbox;
    if sandbox.mode == crate::config::SandboxMode::Custom && sandbox.wrapper.is_empty() {
        return Err(StackError::InvalidParam {
            field: "workspace.sandbox.wrapper",
            reason: "mode = \"custom\" requires a non-empty wrapper argv".to_owned(),
        });
    }
    for path in sandbox.mask_paths.iter().chain(sandbox.allow_paths.iter()) {
        if !Path::new(path).is_absolute() {
            return Err(StackError::InvalidParam {
                field: "workspace.sandbox",
                reason: format!("sandbox mask/allow path `{path}` must be absolute"),
            });
        }
    }
    self::extensions::validate_extensions(config)?;
    if let Some(socket_path) = &config.local.socket_path {
        validate_optional_config_path("local.socket_path", socket_path)?;
    }
    validate_code_sources(&config.workspace.code_sources)?;
    validate_data_sources(&config.workspace.data_sources)?;
    validate_array(config)?;
    validate_custom_provider_api_key_refs(config)?;
    validate_permissions(&config.permissions)?;
    validate_commands(&config.commands)?;
    validate_prompts(&config.prompts)?;
    validate_trusted_proxies(&config.security.http)?;
    validate_edge(&config.edge)?;
    validate_dependencies(&config.dependencies)?;
    self::skills::validate_skills(&config.skills)?;
    // The screening sweep MUST run before any name-shape validation (validate_mcp
    // included): a screening rejection redacts the offending value, a name-shape
    // rejection echoes it, and secret-shaped strings fail name validation.
    validate_secret_refs_not_looking_like_values(config)?;
    validate_mcp(&config.mcp)?;
    validate_secret_refs(config)?;
    validate_supabase_logging(config.logging.supabase.as_ref())?;

    Ok(())
}

fn validate_array(config: &Config) -> Result<()> {
    if config.array.targets.is_empty() {
        return Err(StackError::MissingField {
            field: "array.targets",
        });
    }
    let mut target_ids = HashSet::new();
    let mut agent_ids = HashSet::new();
    let mut primary_seen = false;
    for target in &config.array.targets {
        validate_array_target_id(&target.id)?;
        if target.id != target.agent.id {
            return Err(StackError::InvalidParam {
                field: "array.targets.id",
                reason: format!(
                    "target id `{}` must match agent id `{}`",
                    target.id, target.agent.id
                ),
            });
        }
        if target.id == config.array.primary_target {
            primary_seen = true;
        }
        if !agent_ids.insert(target.agent.id.clone()) {
            return Err(StackError::InvalidParam {
                field: "array.targets.agent.id",
                reason: format!(
                    "duplicate harness `{}`; Array v1 requires different harnesses per target",
                    target.agent.id
                ),
            });
        }
        if !target_ids.insert(target.id.clone()) {
            return Err(StackError::InvalidParam {
                field: "array.targets.id",
                reason: format!("duplicate target id `{}`", target.id),
            });
        }
        // Per-target validation reuses the static `agent.*` field names, so wrap
        // failures with the target id to keep multi-target errors identifiable.
        validate_agent_config(&target.agent).map_err(|err| StackError::InvalidParam {
            field: "array.targets.agent",
            reason: format!("target `{}`: {err}", target.id),
        })?;
    }
    validate_array_target_id(&config.array.primary_target)?;
    if !primary_seen {
        return Err(StackError::InvalidParam {
            field: "array.primary_target",
            reason: "must reference an entry in array.targets".to_owned(),
        });
    }
    Ok(())
}

/// One credential set is stored per custom provider id instance-wide, so the
/// id → api-key-ref binding must be unique across every agent and array target;
/// conflicting refs would make delivery depend on iteration order.
fn validate_custom_provider_api_key_refs(config: &Config) -> Result<()> {
    let mut bindings: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    let agents = std::iter::once(&config.agent).chain(
        config
            .array
            .targets
            .iter()
            .map(|target| &target.agent as &AgentConfig),
    );
    for agent in agents {
        let providers = agent.provider.iter().chain(
            agent
                .subagent
                .iter()
                .filter(|subagent| !subagent.disabled)
                .filter_map(|subagent| subagent.provider.as_ref()),
        );
        for provider in providers {
            if provider.custom.is_none() {
                continue;
            }
            let Some(api_key_ref) = provider.api_key_ref.as_deref() else {
                continue;
            };
            match bindings.insert(provider.id.as_str(), api_key_ref) {
                Some(existing) if existing != api_key_ref => {
                    return Err(StackError::InvalidParam {
                        field: "agent.provider.api_key_ref",
                        reason: format!(
                            "custom provider `{}` is declared with conflicting api_key_ref values `{existing}` and `{api_key_ref}`; one credential set is stored per provider id, so every declaration must share one ref",
                            provider.id
                        ),
                    });
                }
                _ => {}
            }
        }
    }
    Ok(())
}

fn validate_array_target_id(value: &str) -> Result<()> {
    if value.trim().is_empty() || value.len() != value.trim().len() {
        return Err(StackError::MissingField {
            field: "array.targets.id",
        });
    }
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return Err(StackError::MissingField {
            field: "array.targets.id",
        });
    };
    let valid = first.is_ascii_alphanumeric()
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !valid {
        return Err(StackError::InvalidParam {
            field: "array.targets.id",
            reason: format!(
                "`{value}` must start with an ASCII letter or digit and contain only ASCII letters, digits, '-', '_', or '.'"
            ),
        });
    }
    Ok(())
}

fn validate_agent_config(agent: &AgentConfig) -> Result<()> {
    if let Some(cwd) = &agent.cwd {
        validate_absolute_path("agent.cwd", cwd)?;
    }
    validate_agent_restart(&agent.restart)?;
    if let Some(expected_sha256) = &agent.expected_sha256 {
        validate_expected_sha256(expected_sha256)?;
    }
    if let Some(install) = &agent.install {
        validate_agent_install(install)?;
    }
    if let Some(adapter_override) = &agent.adapter_override {
        if agent.install.is_some() {
            return Err(StackError::InvalidParam {
                field: "agent.adapter_override",
                reason: "cannot be combined with [agent.install]; the install escape hatch \
                         replaces the entire registry install"
                    .to_owned(),
            });
        }
        agent::validate_agent_adapter_override(adapter_override)?;
        // The launch command doubles as the adapter identity for install/update, so
        // divergence would launch the bare harness while those lanes track the adapter.
        if agent.command != adapter_override.command || agent.args != adapter_override.args {
            return Err(StackError::InvalidParam {
                field: "agent.command",
                reason: "must match agent.adapter_override.command/args; the launch command \
                         doubles as the adapter identity, so point [agent] command/args at the \
                         designated adapter"
                    .to_owned(),
            });
        }
    }
    if let Some(provider) = &agent.provider {
        validate_agent_provider(&agent.id, provider)?;
    }
    if let Some(providers) = &agent.providers {
        validate_agent_providers(
            &agent.id,
            agent.provider.as_ref(),
            agent.subagent.as_ref(),
            providers,
        )?;
    }
    if agent.model.is_some()
        && agent
            .provider
            .as_ref()
            .and_then(|provider| provider.model.as_ref())
            .is_some()
    {
        return Err(StackError::InvalidParam {
            field: "agent.model",
            reason: "must be omitted when agent.provider.model is set".to_owned(),
        });
    }
    if let Some(subagent) = &agent.subagent {
        validate_agent_subagent(&agent.id, subagent)?;
    }
    if let Some(auto_update) = &agent.auto_update {
        validate_agent_auto_update(auto_update)?;
    }
    if let Some(mode) = agent.mode.as_deref()
        && (mode.trim().is_empty() || mode.len() != mode.trim().len())
    {
        return Err(StackError::MissingField {
            field: "agent.mode",
        });
    }
    if let Some(model) = agent.model.as_deref()
        && (model.trim().is_empty() || model.len() != model.trim().len())
    {
        return Err(StackError::MissingField {
            field: "agent.model",
        });
    }
    if let Some(effort) = agent.effort.as_deref()
        && (effort.trim().is_empty() || effort.len() != effort.trim().len())
    {
        return Err(StackError::MissingField {
            field: "agent.effort",
        });
    }
    agent::validate_agent_config_options(&agent.config_options)?;
    Ok(())
}

/// Stack self-update polls GitHub Releases, so a day is the finest cadence allowed.
pub(crate) const STACK_UPDATE_FREQUENCY_LIMITS: primitives::DurationLimits =
    primitives::DurationLimits::new(
        &[
            primitives::DurationUnit::Day,
            primitives::DurationUnit::Week,
        ],
        std::time::Duration::from_secs(86_400),
    );

fn validate_stack_updates(config: &Config) -> Result<()> {
    self::primitives::normalize_duration(
        "updates.acp_stack.frequency",
        &config.updates.acp_stack.frequency,
        &STACK_UPDATE_FREQUENCY_LIMITS,
    )?;
    Ok(())
}

/// Check every secret-ref name for identifier shape and duplicate declaration.
/// Only whole-value declarations dedupe; refs inside `${NAME}` templates may repeat.
fn validate_secret_refs(config: &Config) -> Result<()> {
    let mut seen: HashSet<String> = HashSet::new();

    let mut record = |name: &str, _kind: &'static str| -> Result<()> {
        validate_secret_ref_name_value(name)?;
        if !seen.insert(name.to_owned()) {
            return Err(StackError::DuplicateSecretRef {
                name: name.to_owned(),
            });
        }
        Ok(())
    };

    // Each Array target is a separate process with its own env namespace, so a ref
    // shared ACROSS targets is allowed: only the primary feeds the global `seen`
    // set, and every other target dedupes only WITHIN itself.
    for target in &config.array.targets {
        validate_env_var_names_unique("agent.env", &target.agent.env)?;
        if target.id == config.array.primary_target {
            for env_ref in &target.agent.env {
                match parse_env_entry("agent.env", env_ref)? {
                    EnvEntry::WholeValueRef(name) => record(&name, "agent.env")?,
                    EnvEntry::Templated { .. } => {}
                }
            }
        } else {
            let mut target_seen: HashSet<String> = HashSet::new();
            for env_ref in &target.agent.env {
                match parse_env_entry("agent.env", env_ref)? {
                    EnvEntry::WholeValueRef(name) => {
                        if !target_seen.insert(name.clone()) {
                            return Err(StackError::DuplicateSecretRef { name });
                        }
                    }
                    EnvEntry::Templated { .. } => {}
                }
            }
        }
    }
    if let Some(supabase) = &config.logging.supabase {
        record(&supabase.api_key_ref, "logging.supabase")?;
        if let Some(db_url_ref) = supabase.db_url_ref.as_deref() {
            record(db_url_ref, "logging.supabase.db_url_ref")?;
        }
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
                    match parse_env_entry("mcp.servers.env", env_ref)? {
                        EnvEntry::WholeValueRef(name) => record(&name, "mcp.servers.env")?,
                        EnvEntry::Templated { .. } => {}
                    }
                }
            }
            McpServerConfig::Http(s) => {
                for header in &s.headers {
                    match header.source()? {
                        HeaderValueSource::Ref(value_ref) => {
                            record(value_ref, "mcp.servers.headers")?;
                        }
                        HeaderValueSource::Template(template) => {
                            SecretTemplate::parse("mcp.servers.headers.value", template)?;
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Backstop against code appending a bare `NAME` entry over an existing
/// `NAME=template`: env var names must be unique within one list.
fn validate_env_var_names_unique(field: &'static str, env: &[String]) -> Result<()> {
    let mut seen: HashSet<&str> = HashSet::new();
    for entry in env {
        let var_name = env_entry_var_name(entry);
        if !seen.insert(var_name) {
            return Err(StackError::DuplicateEnvVarName {
                field,
                name: var_name.to_owned(),
            });
        }
    }
    Ok(())
}

fn validate_secret_refs_not_looking_like_values(config: &Config) -> Result<()> {
    let check = |name: &str, field: &'static str| -> Result<()> {
        if secret_ref_looks_like_value(name) {
            return Err(StackError::SecretRefLooksLikeValue { field });
        }
        Ok(())
    };

    // This sweep runs before any name validation and MUST stay parse-error-free:
    // screening rejections redact the value, name-shape rejections echo it, so an
    // echoing error may only fire once screening has passed.
    for target in &config.array.targets {
        for env_ref in &target.agent.env {
            screen_env_entry("agent.env", env_ref)?;
        }
        if let Some(provider) = &target.agent.provider
            && let Some(api_key_ref) = provider.api_key_ref.as_deref()
        {
            check(api_key_ref, "agent.provider.api_key_ref")?;
        }
        if let Some(subagent) = &target.agent.subagent
            && let Some(provider) = &subagent.provider
            && let Some(api_key_ref) = provider.api_key_ref.as_deref()
        {
            check(api_key_ref, "agent.subagent.provider.api_key_ref")?;
        }
    }
    if let Some(supabase) = &config.logging.supabase {
        check(&supabase.api_key_ref, "logging.supabase.api_key_ref")?;
        if let Some(db_url_ref) = supabase.db_url_ref.as_deref() {
            check(db_url_ref, "logging.supabase.db_url_ref")?;
        }
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
        self::mcp::screen_server(server)?;
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
    validate_supabase_identifiers(&supabase.schema, &supabase.table_prefix)?;
    if supabase.backend == SupabaseLoggingBackend::Postgres && supabase.db_url_ref.is_none() {
        return Err(StackError::MissingField {
            field: "logging.supabase.db_url_ref",
        });
    }
    Ok(())
}

/// Reject Supabase schema/table-prefix values unsafe as Postgres identifiers;
/// `acps logging supabase sql` builds DDL directly from these.
pub(crate) fn validate_supabase_identifiers(schema: &str, table_prefix: &str) -> Result<()> {
    if !is_safe_pg_identifier(schema) {
        return Err(StackError::InvalidSupabaseSchema {
            schema: schema.to_owned(),
        });
    }
    if !is_safe_table_prefix(table_prefix) {
        return Err(StackError::InvalidSupabaseTablePrefix {
            prefix: table_prefix.to_owned(),
        });
    }
    Ok(())
}

/// Postgres unquoted-identifier rules, with uppercase deliberately rejected so the
/// `Content-Profile` header stays lowercase and needs no quoting.
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

fn is_safe_table_prefix(s: &str) -> bool {
    if s.is_empty() {
        return true;
    }
    if s.len() > 32 {
        return false;
    }
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return true;
    };
    (first.is_ascii_lowercase() || first == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}
