//! Data lane: local filesystem source. Mirrors a configured path into the
//! workspace data lane, rejecting symlinks at every level so a configured
//! `/data/link -> /etc` cannot smuggle untrusted bytes inside the workspace.

use std::path::Path;

use crate::config::DataSourceConfig;
use crate::error::{Result, StackError};

use super::common::{
    Sentinel, SentinelBody, capture_error, cleanup_partial_destination, ensure_dest_or_fail,
    ensure_destination_not_symlink, sentinel_if_present, write_operation_capture,
};
use super::{CAPTURE_TAG_COPY, MaterializeOutcome, SourceReport};

pub(super) fn materialize_local(
    index: usize,
    source: &DataSourceConfig,
    name: &str,
    dest: &Path,
    log_dir: Option<&Path>,
) -> Result<SourceReport> {
    let path = source
        .path
        .as_deref()
        .ok_or_else(|| StackError::WorkspaceDataSourceInvalid {
            index,
            reason: "path is required".to_owned(),
        })?;
    let src = Path::new(path);
    // Reject a top-level symlink **before** canonicalize follows it.
    // `copy_tree` checks symlinks during the walk, but that runs only
    // after canonicalization has already resolved the configured root; a
    // configured `/data/link -> /etc` would otherwise be copied as if
    // `/etc` were the declared source.
    let src_metadata = std::fs::symlink_metadata(src).map_err(|source_err| {
        StackError::WorkspaceDataSourceInvalid {
            index,
            reason: format!("local path `{path}` is not readable: {source_err}"),
        }
    })?;
    if src_metadata.file_type().is_symlink() {
        return Err(StackError::WorkspaceDataSourceInvalid {
            index,
            reason: format!("local source `{path}` is a symlink; declare the target directly"),
        });
    }
    let canonical_src =
        src.canonicalize()
            .map_err(|source_err| StackError::WorkspaceDataSourceInvalid {
                index,
                reason: format!("local path `{path}` is not readable: {source_err}"),
            })?;

    // Refuse if the destination would end up inside the source tree —
    // copying into a subdirectory of `path` would either loop forever or
    // produce a recursive snapshot of itself. The canonical form of `dest`
    // may not exist yet, so we check against its parent (which we just
    // created or are about to create).
    ensure_destination_not_symlink(dest)?;
    let dest_parent_canonical = dest
        .parent()
        .map(|parent| parent.canonicalize().ok())
        .unwrap_or(None);
    if let Some(parent) = dest_parent_canonical
        && (parent == canonical_src || parent.starts_with(&canonical_src))
    {
        return Err(StackError::WorkspaceDataSourceInvalid {
            index,
            reason: format!(
                "local source path `{path}` is an ancestor of the workspace destination; \
                 the copy would recurse into itself"
            ),
        });
    }

    if let Some(existing) = sentinel_if_present(dest)?
        && let SentinelBody::Local {
            path: existing_path,
            ..
        } = &existing.body
        && existing_path == &canonical_src.display().to_string()
    {
        return Ok(SourceReport {
            name: name.to_owned(),
            destination: dest.to_path_buf(),
            outcome: MaterializeOutcome::Verified,
            log_dir: None,
        });
    }
    ensure_dest_or_fail(dest)?;
    std::fs::create_dir_all(dest).map_err(|source_err| StackError::WorkspaceMaterializeFailed {
        reason: format!("create dest `{}`: {source_err}", dest.display()),
    })?;

    let copy = match copy_tree(&canonical_src, dest) {
        Ok(copy) => copy,
        Err(err) => {
            capture_error(log_dir, CAPTURE_TAG_COPY, &err);
            return Err(cleanup_partial_destination(dest, err));
        }
    };
    write_operation_capture(
        log_dir,
        CAPTURE_TAG_COPY,
        &format!(
            "source={}\ndestination={}\nbytes={}\nentries={}\n",
            canonical_src.display(),
            dest.display(),
            copy.bytes,
            copy.entries,
        ),
        "",
    )
    .map_err(|err| cleanup_partial_destination(dest, err))?;

    let sentinel = Sentinel::new(SentinelBody::Local {
        path: canonical_src.display().to_string(),
        bytes: copy.bytes,
        entries: copy.entries,
    });
    if let Err(err) = sentinel.write(dest) {
        return Err(cleanup_partial_destination(dest, err));
    }

    Ok(SourceReport {
        name: name.to_owned(),
        destination: dest.to_path_buf(),
        outcome: MaterializeOutcome::Created,
        log_dir: log_dir.map(Path::to_path_buf),
    })
}

