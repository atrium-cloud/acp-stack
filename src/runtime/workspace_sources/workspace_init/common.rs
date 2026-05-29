//! Cross-lane primitives shared by every workspace materializer:
//!
//! * Capture-file plumbing (`write_command_capture`, `write_operation_capture`,
//!   `capture_error`, `sync_capture_dir`, the `CaptureFiles` pair reservation,
//!   and the owner-only create helper).
//! * Destination/lane safety guards (`ensure_lane_root`,
//!   `ensure_destination_not_symlink`, `ensure_dest_or_fail`,
//!   `destination_is_empty_except_sentinel`, `cleanup_partial_destination`).
//! * Sentinel encoding/decoding (`Sentinel`, `SentinelBody`).
//! * Path-segment sanitization for log directories.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Result, StackError};
use crate::fs_util::atomic_write_owner_only;

use super::SOURCE_SENTINEL_FILE;

pub(super) fn sanitize_segment(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

pub(super) fn ensure_workspace_log_dir(path: &Path) -> Result<()> {
    // Workspace captures contain full git stdout/stderr, which can
    // include private repo URLs and (rarely) credentials in error
    // messages. Match the project-wide owner-only directory convention
    // so a permissive umask doesn't leak any of that to other local
    // users. `create_dir_owner_only` is idempotent.
    crate::fs_util::create_dir_owner_only(path)
}

pub(super) fn ensure_workspace_base_dir(path: &Path, label: &str) -> Result<()> {
    std::fs::create_dir_all(path).map_err(|source| StackError::WorkspaceMaterializeFailed {
        reason: format!("create {label} `{}`: {source}", path.display()),
    })
}

/// Persist a subprocess capture to `<log_dir>/<command>.{stdout,stderr}`.
/// Errors propagate — losing the audit copy on success would mean the
/// promise of "structured step status + per-step log files" is violated
/// for the operator. Empty streams are still written so the operator can
/// distinguish "ran with no output" from "logs never persisted".
pub(super) fn write_command_capture(
    log_dir: Option<&Path>,
    command_tag: &str,
    stdout: &[u8],
    stderr: &[u8],
) -> Result<Option<PathBuf>> {
    let Some(dir) = log_dir else {
        return Ok(None);
    };
    // Each call lands a fresh pair of files so a resume that re-runs
    // the same step preserves the prior failure's capture chain. The
    // base suffix is a wall-clock nanosecond stamp (natural sort
    // order). On collision — two captures landing in the same
    // nanosecond on a coarse clock, or under a concurrent resume — we
    // extend with a 2-digit sequence number and try `create_new` again.
    // `create_new` is the O_CREAT|O_EXCL atomic-create syscall, so the
    // loop terminates with file ownership guaranteed by the OS rather
    // than by an existence probe.
    let base_stamp = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0).max(0);
    let CaptureFiles {
        stdout_path,
        stdout_file: mut stdout_file_owned,
        stderr_path,
        stderr_file: mut stderr_file_owned,
    } = create_capture_file_pair(dir, command_tag, base_stamp)?;
    let stdout_file = &mut stdout_file_owned;
    let stderr_file = &mut stderr_file_owned;
    std::io::Write::write_all(stdout_file, stdout).map_err(|source| {
        StackError::WorkspaceMaterializeFailed {
            reason: format!("write `{}`: {source}", stdout_path.display()),
        }
    })?;
    std::io::Write::write_all(stderr_file, stderr).map_err(|source| {
        StackError::WorkspaceMaterializeFailed {
            reason: format!("write `{}`: {source}", stderr_path.display()),
        }
    })?;
    // fsync both the files AND the parent directory before returning,
    // so a crash between this point and SQLite's `init_steps.log_dir`
    // write cannot leave the row pointing at a missing or zero-length
    // file. Matches the installer log persistence contract.
    stdout_file
        .sync_all()
        .map_err(|source| StackError::WorkspaceMaterializeFailed {
            reason: format!("fsync `{}`: {source}", stdout_path.display()),
        })?;
    stderr_file
        .sync_all()
        .map_err(|source| StackError::WorkspaceMaterializeFailed {
            reason: format!("fsync `{}`: {source}", stderr_path.display()),
        })?;
    sync_capture_dir(dir)?;
    Ok(Some(dir.to_path_buf()))
}

pub(super) fn write_operation_capture(
    log_dir: Option<&Path>,
    operation_tag: &str,
    stdout: &str,
    stderr: &str,
) -> Result<Option<PathBuf>> {
    write_command_capture(log_dir, operation_tag, stdout.as_bytes(), stderr.as_bytes())
}

