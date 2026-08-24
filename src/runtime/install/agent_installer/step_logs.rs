//! On-disk persistence of installer step stdout/stderr for the audit trail.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::{Result, StackError};

use super::InstallerRowDraft;

/// Write a step's full stdout/stderr under
/// `log_base/<agent_id>/<started_at>/<step>/` and stamp the path onto the row.
/// Fail-fast: no history row may claim a completed run once the audit copy is
/// lost.
pub fn persist_step_logs_to_disk(
    row: &mut InstallerRowDraft,
    agent_id: &str,
    log_base: Option<&Path>,
) -> Result<()> {
    let Some(base) = log_base else {
        return Ok(());
    };
    if row.stdout.is_empty() && row.stderr.is_empty() {
        return Ok(());
    }
    let sanitized_started = sanitize_for_path(&row.started_at);
    let log_dir = base
        .join(sanitize_for_path(agent_id))
        .join(sanitized_started)
        .join(sanitize_for_path(&row.step));
    create_dir_tree_synced(&log_dir)?;
    if !row.stdout.is_empty() {
        write_synced_log_file(&log_dir.join("stdout"), row.stdout.as_bytes())?;
    }
    if !row.stderr.is_empty() {
        write_synced_log_file(&log_dir.join("stderr"), row.stderr.as_bytes())?;
    }
    sync_directory(&log_dir)?;
    row.log_dir = Some(log_dir.to_string_lossy().into_owned());
    Ok(())
}

fn create_dir_tree_synced(path: &Path) -> Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        if current.as_os_str().is_empty() || current == Path::new("/") {
            continue;
        }
        match std::fs::metadata(&current) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                return Err(StackError::AgentInstallerLogPersist {
                    path: current,
                    source: std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        "path exists and is not a directory",
                    ),
                });
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&current).map_err(|source| {
                    StackError::AgentInstallerLogPersist {
                        path: current.clone(),
                        source,
                    }
                })?;
                sync_parent_directory(&current)?;
            }
            Err(source) => {
                return Err(StackError::AgentInstallerLogPersist {
                    path: current,
                    source,
                });
            }
        }
    }
    Ok(())
}

fn write_synced_log_file(path: &Path, body: &[u8]) -> Result<()> {
    let mut file =
        std::fs::File::create(path).map_err(|source| StackError::AgentInstallerLogPersist {
            path: path.to_path_buf(),
            source,
        })?;
    file.write_all(body)
        .map_err(|source| StackError::AgentInstallerLogPersist {
            path: path.to_path_buf(),
            source,
        })?;
    file.sync_all()
        .map_err(|source| StackError::AgentInstallerLogPersist {
            path: path.to_path_buf(),
            source,
        })
}

fn sync_parent_directory(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    sync_directory(parent)
}

fn sync_directory(path: &Path) -> Result<()> {
    let directory =
        std::fs::File::open(path).map_err(|source| StackError::AgentInstallerLogPersist {
            path: path.to_path_buf(),
            source,
        })?;
    directory
        .sync_all()
        .map_err(|source| StackError::AgentInstallerLogPersist {
            path: path.to_path_buf(),
            source,
        })
}

/// Convert an arbitrary string into a path-safe single segment, replacing `/`,
/// `\`, and ASCII control chars with `_`.
fn sanitize_for_path(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\') {
                '_'
            } else {
                c
            }
        })
        .collect()
}
