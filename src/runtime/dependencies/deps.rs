//! Declarative dependency checker.
//!
//! `[dependencies]` in the config declares external programs, package names,
//! runtimes, and MCP servers that the operator expects to be available. This
//! module reports their status — but **does not install** anything. Per
//! `docs/specs/api/api.md#dependencies-api`, 0.0.2 reports missing
//! dependencies; broad automatic installation is out of scope.
//!
//! Today only `command` checks are implemented (PATH lookup). Packages and
//! runtimes report `available = false` with a clear `reason`. MCP entries
//! cross-reference `[[mcp.servers]]` so the operator can see which declared
//! integrations have actual server configs.

use std::path::PathBuf;

use serde::Serialize;

use crate::config::{Config, DependencyEntry};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DepKind {
    Command,
    Package,
    Runtime,
    Mcp,
}

#[derive(Debug, Clone, Serialize)]
pub struct DepStatus {
    pub name: String,
    pub kind: DepKind,
    pub required: bool,
    pub available: bool,
    pub path: Option<String>,
    pub feature: Option<String>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DepsReport {
    pub dependencies: Vec<DepStatus>,
}

pub fn check_dependencies(config: &Config) -> DepsReport {
    let mut dependencies = Vec::new();
    for entry in &config.dependencies.commands {
        dependencies.push(check_command(entry));
    }
    for entry in &config.dependencies.packages {
        dependencies.push(unimplemented_status(entry, DepKind::Package));
    }
    for entry in &config.dependencies.runtimes {
        dependencies.push(unimplemented_status(entry, DepKind::Runtime));
    }
    for entry in &config.dependencies.mcp {
        dependencies.push(check_mcp(entry, config));
    }
    DepsReport { dependencies }
}

fn check_command(entry: &DependencyEntry) -> DepStatus {
    match resolve_command_path(&entry.name) {
        Some(path) => DepStatus {
            name: entry.name.clone(),
            kind: DepKind::Command,
            required: entry.required,
            available: true,
            path: Some(path.to_string_lossy().into_owned()),
            feature: entry.feature.clone(),
            reason: None,
        },
        None => DepStatus {
            name: entry.name.clone(),
            kind: DepKind::Command,
            required: entry.required,
            available: false,
            path: None,
            feature: entry.feature.clone(),
            reason: Some(format!(
                "`{}` not found or not executable on PATH",
                entry.name
            )),
        },
    }
}

fn unimplemented_status(entry: &DependencyEntry, kind: DepKind) -> DepStatus {
    let reason = match kind {
        DepKind::Package => "package-check-not-implemented",
        DepKind::Runtime => "runtime-check-not-implemented",
        DepKind::Command => "command-check-not-implemented",
        DepKind::Mcp => "mcp-check-not-implemented",
    };
    DepStatus {
        name: entry.name.clone(),
        kind,
        required: entry.required,
        available: false,
        path: None,
        feature: entry.feature.clone(),
        reason: Some(reason.to_owned()),
    }
}

fn check_mcp(entry: &DependencyEntry, config: &Config) -> DepStatus {
    let configured = config
        .mcp
        .servers
        .iter()
        .any(|server| server.name() == entry.name);
    DepStatus {
        name: entry.name.clone(),
        kind: DepKind::Mcp,
        required: entry.required,
        available: configured,
        path: None,
        feature: entry.feature.clone(),
        reason: if configured {
            None
        } else {
            Some("no matching [[mcp.servers]] entry".to_owned())
        },
    }
}

pub(crate) fn resolve_command_path(command: &str) -> Option<PathBuf> {
    if command.contains('/') {
        let candidate = PathBuf::from(command);
        if executable_file(&candidate) {
            return Some(candidate);
        }
        return None;
    }
    let path_env = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_env) {
        let candidate = dir.join(command);
        if executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn executable_file(path: &std::path::Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        DependenciesConfig, DependencyEntry, McpConfig, McpServerConfig, McpStdioServer,
    };

    fn minimal_config(deps: DependenciesConfig, mcp: McpConfig) -> Config {
        let toml_text = include_str!("../../../tests/fixtures/valid-acp-stack.toml");
        let mut config = crate::config::load_config_from_str(toml_text).expect("config");
        config.dependencies = deps;
        config.mcp = mcp;
        config
    }

    #[test]
    fn command_on_path_reports_available() {
        let deps = DependenciesConfig {
            commands: vec![DependencyEntry {
                name: "sh".to_owned(),
                required: true,
                feature: None,
                install: None,
            }],
            ..Default::default()
        };
        let report = check_dependencies(&minimal_config(deps, McpConfig::default()));
        let entry = &report.dependencies[0];
        assert!(entry.available);
        assert!(entry.path.is_some(), "expected resolved path: {entry:?}");
    }

    #[test]
    fn missing_command_reports_unavailable_with_reason() {
        let deps = DependenciesConfig {
            commands: vec![DependencyEntry {
                name: "definitely-not-installed-12345".to_owned(),
                required: true,
                feature: Some("test".to_owned()),
                install: None,
            }],
            ..Default::default()
        };
        let report = check_dependencies(&minimal_config(deps, McpConfig::default()));
        let entry = &report.dependencies[0];
        assert!(!entry.available);
        assert!(entry.reason.as_deref().unwrap_or("").contains("not found"));
        assert_eq!(entry.feature.as_deref(), Some("test"));
    }

    #[cfg(unix)]
    #[test]
    fn non_executable_command_reports_unavailable() {
        use std::os::unix::fs::PermissionsExt;

        let tempdir = tempfile::tempdir().expect("tempdir");
        let command_path = tempdir.path().join("not-executable");
        std::fs::write(&command_path, "#!/bin/sh\n").expect("write marker");
        std::fs::set_permissions(&command_path, std::fs::Permissions::from_mode(0o644))
            .expect("chmod marker");
        let deps = DependenciesConfig {
            commands: vec![DependencyEntry {
                name: command_path.to_string_lossy().into_owned(),
                required: true,
                feature: None,
                install: None,
            }],
            ..Default::default()
        };
        let report = check_dependencies(&minimal_config(deps, McpConfig::default()));
        let entry = &report.dependencies[0];
        assert!(
            !entry.available,
            "non-executable file must not satisfy command dep"
        );
        assert!(
            entry
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("not executable")),
            "{entry:?}"
        );
    }

    #[test]
    fn mcp_dep_cross_references_servers() {
        let deps = DependenciesConfig {
            mcp: vec![DependencyEntry {
                name: "slack".to_owned(),
                required: false,
                feature: None,
                install: None,
            }],
            ..Default::default()
        };
        let mcp = McpConfig {
            servers: vec![McpServerConfig::Stdio(McpStdioServer {
                name: "slack".to_owned(),
                command: "slack-mcp".to_owned(),
                args: vec![],
                env: vec![],
            })],
        };
        let report = check_dependencies(&minimal_config(deps, mcp));
        assert!(report.dependencies[0].available);
    }

    #[test]
    fn mcp_dep_without_server_reports_missing() {
        let deps = DependenciesConfig {
            mcp: vec![DependencyEntry {
                name: "linear".to_owned(),
                required: true,
                feature: None,
                install: None,
            }],
            ..Default::default()
        };
        let report = check_dependencies(&minimal_config(deps, McpConfig::default()));
        assert!(!report.dependencies[0].available);
    }
}