pub(super) fn capture_error(log_dir: Option<&Path>, operation_tag: &str, error: &StackError) {
    if let Err(capture_error) =
        write_operation_capture(log_dir, operation_tag, "", &format!("{error}\n"))
    {
        tracing::warn!(
            error = %capture_error,
            original_error = %error,
            operation = operation_tag,
            "failed to persist workspace materialization error capture"
        );
    }
}

pub(super) fn sync_capture_dir(dir: &Path) -> Result<()> {
    let directory =
        std::fs::File::open(dir).map_err(|source| StackError::WorkspaceMaterializeFailed {
            reason: format!("open `{}` for fsync: {source}", dir.display()),
        })?;
    directory
        .sync_all()
        .map_err(|source| StackError::WorkspaceMaterializeFailed {
            reason: format!("fsync directory `{}`: {source}", dir.display()),
        })
}

pub(super) struct CaptureFiles {
    pub(super) stdout_path: PathBuf,
    pub(super) stdout_file: std::fs::File,
    pub(super) stderr_path: PathBuf,
    pub(super) stderr_file: std::fs::File,
}

/// Reserve the stdout AND stderr filenames for one capture as an
/// atomic pair. Picking each independently would let two concurrent
/// resumes interleave: process A claims `stdout.00`, process B then
/// claims `stdout.01` followed by `stderr.00`, and process A finally
/// claims `stderr.01` — mismatched pairs that defeat the audit log.
/// This loop reserves both files at the same suffix or rolls both on
/// any collision.
pub(super) fn create_capture_file_pair(
    dir: &Path,
    command_tag: &str,
    base_stamp: i64,
) -> Result<CaptureFiles> {
    for sequence in 0u32..64 {
        let suffix = if sequence == 0 {
            format!("{base_stamp:020}")
        } else {
            format!("{base_stamp:020}.{sequence:02}")
        };
        let stdout_path = dir.join(format!("{command_tag}.{suffix}.stdout"));
        let stderr_path = dir.join(format!("{command_tag}.{suffix}.stderr"));
        let stdout_file = match create_new_owner_only(&stdout_path) {
            Ok(file) => file,
            Err(CaptureCreateError::Collision) => continue,
            Err(CaptureCreateError::Io(err)) => {
                return Err(StackError::WorkspaceMaterializeFailed {
                    reason: format!("create `{}`: {err}", stdout_path.display()),
                });
            }
        };
        match create_new_owner_only(&stderr_path) {
            Ok(stderr_file) => {
                return Ok(CaptureFiles {
                    stdout_path,
                    stdout_file,
                    stderr_path,
                    stderr_file,
                });
            }
            Err(CaptureCreateError::Collision) => {
                // Roll the pair: drop the stdout we just claimed so a
                // concurrent resume sees a clean slot at the next suffix.
                drop(stdout_file);
                let _ = std::fs::remove_file(&stdout_path);
                continue;
            }
            Err(CaptureCreateError::Io(err)) => {
                drop(stdout_file);
                let _ = std::fs::remove_file(&stdout_path);
                return Err(StackError::WorkspaceMaterializeFailed {
                    reason: format!("create `{}`: {err}", stderr_path.display()),
                });
            }
        }
    }
    Err(StackError::WorkspaceMaterializeFailed {
        reason: format!(
            "exhausted 64 capture-filename retries for `{command_tag}` under `{}`",
            dir.display(),
        ),
    })
}

pub(super) enum CaptureCreateError {
    Collision,
    Io(std::io::Error),
}

pub(super) fn create_new_owner_only(
    path: &Path,
) -> std::result::Result<std::fs::File, CaptureCreateError> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        // Captures can carry private repo URLs and (rarely) credential
        // material in error messages. Set 0o600 atomically at create
        // time so a permissive umask never opens a window for other
        // local users to read the bytes between create and chmod.
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(file) => Ok(file),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(CaptureCreateError::Collision)
        }
        Err(err) => Err(CaptureCreateError::Io(err)),
    }
}