#[derive(Debug, Default)]
pub(super) struct CopyOutcome {
    pub(super) bytes: u64,
    pub(super) entries: u64,
}

pub(super) fn copy_tree(src: &Path, dest: &Path) -> Result<CopyOutcome> {
    let mut outcome = CopyOutcome::default();
    let metadata = std::fs::symlink_metadata(src).map_err(|source_err| {
        StackError::WorkspaceMaterializeFailed {
            reason: format!("stat `{}`: {source_err}", src.display()),
        }
    })?;
    if metadata.file_type().is_symlink() {
        return Err(StackError::WorkspaceMaterializeFailed {
            reason: format!(
                "local source `{}` is a symlink; refusing to follow",
                src.display()
            ),
        });
    }
    if metadata.is_file() {
        let target_name = src.file_name().map(|s| s.to_owned()).ok_or_else(|| {
            StackError::WorkspaceMaterializeFailed {
                reason: format!("local source `{}` has no file name", src.display()),
            }
        })?;
        let target = dest.join(target_name);
        let bytes = std::fs::copy(src, &target).map_err(|source_err| {
            StackError::WorkspaceMaterializeFailed {
                reason: format!(
                    "copy `{}` -> `{}`: {source_err}",
                    src.display(),
                    target.display()
                ),
            }
        })?;
        outcome.bytes = bytes;
        outcome.entries = 1;
        return Ok(outcome);
    }
    if !metadata.is_dir() {
        return Err(StackError::WorkspaceMaterializeFailed {
            reason: format!(
                "local source `{}` is neither a regular file nor a directory",
                src.display()
            ),
        });
    }
    copy_dir_recursive(src, dest, &mut outcome)?;
    Ok(outcome)
}

pub(super) fn copy_dir_recursive(src: &Path, dest: &Path, outcome: &mut CopyOutcome) -> Result<()> {
    for entry in
        std::fs::read_dir(src).map_err(|source_err| StackError::WorkspaceMaterializeFailed {
            reason: format!("read_dir `{}`: {source_err}", src.display()),
        })?
    {
        let entry = entry.map_err(|source_err| StackError::WorkspaceMaterializeFailed {
            reason: format!("read_dir entry `{}`: {source_err}", src.display()),
        })?;
        let file_type =
            entry
                .file_type()
                .map_err(|source_err| StackError::WorkspaceMaterializeFailed {
                    reason: format!("file_type `{}`: {source_err}", entry.path().display()),
                })?;
        if file_type.is_symlink() {
            return Err(StackError::WorkspaceMaterializeFailed {
                reason: format!(
                    "local source contains symlink `{}`; refusing to follow",
                    entry.path().display()
                ),
            });
        }
        let target = dest.join(entry.file_name());
        if file_type.is_dir() {
            std::fs::create_dir_all(&target).map_err(|source_err| {
                StackError::WorkspaceMaterializeFailed {
                    reason: format!("create dir `{}`: {source_err}", target.display()),
                }
            })?;
            copy_dir_recursive(&entry.path(), &target, outcome)?;
        } else if file_type.is_file() {
            let bytes = std::fs::copy(entry.path(), &target).map_err(|source_err| {
                StackError::WorkspaceMaterializeFailed {
                    reason: format!(
                        "copy `{}` -> `{}`: {source_err}",
                        entry.path().display(),
                        target.display()
                    ),
                }
            })?;
            outcome.bytes = outcome.bytes.saturating_add(bytes);
            outcome.entries = outcome.entries.saturating_add(1);
        } else {
            return Err(StackError::WorkspaceMaterializeFailed {
                reason: format!(
                    "local source entry `{}` has unsupported file type",
                    entry.path().display()
                ),
            });
        }
    }
    Ok(())
}
