//! Snapshot/restore primitives for the per-agent headless config files written
//! by `agent_headless_config::provision_agent_headless_config`. Snapshots MUST
//! be taken BEFORE provisioning: one taken after would capture the just-written
//! bytes and "restore" them on rejection.

use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::runtime::agent::config_io::WorkloadHome;
use crate::workload_fs::EntryKind;

pub(in crate::cli) fn headless_config_candidate_paths(agent_id: &str, home: &Path) -> Vec<PathBuf> {
    match agent_id {
        "goose" => vec![home.join(".config").join("goose").join("config.yaml")],
        "opencode" => vec![home.join(".config").join("opencode").join("opencode.json")],
        "codex" => vec![home.join(".codex").join("config.toml")],
        "claude" => vec![
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
    workload: &WorkloadHome,
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
        let names = workload
            .list_dir_or_empty(&dir)?
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        listings.push((dir, names));
    }
    Ok(listings)
}

pub(in crate::cli) fn remove_new_files_in_dirs(
    workload: &WorkloadHome,
    listings: Vec<(PathBuf, std::collections::HashSet<std::ffi::OsString>)>,
) {
    for (dir, prior_names) in listings {
        let Ok(entries) = workload.list_dir(&dir) else {
            continue;
        };
        for (name, metadata) in entries {
            if prior_names.contains(&name) {
                continue;
            }
            let path = dir.join(&name);
            // Only known side-effect patterns are removed, so a legitimate
            // sibling written during the discovery window survives.
            if metadata.kind == EntryKind::File
                && is_known_provisioner_side_artifact(&dir, &name)
                && let Err(error) = workload.remove_file(&path)
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
    // Codex backup files, per `backup_codex_config`.
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
    workload: &WorkloadHome,
    paths: &[PathBuf],
) -> Result<Vec<(PathBuf, Option<Vec<u8>>)>> {
    let mut snapshots = Vec::with_capacity(paths.len());
    for path in paths {
        snapshots.push((path.clone(), workload.read(path)?));
    }
    Ok(snapshots)
}

/// Best-effort restore of prior contents; a restore failure is logged rather
/// than masking the real discovery/validation error.
pub(in crate::cli) fn restore_headless_snapshots(
    workload: &WorkloadHome,
    snapshots: Vec<(PathBuf, Option<Vec<u8>>)>,
) {
    for (path, prior) in snapshots {
        match prior {
            Some(bytes) => {
                if let Err(error) = workload.write_atomic(&path, bytes) {
                    tracing::warn!(
                        path = %path.display(),
                        error = %error,
                        "failed to restore prior headless config after discovery rejection",
                    );
                }
            }
            None => {
                if let Err(error) = workload.remove_file_if_present(&path) {
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
