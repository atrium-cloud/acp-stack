//! Snapshot/restore primitives for the per-agent headless config files written
//! by `agent_headless_config::provision_agent_headless_config`. Snapshots MUST
//! be taken BEFORE provisioning: one taken after would capture the just-written
//! bytes and "restore" them on rejection.

use std::path::{Path, PathBuf};

use crate::error::{Result, StackError};

pub(in crate::cli) fn headless_config_candidate_paths(agent_id: &str, home: &Path) -> Vec<PathBuf> {
    match agent_id {
        "goose" => vec![home.join(".config").join("goose").join("config.yaml")],
        "opencode" => vec![home.join(".config").join("opencode").join("opencode.json")],
        "codex" => vec![home.join(".codex").join("config.toml")],
        "claude-code" => vec![
            home.join(".claude").join("settings.json"),
            home.join(".claude.json"),
        ],
        "pi" => vec![
            home.join(".pi").join("agent").join("settings.json"),
            home.join(".pi").join("agent").join("models.json"),
        ],
        "antigravity" => vec![
            home.join(".gemini")
                .join("antigravity-acp")
                .join("settings.json"),
        ],
        _ => Vec::new(),
    }
}

/// Per-agent directories holding provisioner side files whose names are
/// operator-supplied, so they cannot be enumerated up front.
pub(in crate::cli) fn headless_config_side_dirs(agent_id: &str, home: &Path) -> Vec<PathBuf> {
    match agent_id {
        "goose" => vec![home.join(".config").join("goose").join("custom_providers")],
        _ => Vec::new(),
    }
}

/// Capture existing file names per directory before provisioning, so anything
/// new matching a known side-effect pattern can be removed on rejection.
pub(in crate::cli) fn capture_dir_listings_for(
    dirs: &[PathBuf],
) -> Result<Vec<(PathBuf, std::collections::HashSet<std::ffi::OsString>)>> {
    use std::collections::HashSet;
    let mut listings = Vec::new();
    let mut seen_dirs: HashSet<PathBuf> = HashSet::new();
    for dir in dirs {
        let dir = dir.clone();
        if !seen_dirs.insert(dir.clone()) {
            continue;
        }
        let mut names: HashSet<std::ffi::OsString> = HashSet::new();
        if dir.is_dir() {
            for entry in std::fs::read_dir(&dir).map_err(|source| StackError::ConfigRead {
                path: dir.clone(),
                source,
            })? {
                let entry = entry.map_err(|source| StackError::ConfigRead {
                    path: dir.clone(),
                    source,
                })?;
                names.insert(entry.file_name());
            }
        }
        listings.push((dir, names));
    }
    Ok(listings)
}

pub(in crate::cli) fn remove_new_files_in_dirs(
    listings: Vec<(PathBuf, std::collections::HashSet<std::ffi::OsString>)>,
) {
    for (dir, prior_names) in listings {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            if prior_names.contains(&name) {
                continue;
            }
            let path = entry.path();
            // Only known side-effect patterns are removed, so a legitimate
            // sibling written during the discovery window survives.
            if path.is_file()
                && is_known_provisioner_side_artifact(&dir, &name)
                && let Err(error) = std::fs::remove_file(&path)
            {
                tracing::warn!(
                    path = %path.display(),
                    error = %error,
                    "failed to remove headless-config side artifact after discovery rejection",
                );
            }
        }
    }
}

fn is_known_provisioner_side_artifact(dir: &Path, name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    // Codex backup files, per `unique_codex_backup_path`.
    if name.starts_with("config.") && name.ends_with(".toml") && name != "config.toml" {
        return true;
    }
    // Goose custom-provider sidecar; the operator-supplied provider id cannot be
    // enumerated, so match by parent dir name plus `.json`.
    if dir
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n == "custom_providers")
        && name.ends_with(".json")
    {
        return true;
    }
    false
}

pub(in crate::cli) fn capture_path_snapshots(
    paths: &[PathBuf],
) -> Result<Vec<(PathBuf, Option<Vec<u8>>)>> {
    let mut snapshots = Vec::with_capacity(paths.len());
    for path in paths {
        let prior = if path.exists() {
            Some(
                std::fs::read(path).map_err(|source| StackError::ConfigRead {
                    path: path.clone(),
                    source,
                })?,
            )
        } else {
            None
        };
        snapshots.push((path.clone(), prior));
    }
    Ok(snapshots)
}

/// Best-effort restore of prior contents; a restore failure is logged rather
/// than masking the real discovery/validation error.
pub(in crate::cli) fn restore_headless_snapshots(snapshots: Vec<(PathBuf, Option<Vec<u8>>)>) {
    for (path, prior) in snapshots {
        match prior {
            Some(bytes) => {
                if let Err(error) = std::fs::write(&path, &bytes) {
                    tracing::warn!(
                        path = %path.display(),
                        error = %error,
                        "failed to restore prior headless config after discovery rejection",
                    );
                }
            }
            None => {
                if path.exists()
                    && let Err(error) = std::fs::remove_file(&path)
                {
                    tracing::warn!(
                        path = %path.display(),
                        error = %error,
                        "failed to remove headless config provisioned for discovery",
                    );
                }
            }
        }
    }
}
