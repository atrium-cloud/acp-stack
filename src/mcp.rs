//! MCP server configuration resolver.
//!
//! `resolve_mcp_servers` converts the project's `[mcp.servers]` config blocks
//! into the SDK's `McpServer` enum, resolving stdio env names and HTTP header
//! `value_ref`s against the encrypted secret store. Secret values are pulled
//! at session create/load/resume time and passed straight to the agent's
//! `session/new` (or load/resume) call — they never enter SQLite, never enter
//! any event payload, and never leave this resolver alongside the names.

use agent_client_protocol::schema::{
    EnvVariable, HttpHeader, McpServer, McpServerHttp, McpServerStdio,
};

use crate::config::{McpConfig, McpServerConfig};
use crate::error::Result;
use crate::secrets::SecretStore;

pub fn resolve_mcp_servers(config: &McpConfig, store: &SecretStore) -> Result<Vec<McpServer>> {
    let mut out = Vec::with_capacity(config.servers.len());
    for server in &config.servers {
        match server {
            McpServerConfig::Stdio(stdio) => {
                let mut env_vars = Vec::with_capacity(stdio.env.len());
                for env_name in &stdio.env {
                    let value = store.get(env_name)?;
                    env_vars.push(EnvVariable::new(env_name.clone(), value.to_owned()));
                }
                let stdio_server = McpServerStdio::new(stdio.name.clone(), stdio.command.clone())
                    .args(stdio.args.clone());
                let stdio_server = if env_vars.is_empty() {
                    stdio_server
                } else {
                    stdio_server.env(env_vars)
                };
                out.push(McpServer::Stdio(stdio_server));
            }
            McpServerConfig::Http(http) => {
                let mut headers = Vec::with_capacity(http.headers.len());
                for header in &http.headers {
                    let value = store.get(&header.value_ref)?;
                    headers.push(HttpHeader::new(header.name.clone(), value.to_owned()));
                }
                let http_server =
                    McpServerHttp::new(http.name.clone(), http.url.clone()).headers(headers);
                out.push(McpServer::Http(http_server));
            }
        }
    }
    Ok(out)
}

/// Build the list of server names being passed to a session. Used by
/// `mcp.session_attached` event payloads so durable logs reflect which
/// declared integrations the session received without leaking values.
pub fn server_names(servers: &[McpServer]) -> Vec<String> {
    servers.iter().map(|s| server_name(s).to_owned()).collect()
}

/// Convenience: name of a single resolved entry (for error messages).
pub fn server_name(server: &McpServer) -> &str {
    match server {
        McpServer::Stdio(s) => &s.name,
        McpServer::Http(s) => &s.name,
        McpServer::Sse(s) => &s.name,
        _ => "<unknown>",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{HttpHeaderRef, McpHttpServer, McpServerConfig, McpStdioServer};
    use tempfile::TempDir;

    fn store_with(home: &TempDir, pairs: &[(&str, &str)]) -> SecretStore {
        let mut store = SecretStore::open_or_create(home.path()).expect("store");
        store.set_many(pairs.iter().copied()).expect("set secrets");
        store
    }

    #[test]
    fn resolves_stdio_env_from_secret_store() {
        let home = TempDir::new().expect("tempdir");
        let store = store_with(&home, &[("SLACK_BOT_TOKEN", "xoxb-123")]);
        let config = McpConfig {
            servers: vec![McpServerConfig::Stdio(McpStdioServer {
                name: "slack".into(),
                command: "slack-mcp".into(),
                args: vec![],
                env: vec!["SLACK_BOT_TOKEN".into()],
            })],
        };
        let servers = resolve_mcp_servers(&config, &store).expect("resolve");
        assert_eq!(servers.len(), 1);
        match &servers[0] {
            McpServer::Stdio(stdio) => {
                assert_eq!(stdio.env.len(), 1);
                assert_eq!(stdio.env[0].name, "SLACK_BOT_TOKEN");
                assert_eq!(stdio.env[0].value, "xoxb-123");
            }
            _ => panic!("expected stdio"),
        }
    }

    #[test]
    fn resolves_http_headers_from_secret_store() {
        let home = TempDir::new().expect("tempdir");
        let store = store_with(&home, &[("LINEAR_API_KEY", "key-xyz")]);
        let config = McpConfig {
            servers: vec![McpServerConfig::Http(McpHttpServer {
                name: "linear".into(),
                url: "https://api.example.com/mcp".into(),
                headers: vec![HttpHeaderRef {
                    name: "Authorization".into(),
                    value_ref: "LINEAR_API_KEY".into(),
                }],
            })],
        };
        let servers = resolve_mcp_servers(&config, &store).expect("resolve");
        match &servers[0] {
            McpServer::Http(http) => {
                assert_eq!(http.headers[0].name, "Authorization");
                assert_eq!(http.headers[0].value, "key-xyz");
            }
            _ => panic!("expected http"),
        }
    }

    #[test]
    fn missing_secret_propagates_as_typed_error() {
        use crate::error::StackError;
        let home = TempDir::new().expect("tempdir");
        let store = SecretStore::open_or_create(home.path()).expect("store");
        let config = McpConfig {
            servers: vec![McpServerConfig::Stdio(McpStdioServer {
                name: "slack".into(),
                command: "slack-mcp".into(),
                args: vec![],
                env: vec!["MISSING".into()],
            })],
        };
        let err = resolve_mcp_servers(&config, &store).expect_err("must fail");
        assert!(matches!(err, StackError::SecretNotFound { .. }), "{err:?}");
    }
}
