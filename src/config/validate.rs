//! Top-level config validation orchestrator.
//!
//! `validate_config` runs every per-domain check in the same order the
//! pre-split monolith did so error messages and test fixtures are
//! preserved verbatim. Cross-cutting walkers that need to traverse the
//! whole `Config` (the two secret-ref sweeps and the small Supabase
//! check) live here; per-domain validators are exposed as submodules.

pub mod agent;
pub mod commands;
pub mod deps;
pub mod edge;
pub mod mcp;
pub mod permissions;
pub mod primitives;
pub mod prompts;
pub mod sources;

use std::collections::HashSet;
use std::path::Path;

use crate::config::Config;
use crate::config::schema::{
    AgentConfig, McpServerConfig, SupabaseLoggingBackend, SupabaseLoggingConfig,
};
use crate::error::{Result, StackError};

use self::agent::{
    validate_agent_auto_update, validate_agent_install, validate_agent_provider,
    validate_agent_restart, validate_agent_subagent,
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
    // Parsed at runtime by `http_hardening`; validate at config load too so a
    // malformed or absurd block window surfaces here, with the shared 1970
    // hardstop applied like every other duration field.
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
    // Lexical pre-check: uploads must live under root. With `..` segments
    // already rejected above, `starts_with` is sound. The runtime layer
    // also re-resolves the upload destination against workspace.root, so a
    // symlink inside the workspace that points outside is caught at write
    // time; this check rejects the obvious misconfiguration up front and
    // keeps `workspace_relative_string` from emitting absolute paths.
    if !Path::new(&config.workspace.uploads).starts_with(Path::new(&config.workspace.root)) {
        return Err(StackError::WorkspaceUploadsNotUnderRoot);
    }
    // `acps init` materializes code/data sources beneath
    // `<workspace.root>/usr/code/` and `<workspace.root>/usr/data/`. If
    // operators point `workspace.uploads` at either lane root (or any
    // ancestor that overlaps), upload write paths can collide with
    // source materialization. Reject the overlap at config-load time so
    // the conflict is impossible to hit at runtime.
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
    if let Some(socket_path) = &config.local.socket_path {
        validate_optional_config_path("local.socket_path", socket_path)?;
    }
    validate_code_sources(&config.workspace.code_sources)?;
    validate_data_sources(&config.workspace.data_sources)?;
    validate_array(config)?;
    validate_permissions(&config.permissions)?;
    validate_commands(&config.commands)?;
    validate_prompts(&config.prompts)?;
    validate_trusted_proxies(&config.security.http)?;
    validate_edge(&config.edge)?;
    validate_dependencies(&config.dependencies)?;
    validate_mcp(&config.mcp)?;
    validate_secret_refs_not_looking_like_values(config)?;
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
        // Per-target agent validation reuses the static `agent.*` field names.
        // Wrap any failure with the target id so a multi-target config still
        // identifies which target's agent block is invalid.
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
    if let Some(provider) = &agent.provider {
        validate_agent_provider(&agent.id, provider)?;
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
    Ok(())
}

fn validate_stack_updates(config: &Config) -> Result<()> {
    self::primitives::normalize_day_or_week_duration(
        "updates.acp_stack.frequency",
        &config.updates.acp_stack.frequency,
    )?;
    Ok(())
}

/// Walk every secret-ref name in the config and ensure the name itself is a
/// syntactically valid identifier and is not declared twice.
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

    // Each Array target is a separate process with its own env namespace, so
    // sharing a secret ref ACROSS targets (e.g. two harnesses both referencing
    // ANTHROPIC_API_KEY) is intentionally allowed. The primary target's env
    // refs still feed the global `seen` set so they are deduped against the
    // other config sources (supabase, mcp, ...). Every other target is deduped
    // only WITHIN itself, so an intra-target duplicate is still caught for each
    // one instead of being silently skipped.
    for target in &config.array.targets {
        if target.id == config.array.primary_target {
            for env_ref in &target.agent.env {
                record(env_ref, "agent.env")?;
            }
        } else {
            let mut target_seen: HashSet<String> = HashSet::new();
            for env_ref in &target.agent.env {
                validate_secret_ref_name_value(env_ref)?;
                if !target_seen.insert(env_ref.clone()) {
                    return Err(StackError::DuplicateSecretRef {
                        name: env_ref.clone(),
                    });
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

fn validate_secret_refs_not_looking_like_values(config: &Config) -> Result<()> {
    let check = |name: &str, field: &'static str| -> Result<()> {
        if secret_ref_looks_like_value(name) {
            return Err(StackError::SecretRefLooksLikeValue { field });
        }
        Ok(())
    };

    for target in &config.array.targets {
        for env_ref in &target.agent.env {
            check(env_ref, "agent.env")?;
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

/// Reject Supabase schema/table-prefix values that are unsafe as Postgres
/// identifiers. Shared by config validation and `acps logging supabase sql`,
/// which builds DDL directly from CLI arguments — including PL/pgSQL
/// `format()` string literals where a stray `'` or `%` would corrupt the
/// generated revoke statements.
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
