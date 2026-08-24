//! MCP server configuration resolver: converts `[mcp.servers]` blocks into the
//! SDK's `McpServer` enum, resolving secret refs against the encrypted store.
//!
//! Resolved secret values go straight to the agent's `session/new` call; they
//! never enter SQLite, an event payload, or leave this resolver beside names.

use agent_client_protocol::schema::v1::{
    EnvVariable, HttpHeader, McpServer, McpServerHttp, McpServerStdio,
};

use crate::config::{
    HeaderValueSource, McpConfig, McpServerConfig, SecretTemplate, resolve_env_entry,
};
use crate::error::{Result, StackError};
use crate::runtime::dependencies::deps::resolve_command_path;
use crate::secrets::SecretStore;

pub fn resolve_mcp_servers(config: &McpConfig, store: &SecretStore) -> Result<Vec<McpServer>> {
    let mut out = Vec::with_capacity(config.servers.len());
    for server in &config.servers {
        match server {
            McpServerConfig::Stdio(stdio) => {
                let mut env_vars = Vec::with_capacity(stdio.env.len());
                for env_entry in &stdio.env {
                    let (var_name, value) = resolve_env_entry("mcp.servers.env", env_entry, store)?;
                    env_vars.push(EnvVariable::new(var_name, value));
                }
                let command = resolve_command_path(&stdio.command)
                    .and_then(|path| path.canonicalize().ok())
                    .ok_or_else(|| StackError::InvalidMcpServer {
                        name: stdio.name.clone(),
                        reason: "stdio.command was not found or is not executable",
                    })?;
                let stdio_server =
                    McpServerStdio::new(stdio.name.clone(), command).args(stdio.args.clone());
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
                    let value = match header.source()? {
                        HeaderValueSource::Ref(value_ref) => store.get(value_ref)?.to_owned(),
                        HeaderValueSource::Template(template) => {
                            SecretTemplate::parse("mcp.servers.headers.value", template)?
                                .resolve(store)?
                        }
                    };
                    headers.push(HttpHeader::new(header.name.clone(), value));
                }
                let http_server =
                    McpServerHttp::new(http.name.clone(), http.url.clone()).headers(headers);
                out.push(McpServer::Http(http_server));
            }
        }
    }
    Ok(out)
}

