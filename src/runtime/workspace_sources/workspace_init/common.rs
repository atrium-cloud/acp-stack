//! Cross-lane primitives shared by every workspace materializer: capture-file plumbing, the
//! runtime-owned staging directory, the workload-side destination under `workspace.root`, and
//! sentinel encoding.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{Result, StackError};
use crate::runtime::process_runner::HostExec;
use crate::runtime::sandbox::SandboxProfile;
use crate::workload_fs::{
    self, Anchor, DEFAULT_JOB_TIMEOUT, EntryKind, Executor, HandoffOptions, HandoffSummary,
    LinkPolicy, SymlinkPolicy, WriteOptions,
};

use super::SOURCE_SENTINEL_FILE;

// === CONSTANTS ===

/// Subdirectory of the runtime state dir that sources materialize into before the handoff.
const STAGING_DIR_NAME: &str = "staging";
const STAGING_PREFIX: &str = "source-";
const STAGING_DIR_MODE: u32 = 0o700;
/// Bound for one handoff into the workspace; a large checkout or data set copies far longer than
/// [`DEFAULT_JOB_TIMEOUT`].
const HANDOFF_TIMEOUT: Duration = Duration::from_secs(60 * 60);
/// Sentinels are small JSON documents; anything larger is not one of ours.
const SENTINEL_MAX_BYTES: u64 = 64 * 1024;
/// Materialized files keep their source permissions and directories follow the umask, with or
/// without a workload identity; only the sentinel keeps the owner-only mode the workspace writes use.
const MATERIALIZED_FILE_MODE_MASK: u32 = 0o777;
const MATERIALIZED_DIR_MODE: u32 = workload_fs::UMASK_DIR_MODE;
/// Destination walks run under the executor's credentials, so they follow links that stay inside
/// the workspace root.
const ROOT_LINKS: LinkPolicy = LinkPolicy::Follow { contained: true };

/// What every lane materializer shares: the runtime side that fetches into staging and the
/// workload side that receives the result.
pub(super) struct MaterializeContext {
    pub(super) host: HostExec,
    pub(super) destination: WorkspaceDestination,
}

/// A fresh owner-only directory under the runtime state dir that one source materializes into, as
/// the runtime, before [`WorkspaceDestination::install`] moves it into the workspace. Removed on
/// drop, so every exit path cleans it up.
pub(super) struct StagingDir {
    path: PathBuf,
}

impl StagingDir {
    pub(super) fn create(home: &Path) -> Result<Self> {
        let parent = crate::secrets::state_dir(home).join(STAGING_DIR_NAME);
        crate::fs_util::create_dir_owner_only(&parent)?;
        let directory = tempfile::Builder::new()
            .prefix(STAGING_PREFIX)
            .permissions(std::fs::Permissions::from_mode(STAGING_DIR_MODE))
            .tempdir_in(&parent)
            .map_err(|source| StackError::WorkspaceMaterializeFailed {
                reason: format!("create staging dir under `{}`: {source}", parent.display()),
            })?;
        Ok(Self {
            path: directory.keep(),
        })
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for StagingDir {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.path) {
            tracing::warn!(
                %error,
                path = %self.path.display(),
                "failed to remove a workspace staging directory"
            );
        }
    }
}

/// The workload side of materialization. Every operation walks from an anchor on
/// `workspace.root`, following links that stay inside it, under the workload identity's
/// credentials when one is declared; paths are relative to the root.
pub(super) struct WorkspaceDestination {
    root: PathBuf,
    executor: Executor,
    write_options: WriteOptions,
    hard_links: bool,
}

impl WorkspaceDestination {
    pub(super) fn new(root: &Path, profile: &SandboxProfile) -> Self {
        Self {
            root: root.to_path_buf(),
            executor: profile.executor(),
            write_options: crate::workspace::workload_write_options(profile),
            hard_links: profile.accepts_hard_links(),
        }
    }

    /// Whether sources may contain hard-linked files; see [`SandboxProfile::accepts_hard_links`].
    pub(super) fn accepts_hard_links(&self) -> bool {
        self.hard_links
    }

    /// Absolute path of `relative`, for reports and messages.
    pub(super) fn display(&self, relative: &Path) -> PathBuf {
        self.root.join(relative)
    }

