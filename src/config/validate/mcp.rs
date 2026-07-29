//! MCP server validation.

use std::collections::HashSet;

use crate::config::schema::{HeaderValueSource, McpConfig, McpServerConfig};
use crate::config::secret_template::{
    SecretTemplate, parse_env_entry, screen_env_entry, screen_ref_name, screen_template,
};
use crate::config::validate::primitives::validate_secret_ref_name_value;
use crate::error::{Result, StackError};

const LOOPBACK_HOSTS: [&str; 3] = ["127.0.0.1", "::1", "localhost"];

/// MCP HTTP URLs must be https, or http toward a loopback host (a local
/// relay never leaves the host, so the no-plaintext-off-host rule that
/// motivates https-only is not violated). Shared by config validation and
/// the init declaration paths so a hand-edited config and an init flag obey
/// the same rule.
pub(crate) fn validate_mcp_http_url(field: &'static str, name: &str, url: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(url).map_err(|_| StackError::InvalidParam {
        field,
        reason: format!("MCP HTTP server `{name}` URL is not valid"),
    })?;
    // `host_str()` keeps the brackets around IPv6 literals (`[::1]`).
    let http_loopback = parsed.scheme() == "http"
        && parsed.host_str().is_some_and(|host| {
            LOOPBACK_HOSTS.contains(&host.trim_start_matches('[').trim_end_matches(']'))
        });
    if (parsed.scheme() != "https" || parsed.host_str().is_none()) && !http_loopback {
        return Err(StackError::InvalidParam {
            field,
            reason: format!(
                "MCP HTTP server `{name}` must use an https:// URL with a host (or http:// to a loopback host)"
            ),
        });
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(StackError::InvalidParam {
            field,
            reason: format!("MCP HTTP server `{name}` URL must not include credentials"),
        });
    }
    Ok(())
}

pub(crate) fn validate_mcp(mcp: &McpConfig) -> Result<()> {
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
        validate_server_shape(server)?;
    }
    Ok(())
}

/// Every per-server rule a declaration must pass, including the name check.
/// Shared by the daemon-startup partition below.
fn validate_server(server: &McpServerConfig) -> Result<()> {
    let name = server.name();
    if name.trim().is_empty() {
        return Err(StackError::InvalidMcpServer {
            name: name.to_owned(),
            reason: "name is required",
        });
    }
    validate_server_shape(server)
}

fn validate_server_shape(server: &McpServerConfig) -> Result<()> {
    match server {
        McpServerConfig::Stdio(s) => {
            if s.command.trim().is_empty() {
                return Err(StackError::InvalidMcpServer {
                    name: s.name.clone(),
                    reason: "stdio.command is required",
                });
            }
            super::validate_env_var_names_unique("mcp.servers.env", &s.env)?;
            for env_entry in &s.env {
                parse_env_entry("mcp.servers.env", env_entry)?;
            }
        }
        McpServerConfig::Http(s) => {
            validate_mcp_http_url("mcp.servers.url", &s.name, &s.url)?;
            for header in &s.headers {
                if header.name.trim().is_empty() {
                    return Err(StackError::InvalidMcpServer {
                        name: s.name.clone(),
                        reason: "header.name is required",
                    });
                }
                match header.source()? {
                    HeaderValueSource::Ref(value_ref) => {
                        validate_secret_ref_name_value(value_ref)?;
                    }
                    HeaderValueSource::Template(template) => {
                        SecretTemplate::parse("mcp.servers.headers.value", template)?;
                    }
                }
            }
        }
    }
    Ok(())
}

/// Per-server looks-like-a-secret screening, mirroring the config-wide sweep
/// in `validate_secret_refs_not_looking_like_values`. Shared by that sweep
/// and the daemon-startup partition below: screening must run before any
/// name-shape validation so a screening rejection redacts the offending value
/// while a shape rejection echoes it.
pub(crate) fn screen_server(server: &McpServerConfig) -> Result<()> {
    match server {
        McpServerConfig::Stdio(s) => {
            for env_ref in &s.env {
                screen_env_entry("mcp.servers.env", env_ref)?;
            }
        }
        McpServerConfig::Http(s) => {
            for header in &s.headers {
                if let Some(value_ref) = header.value_ref.as_deref() {
                    screen_ref_name("mcp.servers.headers", value_ref)?;
                }
                if let Some(template) = header.value.as_deref() {
                    screen_template("mcp.servers.headers", template)?;
                }
            }
        }
    }
    Ok(())
}

/// Partition declarations into the servers that pass every per-server rule
/// (keeping the first valid declaration of each name) and those that do not.
/// Daemon startup uses this to degrade: one bad declaration drops just that
/// server — the caller logs a warning per drop — instead of bricking the
/// whole runtime. Screening runs before shape validation so a dropped
/// declaration never echoes a pasted credential into that warning.
/// Cross-server and cross-source rules (a whole-value ref duplicated against
/// `agent.env`, Supabase, etc.) still fail startup: those are config-level
/// conflicts, not one bad declaration. Candidate-config write paths keep the
/// fail-fast [`validate_mcp`] behavior.
pub(crate) fn partition_valid_servers(
    servers: Vec<McpServerConfig>,
) -> (Vec<McpServerConfig>, Vec<(String, String)>) {
    let mut seen = HashSet::new();
    let mut kept = Vec::new();
    let mut dropped = Vec::new();
    for server in servers {
        let name = server.name().to_owned();
        let label = if name.trim().is_empty() {
            "<empty>".to_owned()
        } else {
            name.clone()
        };
        let problem = if seen.contains(&name) {
            Some(
                StackError::DuplicateMcpServer {
                    name: label.clone(),
                }
                .to_string(),
            )
        } else {
            screen_server(&server)
                .and_then(|()| validate_server(&server))
                .err()
                .map(|error| error.to_string())
        };
        match problem {
            Some(reason) => dropped.push((label, reason)),
            None => {
                seen.insert(name);
                kept.push(server);
            }
        }
    }
    (kept, dropped)
}
