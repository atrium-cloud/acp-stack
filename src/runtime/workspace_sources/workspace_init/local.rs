//! Data lane: local filesystem source. Mirrors a configured path into the
//! workspace data lane, rejecting symlinks at every level so a configured
//! `/data/link -> /etc` cannot smuggle untrusted bytes inside the workspace.

use std::path::Path;

use crate::config::DataSourceConfig;
use crate::error::{Result, StackError};
use crate::workload_fs::{HandoffSummary, SymlinkPolicy};

use super::common::{
    MaterializeContext, Sentinel, SentinelBody, StagingDir, capture_error, write_operation_capture,
};
use super::{CAPTURE_TAG_COPY, MaterializeOutcome, SourceReport};

// CONSTANTS

const PERMISSION_BITS: u32 = 0o777;

pub(super) fn materialize_local(
    index: usize,
    source: &DataSourceConfig,
    name: &str,
    relative: &Path,
    context: &MaterializeContext,
    log_dir: Option<&Path>,
) -> Result<SourceReport> {
    let dest = context.destination.display(relative);
    let path = source
        .path
        .as_deref()
        .ok_or_else(|| StackError::WorkspaceDataSourceInvalid {
            index,
            reason: "path is required".to_owned(),
        })?;
    let src = Path::new(path);
    // MUST reject a top-level symlink BEFORE canonicalize follows it:
    // `copy_tree`'s walk runs after canonicalization, so a configured
    // `/data/link -> /etc` would otherwise be copied as the declared source.
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

    // A destination inside the source tree would loop or snapshot itself. `dest`
    // may not exist yet, so the check runs against its parent.
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

    if let Some(existing) = context.destination.read_sentinel(relative)?
        && let SentinelBody::Local {
            path: existing_path,
            ..
        } = &existing.body
        && existing_path == &canonical_src.display().to_string()
    {
        return Ok(SourceReport {
            name: name.to_owned(),
            destination: dest,
            outcome: MaterializeOutcome::Verified,
            log_dir: None,
        });
    }
    context.destination.prepare(relative)?;

    let sentinel = |copied: &HandoffSummary| {
        Sentinel::new(SentinelBody::Local {
            path: canonical_src.display().to_string(),
            bytes: copied.bytes,
            entries: copied.files,
        })
    };
    let copy_failed = |error: StackError| StackError::WorkspaceMaterializeFailed {
        reason: format!("copy local source `{}`: {error}", canonical_src.display()),
    };
    let installed = if src_metadata.is_dir() {
        context
            .destination
            .install(&canonical_src, relative, SymlinkPolicy::Reject, sentinel)
            .map_err(copy_failed)
    } else {
        // The handoff walks a directory, so a single file is first copied into its own one.
        StagingDir::create(context.host.home()).and_then(|staging| {
            copy_single_file(
                &canonical_src,
                staging.path(),
                context.destination.accepts_hard_links(),
            )?;
            context
                .destination
                .install(staging.path(), relative, SymlinkPolicy::Reject, sentinel)
                .map_err(copy_failed)
        })
    };
    let copied = match installed {
        Ok(copied) => copied,
        Err(err) => {
            capture_error(log_dir, CAPTURE_TAG_COPY, &err);
            return Err(err);
        }
    };
    if let Err(error) = write_operation_capture(
        log_dir,
        CAPTURE_TAG_COPY,
        &format!(
            "source={}\ndestination={}\nbytes={}\nentries={}\n",
            canonical_src.display(),
            dest.display(),
            copied.bytes,
            copied.files,
        ),
        "",
    ) {
        return Err(context.destination.discard_after_failure(relative, error));
    }

    Ok(SourceReport {
        name: name.to_owned(),
        destination: dest,
        outcome: MaterializeOutcome::Created,
        log_dir: log_dir.map(Path::to_path_buf),
    })
}

/// Copy a single-file source into `staging_dir` under its own name. A local source may be writable
/// by the workload, so the file is opened `O_NOFOLLOW` and copied from that descriptor.
fn copy_single_file(src: &Path, staging_dir: &Path, allow_hard_links: bool) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let failed =
        |operation: &str, source_err: std::io::Error| StackError::WorkspaceMaterializeFailed {
            reason: format!("{operation} `{}`: {source_err}", src.display()),
        };
    let file_name = src
        .file_name()
        .ok_or_else(|| StackError::WorkspaceMaterializeFailed {
            reason: format!("local source `{}` has no file name", src.display()),
        })?;
    let mut source = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(src)
        .map_err(|source_err| failed("open (symlinks are refused)", source_err))?;
    let metadata = source
        .metadata()
        .map_err(|source_err| failed("stat", source_err))?;
    if !metadata.is_file() {
        return Err(StackError::WorkspaceMaterializeFailed {
            reason: format!("local source `{}` must be a regular file", src.display()),
        });
    }
    if !allow_hard_links && metadata.nlink() > 1 {
        return Err(StackError::WorkspaceMaterializeFailed {
            reason: format!(
                "local source `{}` is hard-linked; a workload identity requires a single link",
                src.display()
            ),
        });
    }
    let mut destination = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(metadata.mode() & PERMISSION_BITS)
        .open(staging_dir.join(file_name))
        .map_err(|source_err| failed("create staging copy of", source_err))?;
    std::io::copy(&mut source, &mut destination)
        .map_err(|source_err| failed("copy", source_err))?;
    Ok(())
}