    fn run<T: Send + 'static>(
        &self,
        job: impl FnOnce(&Anchor) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let root = self.root.clone();
        self.executor.run(DEFAULT_JOB_TIMEOUT, move || {
            let anchor = Anchor::open_with(&root, ROOT_LINKS)?;
            job(&anchor)
        })
    }

    /// Create the lane root (`usr/code` or `usr/data`) and any missing parent below the root.
    pub(super) fn ensure_lane_root(&self, lane: &Path) -> Result<()> {
        let relative = lane.to_path_buf();
        let display = self.display(lane);
        let dir_mode = MATERIALIZED_DIR_MODE;
        self.run(
            move |anchor| match workload_fs::stat_followed(anchor, &relative) {
                Ok(None) => workload_fs::create_dir_all(anchor, &relative, dir_mode),
                Ok(Some(info)) => match info.kind {
                    EntryKind::Dir => Ok(()),
                    EntryKind::File | EntryKind::Symlink | EntryKind::Other => {
                        Err(StackError::WorkspaceMaterializeFailed {
                            reason: format!(
                                "lane root `{}` exists and is not a directory",
                                display.display()
                            ),
                        })
                    }
                },
                Err(StackError::WorkloadFsSymlinkRefused { .. }) => {
                    Err(outside_root_error(&display))
                }
                Err(error) => Err(error),
            },
        )
    }

    /// The sentinel in the destination at `relative`; `None` when the destination or its
    /// sentinel is missing. A destination linked outside the root is refused.
    pub(super) fn read_sentinel(&self, relative: &Path) -> Result<Option<Sentinel>> {
        let destination = relative.to_path_buf();
        let display = self.display(relative);
        let sentinel_path = display.join(SOURCE_SENTINEL_FILE);
        let content = self.run(move |anchor| {
            if destination_kind(anchor, &destination, &display)?.is_none() {
                return Ok(None);
            }
            let sentinel = destination.join(SOURCE_SENTINEL_FILE);
            refuse_linked_sentinel(anchor, &sentinel, &display)?;
            match workload_fs::read_file(anchor, &sentinel, SENTINEL_MAX_BYTES) {
                Ok(content) => Ok(Some(content)),
                Err(StackError::WorkloadFsNotFound { .. }) => Ok(None),
                Err(error) => Err(error),
            }
        })?;
        content
            .map(|bytes| {
                serde_json::from_slice(&bytes).map_err(|source| {
                    StackError::WorkspaceMaterializeFailed {
                        reason: format!(
                            "sentinel `{}` is corrupted: {source}",
                            sentinel_path.display()
                        ),
                    }
                })
            })
            .transpose()
    }

    /// Make the destination at `relative` ready for a handoff: absent, or an empty directory. A
    /// stale sentinel left alone in it is removed; any other content refuses.
    pub(super) fn prepare(&self, relative: &Path) -> Result<()> {
        let destination = relative.to_path_buf();
        let display = self.display(relative);
        self.run(move |anchor| {
            match destination_kind(anchor, &destination, &display)? {
                None => return Ok(()),
                Some(EntryKind::Dir) => {}
                Some(_) => return Err(not_empty_error(&display)),
            }
            let mut stale_sentinel = false;
            for (name, info) in workload_fs::list_dir(anchor, &destination)? {
                if name == SOURCE_SENTINEL_FILE && info.kind == EntryKind::File {
                    stale_sentinel = true;
                } else {
                    return Err(not_empty_error(&display));
                }
            }
            if stale_sentinel {
                workload_fs::remove_file(anchor, &destination.join(SOURCE_SENTINEL_FILE))?;
            }
            Ok(())
        })
    }

    /// Remove the destination at `relative` and everything below it. A destination that is a
    /// symlink keeps the link and is emptied through it; links below it are unlinked as links.
    pub(super) fn remove(&self, relative: &Path) -> Result<()> {
        let destination = relative.to_path_buf();
        self.run(move |anchor| {
            let removed = match workload_fs::stat(anchor, &destination)? {
                None => return Ok(()),
                Some(info) if info.kind == EntryKind::Symlink => {
                    clear_linked_directory(anchor, &destination)
                }
                Some(_) => workload_fs::remove_tree(anchor, &destination),
            };
            match removed {
                Err(StackError::WorkloadFsNotFound { .. }) => Ok(()),
                removed => removed,
            }
        })
    }

    /// Hand the tree at `source` off into the destination at `relative`, then stamp the sentinel
    /// built from what was copied. A failed stamp removes the handed-off tree so the next run does
    /// not refuse it as non-empty.
    pub(super) fn install(
        &self,
        source: &Path,
        relative: &Path,
        symlinks: SymlinkPolicy,
        sentinel: impl FnOnce(&HandoffSummary) -> Sentinel,
    ) -> Result<HandoffSummary> {
        let summary = workload_fs::handoff_tree(
            &self.executor,
            source,
            &self.root,
            relative,
            &HandoffOptions {
                symlinks,
                hard_links: self.hard_links,
                destination_links: ROOT_LINKS,
                file_mode: MATERIALIZED_FILE_MODE_MASK,
                dir_mode: MATERIALIZED_DIR_MODE,
                timeout: HANDOFF_TIMEOUT,
            },
        )?;
        match self.write_sentinel(relative, &sentinel(&summary)) {
            Ok(()) => Ok(summary),
            Err(error) => Err(self.discard_after_failure(relative, error)),
        }
    }

    /// Whether every path in `sentinels` is a regular file; a sentinel path that is itself a
    /// symlink does not count.
    pub(super) fn sentinels_present(&self, sentinels: Vec<PathBuf>) -> Result<bool> {
        self.run(move |anchor| {
            for sentinel in &sentinels {
                match workload_fs::stat(anchor, sentinel)? {
                    Some(info) if info.kind == EntryKind::File => {}
                    _ => return Ok(false),
                }
            }
            Ok(true)
        })
    }

    fn write_sentinel(&self, relative: &Path, sentinel: &Sentinel) -> Result<()> {
        let payload = serde_json::to_vec_pretty(sentinel).map_err(|source| {
            StackError::WorkspaceMaterializeFailed {
                reason: format!("serialize sentinel: {source}"),
            }
        })?;
        let target = relative.join(SOURCE_SENTINEL_FILE);
        let display = self.display(relative);
        let options = self.write_options;
        self.run(move |anchor| {
            refuse_linked_sentinel(anchor, &target, &display)?;
            workload_fs::write_file_atomic(anchor, &target, &payload, &options)
        })
    }

    pub(super) fn discard_after_failure(
        &self,
        relative: &Path,
        original: StackError,
    ) -> StackError {
        match self.remove(relative) {
            Ok(()) => original,
            Err(error) => StackError::WorkspaceMaterializeFailed {
                reason: format!(
                    "{original}; additionally failed to clean partial destination `{}`: {error}",
                    self.display(relative).display()
                ),
            },
        }
    }
}

