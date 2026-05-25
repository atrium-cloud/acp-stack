//! On-disk persistence of installer step stdout/stderr for the audit trail.
//!
//! The state-store row keeps only a truncated capture; the full output is
//! written to a per-step directory under `log_base/<agent_id>/<sanitized
//! started_at>/<step>/` and the path is stamped onto the row. Persistence is
//! fail-fast: callers should not append a history row claiming a completed
//! run when the audit copy was lost.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::{Result, StackError};

use super::InstallerRowDraft;

/// Write the unbounded stdout/stderr for a single installer step to a
/// per-step directory under `log_base/<agent_id>/<sanitized started_at>/<step>/`
/// and stamp the path onto the row. Skipped step rows have empty streams,
/// so we don't bother creating a directory in that case. Persistence is
/// fail-fast because the full logs are the audit copy; the caller should not
/// append a history row claiming a completed run when that copy was lost.
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

/// Convert an arbitrary string into a path-safe single segment. Replaces
/// `/`, `\`, and ASCII control chars with `_`. The `agent_id` and `step`
/// values are already safe (alphanumeric and `-`), so this is defense in
/// depth; `started_at` carries `:` which is fine on POSIX but worth keeping
/// readable.
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