pub(super) fn ensure_lane_root(path: &Path) -> Result<()> {
    // Lane roots themselves must be real directories, not symlinks an
    // attacker could swap in to redirect every materialization. We probe
    // with `symlink_metadata` first so we don't auto-follow a swap-in. The
    // `create_dir_all` call only fires when the path doesn't exist yet.
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(StackError::WorkspaceDestinationOutsideRoot {
                dest: path.display().to_string(),
                root: path
                    .parent()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
            });
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Err(StackError::WorkspaceMaterializeFailed {
                reason: format!(
                    "lane root `{}` exists and is not a directory",
                    path.display()
                ),
            });
        }
        Ok(_) => return Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(StackError::WorkspaceMaterializeFailed {
                reason: format!("stat `{}`: {source}", path.display()),
            });
        }
    }
    std::fs::create_dir_all(path).map_err(|source| StackError::WorkspaceMaterializeFailed {
        reason: format!("could not create `{}`: {source}", path.display()),
    })
}

pub(super) fn ensure_destination_not_symlink(dest: &Path) -> Result<()> {
    // Per-source guard: even with a sane lane root, the destination
    // directory could itself be a symlink swap. Reject symlinks at this
    // layer before any `create_dir_all` / `git clone` / `std::fs::copy`
    // call could follow them outside the workspace.
    match std::fs::symlink_metadata(dest) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(StackError::WorkspaceDestinationOutsideRoot {
                dest: dest.display().to_string(),
                root: dest
                    .parent()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
            })
        }
        Ok(_) | Err(_) => Ok(()),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type")]
pub(super) enum SentinelBody {
    #[serde(rename = "git")]
    Git {
        repo: String,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        branch: Option<String>,
        commit: String,
    },
    #[serde(rename = "local")]
    Local {
        path: String,
        bytes: u64,
        entries: u64,
    },
    #[serde(rename = "https")]
    Https {
        url: String,
        sha256: String,
        bytes: u64,
        extracted: bool,
    },
    #[serde(rename = "s3")]
    S3 {
        bucket: String,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        prefix: Option<String>,
        region: String,
        bytes: u64,
        objects: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct Sentinel {
    schema: u32,
    #[serde(flatten)]
    pub(super) body: SentinelBody,
}

impl Sentinel {
    pub(super) fn new(body: SentinelBody) -> Self {
        Self { schema: 1, body }
    }

    pub(super) fn read(dest: &Path) -> Result<Option<Self>> {
        let path = dest.join(SOURCE_SENTINEL_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => Ok(Some(serde_json::from_str(&text).map_err(|source| {
                StackError::WorkspaceMaterializeFailed {
                    reason: format!("sentinel `{}` is corrupted: {source}", path.display()),
                }
            })?)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(StackError::WorkspaceMaterializeFailed {
                reason: format!("read sentinel `{}`: {source}", path.display()),
            }),
        }
    }

    pub(super) fn write(&self, dest: &Path) -> Result<()> {
        let payload = serde_json::to_string_pretty(self).map_err(|source| {
            StackError::WorkspaceMaterializeFailed {
                reason: format!("serialize sentinel: {source}"),
            }
        })?;
        atomic_write_owner_only(&dest.join(SOURCE_SENTINEL_FILE), payload.as_bytes())
    }
}

pub(super) fn sentinel_if_present(dest: &Path) -> Result<Option<Sentinel>> {
    if !dest.exists() {
        return Ok(None);
    }
    Sentinel::read(dest)
}

pub(super) fn destination_is_empty_except_sentinel(dest: &Path) -> Result<bool> {
    let read_dir = match std::fs::read_dir(dest) {
        Ok(rd) => rd,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(source) => {
            return Err(StackError::WorkspaceMaterializeFailed {
                reason: format!("read_dir `{}`: {source}", dest.display()),
            });
        }
    };
    for entry in read_dir {
        let entry = entry.map_err(|source| StackError::WorkspaceMaterializeFailed {
            reason: format!("read_dir entry `{}`: {source}", dest.display()),
        })?;
        if entry.file_name() != SOURCE_SENTINEL_FILE {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(super) fn ensure_dest_or_fail(dest: &Path) -> Result<()> {
    if !dest.exists() {
        return Ok(());
    }
    if destination_is_empty_except_sentinel(dest)? {
        return Ok(());
    }
    Err(StackError::WorkspaceDestinationNotEmpty {
        dest: dest.display().to_string(),
    })
}

pub(super) fn cleanup_partial_destination(dest: &Path, original: StackError) -> StackError {
    match std::fs::remove_dir_all(dest) {
        Ok(()) => original,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => original,
        Err(source) => StackError::WorkspaceMaterializeFailed {
            reason: format!(
                "{original}; additionally failed to clean partial destination `{}`: {source}",
                dest.display()
            ),
        },
    }
}