/// Kind of the entry the destination at `relative` resolves to, `None` when it is missing. A link
/// at the destination or along its parents that resolves outside the root is refused as leaving
/// the workspace.
fn destination_kind(anchor: &Anchor, relative: &Path, display: &Path) -> Result<Option<EntryKind>> {
    match workload_fs::stat_followed(anchor, relative) {
        Ok(None) => Ok(None),
        Ok(Some(info)) => Ok(Some(info.kind)),
        Err(StackError::WorkloadFsSymlinkRefused { .. }) => Err(outside_root_error(display)),
        Err(error) => Err(error),
    }
}

fn clear_linked_directory(anchor: &Anchor, destination: &Path) -> Result<()> {
    for (name, _) in workload_fs::list_dir(anchor, destination)? {
        workload_fs::remove_tree(anchor, &destination.join(name))?;
    }
    Ok(())
}

/// A source tree can carry its own symlink named like the sentinel, which would otherwise redirect
/// the sentinel's write and read onto another file in the workspace.
fn refuse_linked_sentinel(anchor: &Anchor, sentinel: &Path, destination: &Path) -> Result<()> {
    match workload_fs::stat(anchor, sentinel)? {
        Some(info) if info.kind == EntryKind::Symlink => {
            Err(StackError::WorkspaceMaterializeFailed {
                reason: format!(
                    "sentinel `{}` is a symlink",
                    destination.join(SOURCE_SENTINEL_FILE).display()
                ),
            })
        }
        _ => Ok(()),
    }
}

fn outside_root_error(dest: &Path) -> StackError {
    StackError::WorkspaceDestinationOutsideRoot {
        dest: dest.display().to_string(),
        root: dest
            .parent()
            .map(|parent| parent.display().to_string())
            .unwrap_or_default(),
    }
}

fn not_empty_error(dest: &Path) -> StackError {
    StackError::WorkspaceDestinationNotEmpty {
        dest: dest.display().to_string(),
    }
}

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
    // Captures carry raw git stdout/stderr, which can include private repo URLs and credentials,
    // so the directory must be owner-only regardless of umask.
    crate::fs_util::create_dir_owner_only(path)
}

pub(super) fn ensure_workspace_base_dir(path: &Path, label: &str) -> Result<()> {
    std::fs::create_dir_all(path).map_err(|source| StackError::WorkspaceMaterializeFailed {
        reason: format!("create {label} `{}`: {source}", path.display()),
    })
}

/// Persist a subprocess capture to `<log_dir>/<command>.{stdout,stderr}`. Empty streams are still
/// written so "ran with no output" stays distinguishable from "logs never persisted".
pub(super) fn write_command_capture(
    log_dir: Option<&Path>,
    command_tag: &str,
    stdout: &[u8],
    stderr: &[u8],
) -> Result<Option<PathBuf>> {
    let Some(dir) = log_dir else {
        return Ok(None);
    };
    // Each call lands a fresh pair so a resume preserves the prior failure's capture chain; the
    // nanosecond stamp sorts naturally and `create_new` (O_CREAT|O_EXCL) settles collisions.
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
    // fsync the files AND the parent directory, so a crash before SQLite's `init_steps.log_dir`
    // write cannot leave the row pointing at a missing or zero-length file.
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

/// Reserve stdout AND stderr for one capture at the same suffix, rolling both on collision;
/// picking each independently lets concurrent resumes interleave into mismatched pairs.
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
                // Release the stdout slot so a concurrent resume sees a clean pair at this suffix.
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
        // 0o600 is set atomically at create time; a create-then-chmod would leave a window where
        // other local users could read captured repo URLs or credential material.
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
}
