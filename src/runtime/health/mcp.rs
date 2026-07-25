//! MCP health collectors.
//!
//! Each configured server is checked without any network probe: stdio servers
//! resolve their command on `PATH`, and every server's secret refs are matched
//! against the names present in the secret store.

use super::*;

pub(super) fn mcp_secret_store_paths(config_path: &Path, state_path: &Path) -> (PathBuf, PathBuf) {
    let config_dir = config_path.parent().unwrap_or_else(|| Path::new("."));
    let state_dir = state_path.parent().unwrap_or_else(|| Path::new("."));
    (config_dir.join("age.key"), state_dir.join("secrets.age"))
}

pub(super) fn collect_mcp(config: &McpConfig, secret_paths: &(PathBuf, PathBuf)) -> McpHealth {
    let required_refs = mcp_secret_refs(config);
    let (secret_names, secret_probe_reason) = if required_refs.is_empty() {
        (BTreeSet::new(), None)
    } else {
        match SecretStore::open_at_paths(&secret_paths.0, &secret_paths.1) {
            Ok(store) => (
                store.list_names().into_iter().map(str::to_owned).collect(),
                None,
            ),
            Err(err) => {
                tracing::warn!(error = %err, "MCP health could not inspect secret store");
                (BTreeSet::new(), Some("secret store unavailable".to_owned()))
            }
        }
    };
    let servers: Vec<_> = config
        .servers
        .iter()
        .map(|server| collect_mcp_server(server, &secret_names, secret_probe_reason.as_deref()))
        .collect();
    let failing_count = servers.iter().filter(|server| !server.ok).count();
    McpHealth {
        configured_count: config.servers.len(),
        failing_count,
        servers,
    }
}

fn mcp_secret_refs(config: &McpConfig) -> BTreeSet<String> {
    let mut refs = BTreeSet::new();
    for server in &config.servers {
        match server {
            McpServerConfig::Stdio(stdio) => {
                refs.extend(stdio.env.iter().cloned());
            }
            McpServerConfig::Http(http) => {
                refs.extend(http.headers.iter().map(|header| header.value_ref.clone()));
            }
        }
    }
    refs
}

fn collect_mcp_server(
    server: &McpServerConfig,
    secret_names: &BTreeSet<String>,
    secret_probe_reason: Option<&str>,
) -> McpServerHealth {
    match server {
        McpServerConfig::Stdio(stdio) => {
            let command_path = resolve_command_path(&stdio.command)
                .map(|path| path.to_string_lossy().into_owned());
            let missing_secret_refs = missing_refs(&stdio.env, secret_names, secret_probe_reason);
            let reason = if command_path.is_none() {
                Some(format!(
                    "`{}` not found or not executable on PATH",
                    stdio.command
                ))
            } else {
                secret_probe_reason
                    .filter(|_| !missing_secret_refs.is_empty())
                    .map(str::to_owned)
            };
            McpServerHealth {
                name: stdio.name.clone(),
                kind: "stdio".to_owned(),
                ok: command_path.is_some() && missing_secret_refs.is_empty(),
                command_path,
                missing_secret_refs,
                reason,
            }
        }
        McpServerConfig::Http(http) => {
            let refs: Vec<String> = http
                .headers
                .iter()
                .map(|header| header.value_ref.clone())
                .collect();
            let missing_secret_refs = missing_refs(&refs, secret_names, secret_probe_reason);
            let reason = secret_probe_reason
                .filter(|_| !missing_secret_refs.is_empty())
                .map(str::to_owned);
            McpServerHealth {
                name: http.name.clone(),
                kind: "http".to_owned(),
                ok: missing_secret_refs.is_empty(),
                command_path: None,
                missing_secret_refs,
                reason,
            }
        }
    }
}

