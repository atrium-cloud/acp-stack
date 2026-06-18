//! Snapshot/restore primitives for the per-agent headless config files written
//! by `agent_headless_config::provision_agent_headless_config`. Used by the
//! model discovery flow so a failed validation can roll back to the
//! state on disk before provisioning ran.
//!
//! Per-agent list of headless-config files that `provision_agent_headless_config`
//! may write into. Kept in sync with the per-agent provisioners in
//! `agent_headless_config.rs`. Used to snapshot prior contents BEFORE
//! provisioning runs so a failed discovery/validation can roll back to true
//! prior state (a snapshot taken AFTER provision would capture the just-
//! written bytes and "restore" them on rejection, leaking state).
//!
//! Custom-provider variants additionally write a side file —
//! `~/.config/goose/custom_providers/<id>.json` for Goose,
//! `~/.pi/agent/models.json` for Pi — so the candidate list covers those
//! too when a custom provider is configured. Without this, a failed validation
//! on a custom-provider init could leave the side file behind even though
//! `acps-config.toml` was never written.

use std::path::{Path, PathBuf};

use crate::error::{Result, StackError};

pub(super) fn headless_config_candidate_paths(agent_id: &str, home: &Path) -> Vec<PathBuf> {
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
        _ => Vec::new(),
    }
}

/// Per-agent extra directories whose new files should be rolled back on
/// discovery/validation rejection. Today this covers Goose's
/// `custom_providers/` sidecar dir — its file names are operator-supplied
/// (`<provider_id>.json`) so they can't be enumerated up front the way the
/// primary headless-config files can.
pub(super) fn headless_config_side_dirs(agent_id: &str, home: &Path) -> Vec<PathBuf> {
    match agent_id {
        "goose" => vec![home.join(".config").join("goose").join("custom_providers")],
        _ => Vec::new(),
    }
}

/// Capture the existing file names in each given directory before
/// provisioning runs. On rejection, anything new in those directories
/// matching a known provisioner side-effect pattern is removed — covers
/// codex backup files (`~/.codex/config.<provider>.toml`) and Goose
/// custom-provider sidecars (`~/.config/goose/custom_providers/<id>.json`)
/// that the per-agent provisioner writes without exposing the paths through
/// its return value.
pub(super) fn capture_dir_listings_for(
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

pub(super) fn remove_new_files_in_dirs(
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
            // Only remove regular files matching a known side-effect
            // pattern (today: `config.<provider>.toml` backups codex
            // writes alongside the primary config). Skip anything
            // else so a legitimate sibling file written during the
            // short discovery window (logs, lockfiles, the agent's
            // own ephemeral state) is preserved.
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
    // Codex OpenAI backup files: `config.<provider>.toml` or
    // `config.<provider>-<n>.toml`. See `unique_codex_backup_path` in
    // `runtime/agent/agent_headless_config.rs`.
    if name.starts_with("config.") && name.ends_with(".toml") && name != "config.toml" {
        return true;
    }
    // Goose custom-provider sidecar: any `<provider_id>.json` written
    // under `~/.config/goose/custom_providers/`. The operator-supplied
    // provider id can't be enumerated up front, so we match by parent
    // dir name + .json suffix instead.
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

pub(super) fn capture_path_snapshots(paths: &[PathBuf]) -> Result<Vec<(PathBuf, Option<Vec<u8>>)>> {
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

/// Best-effort restore: write each prior content back, or delete the file
/// we created. A restore failure is logged but does not mask the real
/// discovery/validation error — the operator still sees the root cause and
/// can correct it on the next run.
pub(super) fn restore_headless_snapshots(snapshots: Vec<(PathBuf, Option<Vec<u8>>)>) {
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
