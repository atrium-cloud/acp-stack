//! MCP server validation.

use std::collections::HashSet;

use crate::config::schema::{McpConfig, McpServerConfig};
use crate::config::validate::primitives::{
    validate_http_url_prefix, validate_secret_ref_name_value,
};
use crate::error::{Result, StackError};

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
