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
use crate::config::schema::{McpServerConfig, SupabaseLoggingBackend, SupabaseLoggingConfig};
use crate::error::{Result, StackError};

use self::agent::{
    validate_agent_install, validate_agent_provider, validate_agent_restart,
    validate_agent_subagent,
};
use self::commands::validate_commands;
use self::deps::validate_dependencies;
use self::edge::validate_edge;
use self::mcp::validate_mcp;
use self::permissions::{validate_permissions, validate_trusted_proxies};
use self::primitives::{
    secret_ref_looks_like_value, validate_absolute_path, validate_auth_refs,
    validate_expected_sha256, validate_no_parent_dir_segments, validate_nonzero,
    validate_optional_config_path, validate_secret_ref_name_value, validate_socket_address,
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
    validate_nonzero("api.max_request_bytes", config.api.max_request_bytes)?;
    validate_auth_refs(&config.auth)?;
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
    if let Some(socket_path) = &config.acpctl.socket_path {
        validate_optional_config_path("acpctl.socket_path", socket_path)?;
    }
    validate_code_sources(&config.workspace.code_sources)?;
    validate_data_sources(&config.workspace.data_sources)?;
    if let Some(cwd) = &config.agent.cwd {
        validate_absolute_path("agent.cwd", cwd)?;
    }
    validate_agent_restart(&config.agent.restart)?;
    if let Some(expected_sha256) = &config.agent.expected_sha256 {
        validate_expected_sha256(expected_sha256)?;
    }
    if let Some(install) = &config.agent.install {
        validate_agent_install(install)?;
    }
    if let Some(provider) = &config.agent.provider {
        validate_agent_provider(provider)?;
    }
    if let Some(subagent) = &config.agent.subagent {
        validate_agent_subagent(subagent)?;
    }
    if let Some(mode) = config.agent.mode.as_deref()
        && (mode.trim().is_empty() || mode.len() != mode.trim().len())
    {
        return Err(StackError::MissingField {
            field: "agent.mode",
        });
    }
    if let Some(model) = config.agent.model.as_deref()
        && (model.trim().is_empty() || model.len() != model.trim().len())
    {
        return Err(StackError::MissingField {
            field: "agent.model",
        });
    }
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

    for env_ref in &config.agent.env {
        check(env_ref, "agent.env")?;
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
    check(&config.auth.session_key_ref, "auth.session_key_ref")?;
    check(&config.auth.admin_key_ref, "auth.admin_key_ref")?;
    if let Some(provider) = &config.agent.provider
        && let Some(api_key_ref) = provider.api_key_ref.as_deref()
    {
        check(api_key_ref, "agent.provider.api_key_ref")?;
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
    if !is_safe_table_prefix(&supabase.table_prefix) {
        return Err(StackError::InvalidSupabaseTablePrefix {
            prefix: supabase.table_prefix.clone(),
        });
    }
    if supabase.backend == SupabaseLoggingBackend::Postgres && supabase.db_url_ref.is_none() {
        return Err(StackError::MissingField {
            field: "logging.supabase.db_url_ref",
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
