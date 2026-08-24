//! MCP server validation.

use std::collections::HashSet;

use crate::config::schema::{HeaderValueSource, McpConfig, McpServerConfig};
use crate::config::secret_template::{
    SecretTemplate, parse_env_entry, screen_env_entry, screen_ref_name, screen_template,
};
use crate::config::validate::primitives::validate_secret_ref_name_value;
use crate::error::{Result, StackError};

/// The shared endpoint rule with MCP-specific wording. Query strings and fragments stay legal because an MCP endpoint is a full request URL, not an API base.
pub(crate) fn validate_mcp_http_url(field: &'static str, name: &str, url: &str) -> Result<()> {
    use crate::config::{EndpointUrlProblem, check_endpoint_url};

    check_endpoint_url(url, true).map_err(|problem| StackError::InvalidParam {
        field,
        reason: match problem {
            EndpointUrlProblem::Unparseable | EndpointUrlProblem::TooLong => {
                format!("MCP HTTP server `{name}` URL is not valid")
            }
            EndpointUrlProblem::NotHttpsOrLoopback => format!(
                "MCP HTTP server `{name}` must use an https:// URL with a host (or http:// to a loopback host)"
            ),
            EndpointUrlProblem::ContainsCredentials => {
                format!("MCP HTTP server `{name}` URL must not include credentials")
            }
            EndpointUrlProblem::ContainsQueryOrFragment => {
                format!("MCP HTTP server `{name}` URL is not valid")
            }
        },
    })
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

/// Per-server looks-like-a-secret screening. Callers MUST run this before any name-shape validation: a screening rejection redacts the offending value while a shape rejection echoes it.
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

/// Split declarations into those passing every per-server rule and those that do not, so daemon startup drops one bad server instead of bricking the runtime.
/// Cross-server and cross-source conflicts still fail startup; candidate-config write paths keep the fail-fast [`validate_mcp`] behavior.
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
