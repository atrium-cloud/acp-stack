//! Declarative dependency checker.
//!
//! `[dependencies]` in the config declares external programs, package names,
//! runtimes, and MCP servers that the operator expects to be available. This
//! module reports their status — but **does not install** anything. Per
//! `docs/specs/api/api.md#dependencies-api`, 0.0.2 reports missing
//! dependencies; broad automatic installation is out of scope.
//!
//! Commands and runtimes are checked as executable names on PATH. Linux package
//! entries are checked against local package databases when one is available.
//! MCP entries cross-reference `[[mcp.servers]]` so the operator can see which
//! declared integrations have actual server configs.

use std::path::PathBuf;
use std::process::Command;

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
        dependencies.push(check_package(entry));
    }
    for entry in &config.dependencies.runtimes {
        dependencies.push(check_runtime(entry));
    }
    for entry in &config.dependencies.mcp {
        dependencies.push(check_mcp(entry, config));
    }
    DepsReport { dependencies }
}

fn check_command(entry: &DependencyEntry) -> DepStatus {
    check_executable(entry, DepKind::Command, "command")
}

fn check_runtime(entry: &DependencyEntry) -> DepStatus {
    check_executable(entry, DepKind::Runtime, "runtime")
}

fn check_executable(entry: &DependencyEntry, kind: DepKind, label: &str) -> DepStatus {
    match resolve_command_path(&entry.name) {
        Some(path) => DepStatus {
            name: entry.name.clone(),
            kind,
            required: entry.required,
            available: true,
            path: Some(path.to_string_lossy().into_owned()),
            feature: entry.feature.clone(),
            reason: None,
        },
        None => DepStatus {
            name: entry.name.clone(),
            kind,
            required: entry.required,
            available: false,
            path: None,
            feature: entry.feature.clone(),
            reason: Some(format!(
                "{label} `{}` not found or not executable on PATH",
                entry.name,
            )),
        },
    }
}

#[derive(Debug, Clone, Copy)]
enum PackageCheckerKind {
    Dpkg,
    ExitStatus,
}

#[derive(Debug, Clone, Copy)]
struct PackageChecker {
    command: &'static str,
    args_before_name: &'static [&'static str],
    kind: PackageCheckerKind,
}

const LINUX_PACKAGE_CHECKERS: &[PackageChecker] = &[
    PackageChecker {
        command: "dpkg-query",
        args_before_name: &["-W", "-f=${db:Status-Abbrev}"],
        kind: PackageCheckerKind::Dpkg,
    },
    PackageChecker {
        command: "rpm",
        args_before_name: &["-q", "--quiet"],
        kind: PackageCheckerKind::ExitStatus,
    },
    PackageChecker {
        command: "apk",
        args_before_name: &["info", "-e"],
        kind: PackageCheckerKind::ExitStatus,
    },
    PackageChecker {
        command: "pacman",
        args_before_name: &["-Q"],
        kind: PackageCheckerKind::ExitStatus,
    },
];

fn check_package(entry: &DependencyEntry) -> DepStatus {
    let mut available_checkers = Vec::new();
    for checker in LINUX_PACKAGE_CHECKERS {
        let Some(checker_path) = resolve_command_path(checker.command) else {
            continue;
        };
        available_checkers.push(checker.command);
        match run_package_checker(checker, &checker_path, &entry.name) {
            PackageCheckResult::Available => {
                return DepStatus {
                    name: entry.name.clone(),
                    kind: DepKind::Package,
                    required: entry.required,
                    available: true,
                    path: None,
                    feature: entry.feature.clone(),
                    reason: None,
                };
            }
            PackageCheckResult::Missing => {}
            PackageCheckResult::Failed(reason) => {
                return DepStatus {
                    name: entry.name.clone(),
                    kind: DepKind::Package,
                    required: entry.required,
                    available: false,
                    path: None,
                    feature: entry.feature.clone(),
                    reason: Some(reason),
                };
            }
        }
    }

    let reason = if available_checkers.is_empty() {
        "no supported Linux package database command found on PATH (tried dpkg-query, rpm, apk, pacman)"
            .to_owned()
    } else {
        format!(
            "package `{}` was not reported installed by {}",
            entry.name,
            available_checkers.join(", "),
        )
    };

    DepStatus {
        name: entry.name.clone(),
        kind: DepKind::Package,
        required: entry.required,
        available: false,
        path: None,
        feature: entry.feature.clone(),
        reason: Some(reason),
    }
}

enum PackageCheckResult {
    Available,
    Missing,
    Failed(String),
}

fn run_package_checker(
    checker: &PackageChecker,
    checker_path: &std::path::Path,
    package_name: &str,
) -> PackageCheckResult {
    let output = Command::new(checker_path)
        .args(checker.args_before_name)
        .arg(package_name)
        .output();
    let output = match output {
        Ok(output) => output,
        Err(source) => {
            return PackageCheckResult::Failed(format!(
                "package check with `{}` failed: {source}",
                checker.command,
            ));
        }
    };

    match checker.kind {
        PackageCheckerKind::Dpkg => {
            if output.status.success() && dpkg_status_is_installed(&output.stdout) {
                PackageCheckResult::Available
            } else {
                PackageCheckResult::Missing
            }
        }
        PackageCheckerKind::ExitStatus => {
            if output.status.success() {
                PackageCheckResult::Available
            } else {
                PackageCheckResult::Missing
            }
        }
    }
}

fn dpkg_status_is_installed(stdout: &[u8]) -> bool {
    let text = String::from_utf8_lossy(stdout);
    let mut chars = text.trim_start().chars();
    chars.next();
    matches!(chars.next(), Some('i'))
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
        let toml_text = include_str!("../../../tests/fixtures/valid-opencode-stack.toml");
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

    #[test]
    fn missing_runtime_reports_unavailable_without_placeholder_reason() {
        let deps = DependenciesConfig {
            runtimes: vec![DependencyEntry {
                name: "definitely-not-installed-runtime-12345".to_owned(),
                required: true,
                feature: Some("runtime-test".to_owned()),
                install: None,
            }],
            ..Default::default()
        };
        let report = check_dependencies(&minimal_config(deps, McpConfig::default()));
        let entry = &report.dependencies[0];
        assert_eq!(entry.kind, DepKind::Runtime);
        assert!(!entry.available);
        let reason = entry.reason.as_deref().unwrap_or("");
        assert!(reason.contains("runtime"));
        assert!(!reason.contains("runtime-check-not-implemented"));
        assert_eq!(entry.feature.as_deref(), Some("runtime-test"));
    }

    #[test]
    fn missing_package_reports_unavailable_without_placeholder_reason() {
        let deps = DependenciesConfig {
            packages: vec![DependencyEntry {
                name: "definitely-not-installed-package-12345".to_owned(),
                required: true,
                feature: Some("package-test".to_owned()),
                install: None,
            }],
            ..Default::default()
        };
        let report = check_dependencies(&minimal_config(deps, McpConfig::default()));
        let entry = &report.dependencies[0];
        assert_eq!(entry.kind, DepKind::Package);
        assert!(!entry.available);
        let reason = entry.reason.as_deref().unwrap_or("");
        assert!(!reason.contains("package-check-not-implemented"));
        assert_eq!(entry.feature.as_deref(), Some("package-test"));
    }

    #[test]
    fn dpkg_status_parser_accepts_held_installed_packages() {
        assert!(dpkg_status_is_installed(b"ii "));
        assert!(dpkg_status_is_installed(b"hi "));
        assert!(!dpkg_status_is_installed(b"rc "));
        assert!(!dpkg_status_is_installed(b"iH "));
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