/// Validate only the secret references used by MCP configuration, so native
/// import can accept a bare command before its executable is installed.
pub(crate) fn validate_mcp_secret_refs(config: &McpConfig, store: &SecretStore) -> Result<()> {
    for server in &config.servers {
        match server {
            McpServerConfig::Stdio(stdio) => {
                for env_entry in &stdio.env {
                    match crate::config::parse_env_entry("mcp.servers.env", env_entry)? {
                        crate::config::EnvEntry::WholeValueRef(name) => {
                            store.get(&name)?;
                        }
                        crate::config::EnvEntry::Templated { template, .. } => {
                            for ref_name in template.ref_names() {
                                store.get(ref_name)?;
                            }
                        }
                    }
                }
            }
            McpServerConfig::Http(http) => {
                for header in &http.headers {
                    match header.source()? {
                        HeaderValueSource::Ref(value_ref) => {
                            store.get(value_ref)?;
                        }
                        HeaderValueSource::Template(template) => {
                            for ref_name in
                                SecretTemplate::parse("mcp.servers.headers.value", template)?
                                    .ref_names()
                            {
                                store.get(ref_name)?;
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Server names passed to a session, for `mcp.session_attached` payloads:
/// names only, never values.
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
                command: "sh".into(),
                args: vec![],
                env: vec!["SLACK_BOT_TOKEN".into()],
            })],
        };
        let servers = resolve_mcp_servers(&config, &store).expect("resolve");
        assert_eq!(servers.len(), 1);
        match &servers[0] {
            McpServer::Stdio(stdio) => {
                assert!(stdio.command.is_absolute());
                assert!(stdio.command.is_file());
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
                headers: vec![HttpHeaderRef::from_ref("Authorization", "LINEAR_API_KEY")],
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
    fn resolves_templated_http_header() {
        let home = TempDir::new().expect("tempdir");
        let store = store_with(&home, &[("PARALLEL_API_KEY", "key-xyz")]);
        let config = McpConfig {
            servers: vec![McpServerConfig::Http(McpHttpServer {
                name: "parallel".into(),
                url: "https://api.example.com/mcp".into(),
                headers: vec![HttpHeaderRef::from_template(
                    "Authorization",
                    "Bearer ${PARALLEL_API_KEY}",
                )],
            })],
        };
        let servers = resolve_mcp_servers(&config, &store).expect("resolve");
        match &servers[0] {
            McpServer::Http(http) => {
                assert_eq!(http.headers[0].name, "Authorization");
                assert_eq!(http.headers[0].value, "Bearer key-xyz");
            }
            _ => panic!("expected http"),
        }
    }

    #[test]
    fn resolves_templated_stdio_env_with_var_name() {
        let home = TempDir::new().expect("tempdir");
        let store = store_with(&home, &[("DB_PASS", "hunter2")]);
        let config = McpConfig {
            servers: vec![McpServerConfig::Stdio(McpStdioServer {
                name: "db".into(),
                command: "sh".into(),
                args: vec![],
                env: vec!["DATABASE_URL=postgres://u:${DB_PASS}@h/db".into()],
            })],
        };
        let servers = resolve_mcp_servers(&config, &store).expect("resolve");
        match &servers[0] {
            McpServer::Stdio(stdio) => {
                assert_eq!(stdio.env[0].name, "DATABASE_URL");
                assert_eq!(stdio.env[0].value, "postgres://u:hunter2@h/db");
            }
            _ => panic!("expected stdio"),
        }
    }

    #[test]
    fn missing_template_ref_propagates_as_secret_not_found() {
        use crate::error::StackError;
        let home = TempDir::new().expect("tempdir");
        let store = SecretStore::open_or_create(home.path()).expect("store");
        let config = McpConfig {
            servers: vec![McpServerConfig::Http(McpHttpServer {
                name: "parallel".into(),
                url: "https://api.example.com/mcp".into(),
                headers: vec![HttpHeaderRef::from_template(
                    "Authorization",
                    "Bearer ${GONE}",
                )],
            })],
        };
        let err = resolve_mcp_servers(&config, &store).expect_err("must fail");
        assert!(matches!(err, StackError::SecretNotFound { .. }), "{err:?}");
    }

    #[test]
    fn secret_ref_validation_covers_template_refs() {
        use crate::error::StackError;
        let home = TempDir::new().expect("tempdir");
        let store = store_with(&home, &[("PRESENT", "value")]);
        let config = McpConfig {
            servers: vec![McpServerConfig::Stdio(McpStdioServer {
                name: "db".into(),
                command: "definitely-not-installed-mcp-12345".into(),
                args: vec![],
                env: vec!["DATABASE_URL=x-${PRESENT}-${ABSENT}".into()],
            })],
        };
        let err = validate_mcp_secret_refs(&config, &store).expect_err("must fail");
        assert!(
            matches!(err, StackError::SecretNotFound { ref name } if name == "ABSENT"),
            "{err:?}"
        );
    }

    #[test]
    fn missing_secret_propagates_as_typed_error() {
        use crate::error::StackError;
        let home = TempDir::new().expect("tempdir");
        let store = SecretStore::open_or_create(home.path()).expect("store");
        let config = McpConfig {
            servers: vec![McpServerConfig::Stdio(McpStdioServer {
                name: "slack".into(),
                command: "sh".into(),
                args: vec![],
                env: vec!["MISSING".into()],
            })],
        };
        let err = resolve_mcp_servers(&config, &store).expect_err("must fail");
        assert!(matches!(err, StackError::SecretNotFound { .. }), "{err:?}");
    }

    #[test]
    fn missing_stdio_executable_is_a_typed_error() {
        use crate::error::StackError;

        let home = TempDir::new().expect("tempdir");
        let store = SecretStore::open_or_create(home.path()).expect("store");
        let config = McpConfig {
            servers: vec![McpServerConfig::Stdio(McpStdioServer {
                name: "missing".into(),
                command: "definitely-not-installed-mcp-12345".into(),
                args: vec![],
                env: vec![],
            })],
        };

        let error = resolve_mcp_servers(&config, &store).expect_err("must fail");
        assert!(
            matches!(
                error,
                StackError::InvalidMcpServer { ref name, reason }
                    if name == "missing"
                        && reason == "stdio.command was not found or is not executable"
            ),
            "{error:?}"
        );
    }

    #[test]
    fn secret_ref_validation_does_not_require_stdio_executable() {
        let home = TempDir::new().expect("tempdir");
        let store = store_with(&home, &[("MCP_TOKEN", "secret")]);
        let config = McpConfig {
            servers: vec![McpServerConfig::Stdio(McpStdioServer {
                name: "portable".into(),
                command: "not-installed-yet".into(),
                args: vec![],
                env: vec!["MCP_TOKEN".into()],
            })],
        };

        validate_mcp_secret_refs(&config, &store).expect("validate secret refs only");
    }
}