fn missing_refs(
    refs: &[String],
    secret_names: &BTreeSet<String>,
    secret_probe_reason: Option<&str>,
) -> Vec<String> {
    if secret_probe_reason.is_some() {
        return refs.to_vec();
    }
    refs.iter()
        .filter(|name| !secret_names.contains(name.as_str()))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{HttpHeaderRef, McpHttpServer, McpStdioServer};

    fn empty_secret_paths(home: &tempfile::TempDir) -> (PathBuf, PathBuf) {
        (
            crate::secrets::age_key_path(home.path()),
            crate::secrets::secret_store_path(home.path()),
        )
    }

    fn secret_paths_with(home: &tempfile::TempDir, pairs: &[(&str, &str)]) -> (PathBuf, PathBuf) {
        let mut store = SecretStore::open_or_create(home.path()).expect("secret store");
        store.set_many(pairs.iter().copied()).expect("set secrets");
        empty_secret_paths(home)
    }

    #[test]
    fn collect_mcp_with_no_servers_is_healthy() {
        let home = tempfile::tempdir().expect("tempdir");
        let health = collect_mcp(&McpConfig::default(), &empty_secret_paths(&home));
        assert_eq!(health.configured_count, 0);
        assert_eq!(health.failing_count, 0);
        assert!(health.servers.is_empty());
    }

    #[test]
    fn collect_mcp_stdio_reports_command_path() {
        let home = tempfile::tempdir().expect("tempdir");
        let config = McpConfig {
            servers: vec![McpServerConfig::Stdio(McpStdioServer {
                name: "local".to_owned(),
                command: "sh".to_owned(),
                args: vec![],
                env: vec![],
            })],
        };
        let health = collect_mcp(&config, &empty_secret_paths(&home));
        assert_eq!(health.failing_count, 0);
        assert!(health.servers[0].ok);
        assert!(health.servers[0].command_path.is_some());
    }

    #[test]
    fn collect_mcp_stdio_missing_command_fails() {
        let home = tempfile::tempdir().expect("tempdir");
        let config = McpConfig {
            servers: vec![McpServerConfig::Stdio(McpStdioServer {
                name: "missing".to_owned(),
                command: "definitely-not-a-real-mcp-command-12345".to_owned(),
                args: vec![],
                env: vec![],
            })],
        };
        let health = collect_mcp(&config, &empty_secret_paths(&home));
        assert_eq!(health.failing_count, 1);
        assert!(!health.servers[0].ok);
        assert!(
            health.servers[0]
                .reason
                .as_deref()
                .is_some_and(|r| r.contains("not found"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn collect_mcp_stdio_non_executable_command_fails() {
        use std::os::unix::fs::PermissionsExt;

        let home = tempfile::tempdir().expect("tempdir");
        let command_path = home.path().join("not-executable");
        std::fs::write(&command_path, "#!/bin/sh\n").expect("write marker");
        std::fs::set_permissions(&command_path, std::fs::Permissions::from_mode(0o644))
            .expect("chmod marker");
        let config = McpConfig {
            servers: vec![McpServerConfig::Stdio(McpStdioServer {
                name: "local".to_owned(),
                command: command_path.to_string_lossy().into_owned(),
                args: vec![],
                env: vec![],
            })],
        };
        let health = collect_mcp(&config, &empty_secret_paths(&home));
        assert_eq!(health.failing_count, 1);
        assert!(!health.servers[0].ok);
        assert!(
            health.servers[0]
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("not executable")),
            "{health:?}"
        );
    }

    #[test]
    fn collect_mcp_http_with_present_secret_is_healthy_without_network_probe() {
        let home = tempfile::tempdir().expect("tempdir");
        let paths = secret_paths_with(&home, &[("LINEAR_API_KEY", "lin_123")]);
        let config = McpConfig {
            servers: vec![McpServerConfig::Http(McpHttpServer {
                name: "linear".to_owned(),
                url: "https://mcp.linear.app/mcp".to_owned(),
                headers: vec![HttpHeaderRef {
                    name: "Authorization".to_owned(),
                    value_ref: "LINEAR_API_KEY".to_owned(),
                }],
            })],
        };
        let health = collect_mcp(&config, &paths);
        assert_eq!(health.failing_count, 0);
        assert!(health.servers[0].ok);
        assert!(health.servers[0].missing_secret_refs.is_empty());
    }

    #[test]
    fn collect_mcp_missing_secret_refs_fail_server() {
        let home = tempfile::tempdir().expect("tempdir");
        let paths = secret_paths_with(&home, &[("OTHER", "value")]);
        let config = McpConfig {
            servers: vec![McpServerConfig::Http(McpHttpServer {
                name: "linear".to_owned(),
                url: "https://mcp.linear.app/mcp".to_owned(),
                headers: vec![HttpHeaderRef {
                    name: "Authorization".to_owned(),
                    value_ref: "LINEAR_API_KEY".to_owned(),
                }],
            })],
        };
        let health = collect_mcp(&config, &paths);
        assert_eq!(health.failing_count, 1);
        assert!(!health.servers[0].ok);
        assert_eq!(
            health.servers[0].missing_secret_refs,
            vec!["LINEAR_API_KEY"]
        );
    }
}
