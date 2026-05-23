//! Seeds `<workspace.root>/usr/code/<repo>/` and `<workspace.root>/usr/data/<name>/`
//! from `[[workspace.code_sources]]` and `[[workspace.data_sources]]` during
//! `acps init`.
//!
//! Phase 4 introduces two parallel ingestion lanes:
//!
//! * `code` lane: Git repositories cloned via the host `git` binary.
//! * `data` lane: local paths, HTTPS archives (Drive/Dropbox/arbitrary
//!   hosts), and S3 buckets/prefixes.
//!
//! Each destination is anchored under `usr/code/` or `usr/data/`, owned by
//! the runtime user, and never collides with `<root>/uploads/`. Init is
//! intentionally not transactional across sources: each completed source
//! drops a `.acp-stack-source.json` sentinel; a subsequent rerun verifies
//! the sentinel and skips already-completed lanes. A non-empty destination
//! without a matching sentinel is a hard failure rather than a
//! best-effort merge — the operator is responsible for cleaning up.
//!
//! Archive extraction, HTTPS download, and Git invocation are all
//! delegated to safe modules under `runtime::safe_*` so they can be tested
//! in isolation.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::config::{
    CodeSourceConfig, DataSourceConfig, WorkspaceConfig, derive_code_source_name,
    derive_data_source_name,
};
use crate::error::{Result, StackError};
use crate::fs_util::atomic_write_owner_only;
use crate::runtime::safe_download::{DownloadOpts, download_to_file};
use crate::runtime::safe_extract::{ExtractOpts, extract_archive};
use crate::secrets::SecretStore;

/// Sentinel filename written into each materialized destination so reruns
/// of `acps init` can detect "already done" lanes and skip cleanly.
pub const SOURCE_SENTINEL_FILE: &str = ".acp-stack-source.json";

/// Subdirectory under `workspace.root` for code lanes.
pub const CODE_LANE_DIR: &str = "usr/code";
/// Subdirectory under `workspace.root` for data lanes.
pub const DATA_LANE_DIR: &str = "usr/data";

const CAPTURE_TAG_GIT_CLONE: &str = "git-clone";
const CAPTURE_TAG_GIT_REV_PARSE: &str = "git-rev-parse";
const CAPTURE_TAG_DOWNLOAD: &str = "download";
const CAPTURE_TAG_EXTRACT: &str = "extract";
const CAPTURE_TAG_COPY: &str = "copy";
const CAPTURE_TAG_S3_DOWNLOAD: &str = "s3-download";

/// Canonical on-disk root for workspace materialization logs. Mirrors
/// the layout used by installer step logs (`default_installer_log_base`)
/// so backups and log rotation can target one directory.
pub fn default_workspace_init_log_base(home: &Path) -> PathBuf {
    home.join(".local")
        .join("share")
        .join("acp-stack")
        .join("workspace-init-logs")
}

/// Per-run capture location for workspace materialization. The init
/// orchestrator constructs one of these per `init_runs.id` and passes
/// it into [`materialize_workspace`]; each source gets a subdirectory
/// underneath it, and each subprocess invocation writes its full
/// stdout/stderr there.
#[derive(Debug, Clone)]
pub struct WorkspaceLogPaths {
    /// `<log_base>/<init_run_id>/`. Becomes the `log_dir` recorded on the
    /// init step row so the operator can drill into any source.
    pub run_dir: PathBuf,
}

impl WorkspaceLogPaths {
    pub fn for_run(log_base: &Path, init_run_id: &str) -> Self {
        Self {
            run_dir: log_base.join(sanitize_segment(init_run_id)),
        }
    }
}

fn sanitize_segment(value: &str) -> String {
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

/// Outcome of a single source materialization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MaterializeOutcome {
    /// Newly materialized — directory was created and source content
    /// fetched/copied from scratch.
    Created,
    /// Sentinel matched existing config — skipped without touching the
    /// filesystem.
    Verified,
}

#[derive(Debug, Clone)]
pub struct SourceReport {
    pub name: String,
    pub destination: PathBuf,
    pub outcome: MaterializeOutcome,
    /// On-disk directory under `WorkspaceLogPaths.run_dir` holding this
    /// source's capture files. Git sources persist subprocess stdout/stderr;
    /// Rust-native data sources persist synthetic stdout/stderr audit entries.
    /// `None` when materialization was a verifier-only skip OR when the caller
    /// did not provide a `WorkspaceLogPaths`.
    pub log_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Default)]
pub struct MaterializeReport {
    pub code: Vec<SourceReport>,
    pub data: Vec<SourceReport>,
    /// Root capture directory shared by every source in this run.
    /// `Some(...)` whenever the caller passed a `WorkspaceLogPaths`,
    /// even if no source actually ran (the directory is still
    /// pre-created so audit tooling can land logs under a stable path).
    pub log_dir: Option<PathBuf>,
}

/// True when every declared code/data source's destination directory has
/// the sentinel file written by a prior successful materialization. Used
/// by the init orchestrator's resume verifier to skip the
/// `workspace_materialize` step when nothing needs re-fetching. Failures
/// to compute names or stat the lane root return `Err`, which the caller
/// treats as a verifier miss (forces re-execution).
pub fn all_sources_have_sentinel(workspace: &WorkspaceConfig) -> Result<bool> {
    if workspace.code_sources.is_empty() && workspace.data_sources.is_empty() {
        return Ok(true);
    }
    let root = Path::new(&workspace.root);
    if !root.is_absolute() {
        return Ok(false);
    }
    let code_root = root.join(CODE_LANE_DIR);
    let data_root = root.join(DATA_LANE_DIR);
    for source in &workspace.code_sources {
        let Ok(name) = derive_code_source_name(source) else {
            return Ok(false);
        };
        if !code_root.join(&name).join(SOURCE_SENTINEL_FILE).is_file() {
            return Ok(false);
        }
    }
    for source in &workspace.data_sources {
        let Ok(name) = derive_data_source_name(source) else {
            return Ok(false);
        };
        if !data_root.join(&name).join(SOURCE_SENTINEL_FILE).is_file() {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Materialize every declared code and data source. No-op when both
/// vectors are empty. When `log_paths` is `Some(...)`, every source
/// operation writes capture pairs under
/// `log_paths.run_dir/<source-tag>/<operation>.{stdout,stderr}`. Git
/// operations persist the child-process streams; Rust-native data
/// operations persist a deterministic summary on stdout and failure
/// detail on stderr. When `None`, the existing tail-on-failure behavior
/// is preserved (used by tests that don't need durable logs).
pub fn materialize_workspace(
    workspace: &WorkspaceConfig,
    secrets: &SecretStore,
    log_paths: Option<&WorkspaceLogPaths>,
) -> Result<MaterializeReport> {
    let root = Path::new(&workspace.root);
    if !root.is_absolute() {
        return Err(StackError::WorkspaceMaterializeFailed {
            reason: format!(
                "workspace.root `{}` must be absolute for materialization",
                workspace.root
            ),
        });
    }

    let code_root = root.join(CODE_LANE_DIR);
    let data_root = root.join(DATA_LANE_DIR);

    let mut report = MaterializeReport {
        log_dir: log_paths.map(|p| p.run_dir.clone()),
        ..MaterializeReport::default()
    };

    if let Some(paths) = log_paths {
        ensure_workspace_log_dir(&paths.run_dir)?;
    }

    for (index, source) in workspace.code_sources.iter().enumerate() {
        ensure_lane_root(&code_root)?;
        let source_log_dir = log_paths
            .map(|p| p.run_dir.join(format!("code-{index:03}")))
            .map(|p| {
                ensure_workspace_log_dir(&p)?;
                Ok::<PathBuf, StackError>(p)
            })
            .transpose()?;
        report.code.push(materialize_code_source(
            index,
            source,
            &code_root,
            secrets,
            source_log_dir.as_deref(),
        )?);
    }
    for (index, source) in workspace.data_sources.iter().enumerate() {
        ensure_lane_root(&data_root)?;
        let source_log_dir = log_paths
            .map(|p| p.run_dir.join(format!("data-{index:03}")))
            .map(|p| {
                ensure_workspace_log_dir(&p)?;
                Ok::<PathBuf, StackError>(p)
            })
            .transpose()?;
        report.data.push(materialize_data_source(
            index,
            source,
            &data_root,
            secrets,
            source_log_dir.as_deref(),
        )?);
    }

    Ok(report)
}

fn ensure_workspace_log_dir(path: &Path) -> Result<()> {
    // Workspace captures contain full git stdout/stderr, which can
    // include private repo URLs and (rarely) credentials in error
    // messages. Match the project-wide owner-only directory convention
    // so a permissive umask doesn't leak any of that to other local
    // users. `create_dir_owner_only` is idempotent.
    crate::fs_util::create_dir_owner_only(path)
}

/// Persist a subprocess capture to `<log_dir>/<command>.{stdout,stderr}`.
/// Errors propagate — losing the audit copy on success would mean the
/// promise of "structured step status + per-step log files" is violated
/// for the operator. Empty streams are still written so the operator can
/// distinguish "ran with no output" from "logs never persisted".
fn write_command_capture(
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

fn write_operation_capture(
    log_dir: Option<&Path>,
    operation_tag: &str,
    stdout: &str,
    stderr: &str,
) -> Result<Option<PathBuf>> {
    write_command_capture(log_dir, operation_tag, stdout.as_bytes(), stderr.as_bytes())
}

fn capture_error(log_dir: Option<&Path>, operation_tag: &str, error: &StackError) {
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

fn sync_capture_dir(dir: &Path) -> Result<()> {
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

struct CaptureFiles {
    stdout_path: PathBuf,
    stdout_file: std::fs::File,
    stderr_path: PathBuf,
    stderr_file: std::fs::File,
}

/// Reserve the stdout AND stderr filenames for one capture as an
/// atomic pair. Picking each independently would let two concurrent
/// resumes interleave: process A claims `stdout.00`, process B then
/// claims `stdout.01` followed by `stderr.00`, and process A finally
/// claims `stderr.01` — mismatched pairs that defeat the audit log.
/// This loop reserves both files at the same suffix or rolls both on
/// any collision.
fn create_capture_file_pair(
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

enum CaptureCreateError {
    Collision,
    Io(std::io::Error),
}

fn create_new_owner_only(path: &Path) -> std::result::Result<std::fs::File, CaptureCreateError> {
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

fn ensure_lane_root(path: &Path) -> Result<()> {
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

fn ensure_destination_not_symlink(dest: &Path) -> Result<()> {
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
enum SentinelBody {
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
struct Sentinel {
    schema: u32,
    #[serde(flatten)]
    body: SentinelBody,
}

impl Sentinel {
    fn new(body: SentinelBody) -> Self {
        Self { schema: 1, body }
    }

    fn read(dest: &Path) -> Result<Option<Self>> {
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

    fn write(&self, dest: &Path) -> Result<()> {
        let payload = serde_json::to_string_pretty(self).map_err(|source| {
            StackError::WorkspaceMaterializeFailed {
                reason: format!("serialize sentinel: {source}"),
            }
        })?;
        atomic_write_owner_only(&dest.join(SOURCE_SENTINEL_FILE), payload.as_bytes())
    }
}

fn destination_is_empty_except_sentinel(dest: &Path) -> Result<bool> {
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

fn ensure_dest_or_fail(dest: &Path) -> Result<()> {
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

fn cleanup_partial_destination(dest: &Path, original: StackError) -> StackError {
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

// ---------- code lane ----------

fn materialize_code_source(
    index: usize,
    source: &CodeSourceConfig,
    code_root: &Path,
    secrets: &SecretStore,
    log_dir: Option<&Path>,
) -> Result<SourceReport> {
    let name = derive_code_source_name(source)
        .map_err(|reason| StackError::WorkspaceCodeSourceInvalid { index, reason })?;
    let dest = code_root.join(&name);
    ensure_destination_not_symlink(&dest)?;
    let repo = source
        .repo
        .as_deref()
        .ok_or_else(|| StackError::WorkspaceCodeSourceInvalid {
            index,
            reason: "repo is required".to_owned(),
        })?;

    if dest.exists() {
        if let Some(existing) = Sentinel::read(&dest)? {
            if let SentinelBody::Git {
                repo: existing_repo,
                branch: existing_branch,
                ..
            } = &existing.body
                && existing_repo == repo
                && existing_branch.as_deref() == source.branch.as_deref()
            {
                return Ok(SourceReport {
                    name,
                    destination: dest,
                    outcome: MaterializeOutcome::Verified,
                    log_dir: None,
                });
            }
            return Err(StackError::WorkspaceDestinationNotEmpty {
                dest: dest.display().to_string(),
            });
        }
        if !destination_is_empty_except_sentinel(&dest)? {
            return Err(StackError::WorkspaceDestinationNotEmpty {
                dest: dest.display().to_string(),
            });
        }
    }

    std::fs::create_dir_all(&dest).map_err(|source_err| {
        StackError::WorkspaceMaterializeFailed {
            reason: format!("create dest `{}`: {source_err}", dest.display()),
        }
    })?;

    let credential = match source.credential_ref.as_deref() {
        Some(name) => Some(secrets.get(name)?.to_owned()),
        None => None,
    };

    let outcome = run_git_clone(
        repo,
        source.branch.as_deref(),
        credential.as_deref(),
        &dest,
        log_dir,
    );
    if let Err(err) = outcome {
        // Clean up the partially-clobbered destination so a rerun can retry.
        return Err(cleanup_partial_destination(&dest, err));
    }

    // Everything from here to the sentinel write is "after a successful
    // clone but before the source is durably marked done". A failure
    // here — git rev-parse, the per-step log writes that now run inside
    // it, or the sentinel write itself — leaves a populated destination
    // without a sentinel, which the next `acps init` would reject under
    // the non-empty destination guard. Funnel every failure through
    // `cleanup_partial_destination` so a retry can proceed.
    let commit = match run_git_rev_parse(&dest, log_dir) {
        Ok(commit) => commit,
        Err(err) => return Err(cleanup_partial_destination(&dest, err)),
    };
    let sentinel = Sentinel::new(SentinelBody::Git {
        repo: repo.to_owned(),
        branch: source.branch.clone(),
        commit,
    });
    if let Err(err) = sentinel.write(&dest) {
        return Err(cleanup_partial_destination(&dest, err));
    }

    Ok(SourceReport {
        name,
        destination: dest,
        outcome: MaterializeOutcome::Created,
        log_dir: log_dir.map(Path::to_path_buf),
    })
}

/// Cap stored stderr from materializer subprocesses (git, curl, tar) so a
/// chatty failure does not poison the error variant. Matches the 2 KiB tail
/// used by `agent_installer::tail_bytes` for installer-step stderr; operators
/// expecting consistent failure ergonomics get the same envelope here.
const WORKSPACE_STDERR_TAIL_BYTES: usize = 2 * 1024;

fn tail_stderr_bytes(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let trimmed = text.trim();
    if trimmed.len() <= WORKSPACE_STDERR_TAIL_BYTES {
        return trimmed.to_owned();
    }
    let start = trimmed.len() - WORKSPACE_STDERR_TAIL_BYTES;
    let mut cutoff = start;
    while cutoff < trimmed.len() && !trimmed.is_char_boundary(cutoff) {
        cutoff += 1;
    }
    trimmed[cutoff..].to_owned()
}

fn run_git_clone(
    repo: &str,
    branch: Option<&str>,
    credential: Option<&str>,
    dest: &Path,
    log_dir: Option<&Path>,
) -> Result<()> {
    let mut cmd = Command::new("git");
    cmd.arg("clone").arg("--depth").arg("1");
    if let Some(branch) = branch {
        cmd.arg("--branch").arg(branch);
    }
    cmd.arg("--").arg(repo).arg(dest);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    // Force non-interactive auth. If we pass a credential, it goes via
    // GIT_ASKPASS so the token never lands in process args (which would
    // leak through ps/audit logs). The askpass helper just echoes the
    // token; this only fires when libcurl asks for a password, so
    // unauthenticated clones still work.
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd.env("GIT_HTTP_LOW_SPEED_LIMIT", "1000");
    cmd.env("GIT_HTTP_LOW_SPEED_TIME", "60");
    if let Some(token) = credential {
        cmd.env("ACP_STACK_GIT_TOKEN", token);
        let helper_path = write_askpass_helper()?;
        cmd.env("GIT_ASKPASS", &helper_path);
    }

    let output = cmd
        .output()
        .map_err(|source| StackError::WorkspaceMaterializeFailed {
            reason: format!("spawning `git clone` failed: {source}"),
        })?;
    // Persist captured streams to disk so a failed clone is auditable
    // without re-running. Successful clones land the same capture so the
    // operator can inspect what git actually did. Write before the
    // exit-status check; the failure tail in `WorkspaceCommandFailed`
    // remains the primary error surface, but the full capture is the
    // audit copy.
    write_command_capture(
        log_dir,
        CAPTURE_TAG_GIT_CLONE,
        &output.stdout,
        &output.stderr,
    )?;
    if !output.status.success() {
        return Err(StackError::WorkspaceCommandFailed {
            command: "git clone",
            exit: output.status.code(),
            stderr_tail: tail_stderr_bytes(&output.stderr),
        });
    }
    Ok(())
}

fn run_git_rev_parse(dest: &Path, log_dir: Option<&Path>) -> Result<String> {
    let output = Command::new("git")
        .arg("rev-parse")
        .arg("HEAD")
        .current_dir(dest)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|source| StackError::WorkspaceMaterializeFailed {
            reason: format!("spawning `git rev-parse` failed: {source}"),
        })?;
    write_command_capture(
        log_dir,
        CAPTURE_TAG_GIT_REV_PARSE,
        &output.stdout,
        &output.stderr,
    )?;
    if !output.status.success() {
        return Err(StackError::WorkspaceCommandFailed {
            command: "git rev-parse HEAD",
            exit: output.status.code(),
            stderr_tail: tail_stderr_bytes(&output.stderr),
        });
    }
    let commit = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if commit.is_empty() {
        return Err(StackError::WorkspaceCommandFailed {
            command: "git rev-parse HEAD",
            exit: output.status.code(),
            stderr_tail: "command produced no output".to_owned(),
        });
    }
    Ok(commit)
}

fn write_askpass_helper() -> Result<PathBuf> {
    let dir =
        tempfile::TempDir::new().map_err(|source| StackError::WorkspaceMaterializeFailed {
            reason: format!("create askpass tempdir: {source}"),
        })?;
    // Leak the TempDir so the askpass helper survives long enough for
    // `git` to exec it. Acceptable in `acps init` (one-shot CLI). Newer
    // `tempfile` deprecates `into_path` in favor of `keep`.
    let path = dir.keep().join("askpass.sh");
    let script = "#!/bin/sh\nprintf %s \"$ACP_STACK_GIT_TOKEN\"\n";
    std::fs::write(&path, script).map_err(|source| StackError::WorkspaceMaterializeFailed {
        reason: format!("write askpass `{}`: {source}", path.display()),
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path)
            .map_err(|source| StackError::WorkspaceMaterializeFailed {
                reason: format!("stat askpass `{}`: {source}", path.display()),
            })?
            .permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(&path, perms).map_err(|source| {
            StackError::WorkspaceMaterializeFailed {
                reason: format!("chmod askpass `{}`: {source}", path.display()),
            }
        })?;
    }
    Ok(path)
}

// ---------- data lane ----------

fn materialize_data_source(
    index: usize,
    source: &DataSourceConfig,
    data_root: &Path,
    secrets: &SecretStore,
    log_dir: Option<&Path>,
) -> Result<SourceReport> {
    let name = derive_data_source_name(source)
        .map_err(|reason| StackError::WorkspaceDataSourceInvalid { index, reason })?;
    let dest = data_root.join(&name);

    match source.source_type.as_str() {
        "local" => materialize_local(index, source, &name, &dest, log_dir),
        "https" => materialize_https(index, source, &name, &dest, log_dir),
        "s3" => materialize_s3(index, source, &name, &dest, secrets, log_dir),
        other => Err(StackError::WorkspaceDataSourceInvalid {
            index,
            reason: format!("unsupported type `{other}`"),
        }),
    }
}

fn materialize_local(
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

    if let Some(existing) = sentinel_if_present(dest)? {
        if let SentinelBody::Local {
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

fn sentinel_if_present(dest: &Path) -> Result<Option<Sentinel>> {
    if !dest.exists() {
        return Ok(None);
    }
    Sentinel::read(dest)
}

#[derive(Debug, Default)]
struct CopyOutcome {
    bytes: u64,
    entries: u64,
}

fn copy_tree(src: &Path, dest: &Path) -> Result<CopyOutcome> {
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

fn copy_dir_recursive(src: &Path, dest: &Path, outcome: &mut CopyOutcome) -> Result<()> {
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

fn materialize_https(
    index: usize,
    source: &DataSourceConfig,
    name: &str,
    dest: &Path,
    log_dir: Option<&Path>,
) -> Result<SourceReport> {
    let url = source
        .url
        .as_deref()
        .ok_or_else(|| StackError::WorkspaceDataSourceInvalid {
            index,
            reason: "url is required".to_owned(),
        })?;

    ensure_destination_not_symlink(dest)?;
    let mut existing_sentinel_diverged = false;
    if let Some(existing) = sentinel_if_present(dest)?
        && let SentinelBody::Https {
            url: existing_url,
            sha256: existing_sha,
            ..
        } = &existing.body
        && existing_url == url
    {
        // Honor `expected_sha256` on every rerun: if the operator now pins
        // a hash, the stored sentinel must already match it. A diverging
        // pin forces a re-download (rather than silently trusting whatever
        // we fetched last time) — sentinel-skip must never weaken the
        // integrity guarantee that `expected_sha256` provides on a fresh
        // fetch.
        let pin_matches = source
            .expected_sha256
            .as_deref()
            .is_none_or(|expected| expected.eq_ignore_ascii_case(existing_sha));
        if pin_matches {
            return Ok(SourceReport {
                name: name.to_owned(),
                destination: dest.to_path_buf(),
                outcome: MaterializeOutcome::Verified,
                log_dir: None,
            });
        }
        existing_sentinel_diverged = true;
    }
    if existing_sentinel_diverged {
        // We trusted the sentinel enough to skip last time but the pin no
        // longer matches; wipe the old contents and re-fetch. The sentinel
        // was self-stamped, so removing the directory is safe.
        std::fs::remove_dir_all(dest).map_err(|source_err| {
            StackError::WorkspaceMaterializeFailed {
                reason: format!(
                    "remove stale destination `{}`: {source_err}",
                    dest.display()
                ),
            }
        })?;
    }
    ensure_dest_or_fail(dest)?;
    std::fs::create_dir_all(dest).map_err(|source_err| StackError::WorkspaceMaterializeFailed {
        reason: format!("create dest `{}`: {source_err}", dest.display()),
    })?;

    let mut opts = DownloadOpts::default();
    if let Some(limit) = source.max_download_bytes {
        opts.max_bytes = limit;
    }
    opts.expected_sha256 = source.expected_sha256.clone();

    let tmp = tempfile::NamedTempFile::new().map_err(|source_err| {
        StackError::WorkspaceMaterializeFailed {
            reason: format!("create download temp: {source_err}"),
        }
    })?;
    let report = match download_to_file(url, tmp.path(), &opts) {
        Ok(report) => report,
        Err(err) => {
            capture_error(log_dir, CAPTURE_TAG_DOWNLOAD, &err);
            return Err(err);
        }
    };
    write_operation_capture(
        log_dir,
        CAPTURE_TAG_DOWNLOAD,
        &format!(
            "url={url}\nfinal_url={}\nbytes={}\nsha256={}\ncontent_type={}\n",
            report.final_url,
            report.bytes_written,
            report.sha256,
            report.content_type.as_deref().unwrap_or(""),
        ),
        "",
    )?;
    let bytes = report.bytes_written;
    let sha256 = report.sha256.clone();

    let mut extract_opts = ExtractOpts::default();
    if let Some(limit) = source.max_extracted_bytes {
        extract_opts.max_total_bytes = limit;
    }
    let extracted = match extract_archive(tmp.path(), dest, &extract_opts) {
        Ok(report) => {
            write_operation_capture(
                log_dir,
                CAPTURE_TAG_EXTRACT,
                &format!(
                    "archive={}\ndestination={}\nentries={}\nbytes={}\ntop_level_dir={}\n",
                    tmp.path().display(),
                    dest.display(),
                    report.entries_written,
                    report.bytes_written,
                    report.top_level_dir.as_deref().unwrap_or(""),
                ),
                "",
            )
            .map_err(|err| cleanup_partial_destination(dest, err))?;
            true
        }
        Err(StackError::ArchiveUnsupportedFormat) => {
            write_operation_capture(
                log_dir,
                CAPTURE_TAG_EXTRACT,
                &format!(
                    "archive={}\ndestination={}\nunsupported_format=true\nfallback=copy\n",
                    tmp.path().display(),
                    dest.display(),
                ),
                "",
            )?;
            // Not an archive: place the file at <dest>/<basename> instead.
            let leaf = derive_leaf_from_url(url);
            let target = dest.join(leaf);
            let copied = match std::fs::copy(tmp.path(), &target) {
                Ok(bytes) => bytes,
                Err(source_err) => {
                    let err = StackError::WorkspaceMaterializeFailed {
                        reason: format!(
                            "copy downloaded body to `{}`: {source_err}",
                            target.display()
                        ),
                    };
                    capture_error(log_dir, CAPTURE_TAG_COPY, &err);
                    return Err(cleanup_partial_destination(dest, err));
                }
            };
            write_operation_capture(
                log_dir,
                CAPTURE_TAG_COPY,
                &format!(
                    "source={}\ndestination={}\nbytes={copied}\nentries=1\n",
                    tmp.path().display(),
                    target.display(),
                ),
                "",
            )
            .map_err(|err| cleanup_partial_destination(dest, err))?;
            false
        }
        Err(err) => {
            capture_error(log_dir, CAPTURE_TAG_EXTRACT, &err);
            return Err(cleanup_partial_destination(dest, err));
        }
    };

    let sentinel = Sentinel::new(SentinelBody::Https {
        url: url.to_owned(),
        sha256,
        bytes,
        extracted,
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

fn derive_leaf_from_url(url: &str) -> String {
    let trimmed = url.trim_end_matches('/');
    let leaf = trimmed
        .rsplit('/')
        .next()
        .and_then(|seg| seg.split('?').next())
        .filter(|seg| !seg.is_empty())
        .unwrap_or("payload");
    leaf.to_owned()
}

fn materialize_s3(
    index: usize,
    source: &DataSourceConfig,
    name: &str,
    dest: &Path,
    secrets: &SecretStore,
    log_dir: Option<&Path>,
) -> Result<SourceReport> {
    use crate::runtime::s3_client::{Credentials, S3Client};

    let bucket =
        source
            .bucket
            .as_deref()
            .ok_or_else(|| StackError::WorkspaceDataSourceInvalid {
                index,
                reason: "bucket is required for s3 sources".to_owned(),
            })?;
    let region =
        source
            .region
            .as_deref()
            .ok_or_else(|| StackError::WorkspaceDataSourceInvalid {
                index,
                reason: "region is required for s3 sources".to_owned(),
            })?;
    let access_ref =
        source
            .access_key_ref
            .as_deref()
            .ok_or_else(|| StackError::WorkspaceDataSourceInvalid {
                index,
                reason: "access_key_ref is required for s3 sources".to_owned(),
            })?;
    let secret_ref =
        source
            .secret_key_ref
            .as_deref()
            .ok_or_else(|| StackError::WorkspaceDataSourceInvalid {
                index,
                reason: "secret_key_ref is required for s3 sources".to_owned(),
            })?;
    let prefix = source.prefix.clone();
    let max_total_bytes = source
        .max_download_bytes
        .unwrap_or(crate::runtime::safe_download::DEFAULT_MAX_DOWNLOAD_BYTES);

    ensure_destination_not_symlink(dest)?;
    if let Some(existing) = sentinel_if_present(dest)?
        && let SentinelBody::S3 {
            bucket: existing_bucket,
            prefix: existing_prefix,
            region: existing_region,
            ..
        } = &existing.body
        && existing_bucket == bucket
        && existing_prefix.as_deref() == prefix.as_deref()
        && existing_region == region
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

    let access_key = secrets.get(access_ref)?.to_owned();
    let secret_key = secrets.get(secret_ref)?.to_owned();
    let mut client = S3Client::new(
        region.to_owned(),
        Credentials {
            access_key,
            secret_key,
        },
    )?;
    if let Ok(endpoint) = std::env::var("ACP_STACK_S3_ENDPOINT_OVERRIDE")
        && !endpoint.is_empty()
    {
        // Test/operator escape hatch for hitting a local MinIO mock or a
        // VPC-internal endpoint without baking it into TOML. Production
        // deployments leave the env var unset.
        client = client.with_endpoint_base(endpoint);
    }

    let outcome = download_s3_objects(&client, bucket, prefix.as_deref(), dest, max_total_bytes);
    let (bytes, objects) = match outcome {
        Ok(value) => {
            write_operation_capture(
                log_dir,
                CAPTURE_TAG_S3_DOWNLOAD,
                &format!(
                    "bucket={bucket}\nprefix={}\nregion={region}\ndestination={}\nbytes={}\nobjects={}\n",
                    prefix.as_deref().unwrap_or(""),
                    dest.display(),
                    value.0,
                    value.1,
                ),
                "",
            )
            .map_err(|err| cleanup_partial_destination(dest, err))?;
            value
        }
        Err(err) => {
            capture_error(log_dir, CAPTURE_TAG_S3_DOWNLOAD, &err);
            return Err(cleanup_partial_destination(dest, err));
        }
    };

    let sentinel = Sentinel::new(SentinelBody::S3 {
        bucket: bucket.to_owned(),
        prefix,
        region: region.to_owned(),
        bytes,
        objects,
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

fn download_s3_objects(
    client: &crate::runtime::s3_client::S3Client,
    bucket: &str,
    prefix: Option<&str>,
    dest: &Path,
    max_total_bytes: u64,
) -> Result<(u64, u64)> {
    let normalized_prefix = prefix.map(|p| p.trim_start_matches('/').to_owned());
    let mut total_bytes: u64 = 0;
    let mut total_objects: u64 = 0;
    let mut continuation: Option<String> = None;
    loop {
        let page = client.list_objects_v2(
            bucket,
            normalized_prefix.as_deref(),
            continuation.as_deref(),
        )?;
        for object in &page.objects {
            if object.key.ends_with('/') {
                continue;
            }
            let relative = match normalized_prefix.as_deref() {
                Some(prefix) => object
                    .key
                    .strip_prefix(prefix)
                    .unwrap_or(&object.key)
                    .trim_start_matches('/'),
                None => object.key.as_str(),
            };
            let safe = safe_object_path(relative)?;
            let target = dest.join(&safe);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|source_err| {
                    StackError::WorkspaceMaterializeFailed {
                        reason: format!("create dir `{}`: {source_err}", parent.display()),
                    }
                })?;
            }
            let projected = total_bytes.saturating_add(object.size);
            if projected > max_total_bytes {
                return Err(StackError::WorkspaceMaterializeFailed {
                    reason: format!(
                        "s3 ingest would exceed the {max_total_bytes}-byte size limit \
                         (after object `{}`)",
                        object.key
                    ),
                });
            }
            let bytes = client.get_object(bucket, &object.key, max_total_bytes - total_bytes)?;
            if total_bytes.saturating_add(bytes.len() as u64) > max_total_bytes {
                return Err(StackError::WorkspaceMaterializeFailed {
                    reason: format!("s3 object `{}` exceeded remaining size budget", object.key),
                });
            }
            std::fs::write(&target, &bytes).map_err(|source_err| {
                StackError::WorkspaceMaterializeFailed {
                    reason: format!("write s3 object `{}`: {source_err}", target.display()),
                }
            })?;
            total_bytes = total_bytes.saturating_add(bytes.len() as u64);
            total_objects = total_objects.saturating_add(1);
        }
        if !page.is_truncated || page.next_continuation_token.is_none() {
            break;
        }
        continuation = page.next_continuation_token;
    }
    Ok((total_bytes, total_objects))
}

fn safe_object_path(relative: &str) -> Result<PathBuf> {
    let mut out = PathBuf::new();
    for component in Path::new(relative).components() {
        match component {
            std::path::Component::Normal(part) => out.push(part),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                return Err(StackError::WorkspaceMaterializeFailed {
                    reason: format!("s3 object key `{relative}` contains a parent-dir segment"),
                });
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                return Err(StackError::WorkspaceMaterializeFailed {
                    reason: format!("s3 object key `{relative}` resolved to an absolute path"),
                });
            }
        }
    }
    if out.as_os_str().is_empty() {
        return Err(StackError::WorkspaceMaterializeFailed {
            reason: format!("s3 object key `{relative}` is empty after sanitization"),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CodeSourceConfig, DataSourceConfig, WorkspaceConfig};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::OnceLock;
    use tempfile::tempdir;

    fn empty_secret_store() -> SecretStore {
        let home = tempdir().expect("tempdir");
        SecretStore::open_or_create(home.path()).expect("secret store")
    }

    fn workspace_with(root: &Path) -> WorkspaceConfig {
        WorkspaceConfig {
            root: root.display().to_string(),
            uploads: root.join("uploads").display().to_string(),
            default_shell: "/bin/bash".to_owned(),
            runtime_user: "acp".to_owned(),
            max_file_bytes: 8_388_608,
            code_sources: Vec::new(),
            data_sources: Vec::new(),
        }
    }

    fn run_git_init(repo: &Path) {
        let status = Command::new("git")
            .arg("init")
            .arg("-q")
            .current_dir(repo)
            .status()
            .expect("git init");
        assert!(status.success());
        // Pin local user identity so commits work in CI sandboxes.
        for (k, v) in [
            ("user.email", "test@example.com"),
            ("user.name", "Test"),
            ("commit.gpgsign", "false"),
        ] {
            let status = Command::new("git")
                .args(["config", k, v])
                .current_dir(repo)
                .status()
                .expect("git config");
            assert!(status.success());
        }
    }

    fn git_commit_in(repo: &Path, file: &str, contents: &str, message: &str) {
        std::fs::write(repo.join(file), contents).expect("write");
        let status = Command::new("git")
            .args(["add", file])
            .current_dir(repo)
            .status()
            .expect("git add");
        assert!(status.success());
        let status = Command::new("git")
            .args(["commit", "-q", "-m", message])
            .current_dir(repo)
            .status()
            .expect("git commit");
        assert!(status.success());
    }

    fn git_available() -> bool {
        static AVAILABLE: OnceLock<bool> = OnceLock::new();
        *AVAILABLE.get_or_init(|| {
            Command::new("git")
                .arg("--version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        })
    }

    fn capture_names(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .expect("read capture dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect()
    }

    fn has_capture(names: &[String], tag: &str, extension: &str) -> bool {
        names
            .iter()
            .any(|name| name.starts_with(&format!("{tag}.")) && name.ends_with(extension))
    }

    #[test]
    fn no_op_when_both_lanes_empty() {
        let root_dir = tempdir().expect("root");
        let workspace = workspace_with(root_dir.path());
        let secrets = empty_secret_store();
        let report = materialize_workspace(&workspace, &secrets, None).expect("ok");
        assert!(report.code.is_empty());
        assert!(report.data.is_empty());
        assert!(!root_dir.path().join("usr").exists());
    }

    #[test]
    fn clones_git_source_and_records_sentinel() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let upstream = tempdir().expect("upstream");
        run_git_init(upstream.path());
        git_commit_in(upstream.path(), "README.md", "hello\n", "init");
        let root_dir = tempdir().expect("root");
        let mut workspace = workspace_with(root_dir.path());
        workspace.code_sources.push(CodeSourceConfig {
            source_type: "git".to_owned(),
            repo: Some(upstream.path().display().to_string()),
            branch: None,
            credential_ref: None,
            name: Some("upstream".to_owned()),
        });
        let secrets = empty_secret_store();
        let report = materialize_workspace(&workspace, &secrets, None).expect("ok");
        let entry = &report.code[0];
        assert_eq!(entry.name, "upstream");
        assert_eq!(entry.outcome, MaterializeOutcome::Created);
        let dest = root_dir.path().join(CODE_LANE_DIR).join("upstream");
        assert!(dest.join(".git").is_dir());
        assert!(dest.join("README.md").is_file());
        assert!(dest.join(SOURCE_SENTINEL_FILE).is_file());

        // Rerun is idempotent.
        let report2 = materialize_workspace(&workspace, &secrets, None).expect("rerun");
        assert_eq!(report2.code[0].outcome, MaterializeOutcome::Verified);
    }

    #[test]
    fn captures_git_clone_stdout_and_stderr_to_log_dir() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let upstream = tempdir().expect("upstream");
        run_git_init(upstream.path());
        git_commit_in(upstream.path(), "README.md", "hello\n", "init");
        let root_dir = tempdir().expect("root");
        let log_root = tempdir().expect("log root");
        let mut workspace = workspace_with(root_dir.path());
        workspace.code_sources.push(CodeSourceConfig {
            source_type: "git".to_owned(),
            repo: Some(upstream.path().display().to_string()),
            branch: None,
            credential_ref: None,
            name: Some("upstream".to_owned()),
        });
        let secrets = empty_secret_store();
        let log_paths = WorkspaceLogPaths::for_run(log_root.path(), "irun_test_run_001");
        let report = materialize_workspace(&workspace, &secrets, Some(&log_paths))
            .expect("clone with log capture");

        // The run-level log dir is recorded on the report so the init
        // orchestrator can stamp it onto the workspace_materialize
        // init_steps row.
        let report_log_dir = report
            .log_dir
            .as_ref()
            .expect("log_dir should be Some when log_paths supplied");
        assert_eq!(report_log_dir, &log_paths.run_dir);
        assert!(report_log_dir.is_dir(), "run-level log dir must exist");

        // Per-source log dir exists and carries the captured streams.
        let source_log_dir = report.code[0]
            .log_dir
            .as_ref()
            .expect("per-source log_dir set");
        assert!(source_log_dir.starts_with(&log_paths.run_dir));
        // Capture filenames carry a per-attempt nanosecond suffix so a
        // resume that re-runs the same step doesn't overwrite the prior
        // failure's capture. Match on the prefix.
        let captures = capture_names(source_log_dir);
        assert!(
            has_capture(&captures, CAPTURE_TAG_GIT_CLONE, ".stdout"),
            "no git-clone.*.stdout capture under {}: {captures:?}",
            source_log_dir.display(),
        );
        assert!(
            has_capture(&captures, CAPTURE_TAG_GIT_CLONE, ".stderr"),
            "no git-clone.*.stderr capture under {}: {captures:?}",
            source_log_dir.display(),
        );
        let rev_parse_stdout = captures
            .iter()
            .find(|n| {
                n.starts_with(&format!("{CAPTURE_TAG_GIT_REV_PARSE}.")) && n.ends_with(".stdout")
            })
            .expect("git-rev-parse.*.stdout capture missing");
        // git-rev-parse stdout for HEAD is the commit hash + newline.
        let head = std::fs::read_to_string(source_log_dir.join(rev_parse_stdout))
            .expect("read rev-parse stdout");
        assert!(
            head.trim().chars().all(|c| c.is_ascii_hexdigit()),
            "expected hex commit hash in capture, got `{head}`",
        );
    }

    #[cfg(unix)]
    #[test]
    fn capture_files_are_created_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().expect("tempdir");
        let written =
            write_command_capture(Some(dir.path()), CAPTURE_TAG_GIT_CLONE, b"out", b"err")
                .expect("capture");
        assert!(written.is_some());
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .filter_map(|e| e.ok())
            .collect();
        assert!(
            !entries.is_empty(),
            "capture must produce at least one file"
        );
        for entry in entries {
            let metadata = entry.metadata().expect("stat");
            let mode = metadata.permissions().mode() & 0o777;
            assert_eq!(
                mode,
                0o600,
                "capture file {:?} should be owner-only (0o600), got {:o}",
                entry.path(),
                mode,
            );
        }
    }

    #[test]
    fn capture_filenames_do_not_overwrite_across_repeated_calls() {
        // Regression: prior to the timestamp suffix, two `write_command_capture`
        // calls into the same dir clobbered each other, losing the first
        // attempt's audit copy when a resume retried. Each call must
        // produce a fresh pair of files.
        let dir = tempdir().expect("tempdir");
        let _ = write_command_capture(
            Some(dir.path()),
            CAPTURE_TAG_GIT_CLONE,
            b"first stdout",
            b"first stderr",
        )
        .expect("first write");
        // Spin briefly so the nanosecond stamp differs between calls
        // even on machines with coarse clocks. 1ms is enough on every
        // platform we ship to.
        std::thread::sleep(std::time::Duration::from_millis(2));
        let _ = write_command_capture(
            Some(dir.path()),
            CAPTURE_TAG_GIT_CLONE,
            b"second stdout",
            b"second stderr",
        )
        .expect("second write");
        let stdouts: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| {
                n.starts_with(&format!("{CAPTURE_TAG_GIT_CLONE}.")) && n.ends_with(".stdout")
            })
            .collect();
        assert_eq!(
            stdouts.len(),
            2,
            "expected 2 distinct stdout captures, got {stdouts:?}",
        );
    }

    #[test]
    fn rejects_existing_non_empty_destination_without_sentinel() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let upstream = tempdir().expect("upstream");
        run_git_init(upstream.path());
        git_commit_in(upstream.path(), "README.md", "x", "init");
        let root_dir = tempdir().expect("root");
        let mut workspace = workspace_with(root_dir.path());
        workspace.code_sources.push(CodeSourceConfig {
            source_type: "git".to_owned(),
            repo: Some(upstream.path().display().to_string()),
            branch: None,
            credential_ref: None,
            name: Some("upstream".to_owned()),
        });
        let dest = root_dir.path().join(CODE_LANE_DIR).join("upstream");
        std::fs::create_dir_all(&dest).expect("create");
        std::fs::write(dest.join("stowed.bin"), b"existing").expect("write");
        let secrets = empty_secret_store();
        let err = materialize_workspace(&workspace, &secrets, None).expect_err("non-empty");
        assert!(matches!(
            err,
            StackError::WorkspaceDestinationNotEmpty { .. }
        ));
    }

    #[test]
    fn copies_local_data_source_and_skips_on_rerun() {
        let upstream = tempdir().expect("upstream");
        let nested = upstream.path().join("inner");
        std::fs::create_dir(&nested).expect("inner");
        std::fs::write(upstream.path().join("a.txt"), b"alpha").expect("a");
        std::fs::write(nested.join("b.txt"), b"beta").expect("b");
        let root_dir = tempdir().expect("root");
        let mut workspace = workspace_with(root_dir.path());
        workspace.data_sources.push(DataSourceConfig {
            source_type: "local".to_owned(),
            name: Some("dataset".to_owned()),
            path: Some(upstream.path().display().to_string()),
            url: None,
            expected_sha256: None,
            max_download_bytes: None,
            max_extracted_bytes: None,
            bucket: None,
            prefix: None,
            region: None,
            access_key_ref: None,
            secret_key_ref: None,
        });
        let secrets = empty_secret_store();
        let report = materialize_workspace(&workspace, &secrets, None).expect("ok");
        assert_eq!(report.data[0].outcome, MaterializeOutcome::Created);
        let dest = root_dir.path().join(DATA_LANE_DIR).join("dataset");
        assert_eq!(std::fs::read(dest.join("a.txt")).expect("a"), b"alpha");
        assert_eq!(
            std::fs::read(dest.join("inner").join("b.txt")).expect("b"),
            b"beta"
        );
        assert!(dest.join(SOURCE_SENTINEL_FILE).is_file());
        let rerun = materialize_workspace(&workspace, &secrets, None).expect("rerun");
        assert_eq!(rerun.data[0].outcome, MaterializeOutcome::Verified);
    }

    #[test]
    fn captures_local_data_copy_to_log_dir() {
        let upstream = tempdir().expect("upstream");
        std::fs::write(upstream.path().join("dataset.txt"), b"alpha").expect("write");
        let root_dir = tempdir().expect("root");
        let log_root = tempdir().expect("log root");
        let mut workspace = workspace_with(root_dir.path());
        workspace.data_sources.push(DataSourceConfig {
            source_type: "local".to_owned(),
            name: Some("dataset".to_owned()),
            path: Some(upstream.path().display().to_string()),
            url: None,
            expected_sha256: None,
            max_download_bytes: None,
            max_extracted_bytes: None,
            bucket: None,
            prefix: None,
            region: None,
            access_key_ref: None,
            secret_key_ref: None,
        });
        let secrets = empty_secret_store();
        let log_paths = WorkspaceLogPaths::for_run(log_root.path(), "irun_data_local");
        let report =
            materialize_workspace(&workspace, &secrets, Some(&log_paths)).expect("materialize");
        let source_log_dir = report.data[0].log_dir.as_ref().expect("data log dir");
        assert!(source_log_dir.starts_with(&log_paths.run_dir));
        let captures = capture_names(source_log_dir);
        assert!(has_capture(&captures, CAPTURE_TAG_COPY, ".stdout"));
        assert!(has_capture(&captures, CAPTURE_TAG_COPY, ".stderr"));
        let copy_stdout = captures
            .iter()
            .find(|name| {
                name.starts_with(&format!("{CAPTURE_TAG_COPY}.")) && name.ends_with(".stdout")
            })
            .expect("copy stdout capture");
        let copy_stdout_text =
            std::fs::read_to_string(source_log_dir.join(copy_stdout)).expect("copy stdout text");
        assert!(copy_stdout_text.contains("bytes=5"));
        assert!(copy_stdout_text.contains("entries=1"));
    }

    #[test]
    fn captures_https_download_failure_to_log_dir() {
        let root_dir = tempdir().expect("root");
        let log_root = tempdir().expect("log root");
        let mut workspace = workspace_with(root_dir.path());
        workspace.data_sources.push(DataSourceConfig {
            source_type: "https".to_owned(),
            name: Some("dataset".to_owned()),
            path: None,
            url: Some("https://127.0.0.1:1/dataset.tar.gz".to_owned()),
            expected_sha256: None,
            max_download_bytes: None,
            max_extracted_bytes: None,
            bucket: None,
            prefix: None,
            region: None,
            access_key_ref: None,
            secret_key_ref: None,
        });
        let secrets = empty_secret_store();
        let log_paths = WorkspaceLogPaths::for_run(log_root.path(), "irun_data_https_fail");
        let err = materialize_workspace(&workspace, &secrets, Some(&log_paths))
            .expect_err("download should fail");
        assert!(
            matches!(err, StackError::SafeDownloadFailed { .. }),
            "got: {err:?}",
        );
        let data_log_dir = log_paths.run_dir.join("data-000");
        let captures = capture_names(&data_log_dir);
        assert!(has_capture(&captures, CAPTURE_TAG_DOWNLOAD, ".stderr"));
    }

    #[test]
    fn capture_failure_after_local_copy_cleans_partial_destination() {
        let upstream = tempdir().expect("upstream");
        std::fs::write(upstream.path().join("dataset.txt"), b"alpha").expect("write");
        let root_dir = tempdir().expect("root");
        let data_root = root_dir.path().join(DATA_LANE_DIR);
        std::fs::create_dir_all(&data_root).expect("data root");
        let dest = data_root.join("dataset");
        let log_file = tempfile::NamedTempFile::new().expect("log file");
        let source = DataSourceConfig {
            source_type: "local".to_owned(),
            name: Some("dataset".to_owned()),
            path: Some(upstream.path().display().to_string()),
            url: None,
            expected_sha256: None,
            max_download_bytes: None,
            max_extracted_bytes: None,
            bucket: None,
            prefix: None,
            region: None,
            access_key_ref: None,
            secret_key_ref: None,
        };

        let err = materialize_local(0, &source, "dataset", &dest, Some(log_file.path()))
            .expect_err("capture write should fail");
        assert!(
            matches!(err, StackError::WorkspaceMaterializeFailed { .. }),
            "got: {err:?}",
        );
        assert!(
            !dest.exists(),
            "partial destination must be removed after post-copy capture failure",
        );
    }

    #[test]
    fn failed_error_capture_does_not_mask_download_error() {
        let root_dir = tempdir().expect("root");
        let data_root = root_dir.path().join(DATA_LANE_DIR);
        std::fs::create_dir_all(&data_root).expect("data root");
        let dest = data_root.join("dataset");
        let log_file = tempfile::NamedTempFile::new().expect("log file");
        let source = DataSourceConfig {
            source_type: "https".to_owned(),
            name: Some("dataset".to_owned()),
            path: None,
            url: Some("https://127.0.0.1:1/dataset.tar.gz".to_owned()),
            expected_sha256: None,
            max_download_bytes: None,
            max_extracted_bytes: None,
            bucket: None,
            prefix: None,
            region: None,
            access_key_ref: None,
            secret_key_ref: None,
        };

        let err = materialize_https(0, &source, "dataset", &dest, Some(log_file.path()))
            .expect_err("download should fail");
        assert!(
            matches!(err, StackError::SafeDownloadFailed { .. }),
            "got: {err:?}",
        );
    }

    #[test]
    fn git_clone_against_nonexistent_path_surfaces_typed_command_failure() {
        if !git_available() {
            eprintln!("skip git_clone_against_nonexistent_path: git not in PATH");
            return;
        }
        let root_dir = tempdir().expect("root");
        let bogus = root_dir.path().join("does-not-exist");
        let mut workspace = workspace_with(root_dir.path());
        workspace.code_sources.push(CodeSourceConfig {
            source_type: "git".to_owned(),
            repo: Some(bogus.display().to_string()),
            branch: None,
            credential_ref: None,
            name: Some("bogus".to_owned()),
        });
        let secrets = empty_secret_store();
        let err = materialize_workspace(&workspace, &secrets, None)
            .expect_err("git clone of missing local repo must fail");
        match err {
            StackError::WorkspaceCommandFailed {
                command,
                stderr_tail,
                exit,
            } => {
                assert_eq!(command, "git clone");
                // Git surfaces an explanatory stderr; we just assert it's
                // non-empty so the typed variant carries a useful tail.
                assert!(!stderr_tail.is_empty(), "stderr_tail must not be empty");
                assert!(exit.is_some(), "git clone must report an exit code");
            }
            other => panic!("expected WorkspaceCommandFailed, got {other:?}"),
        }
    }

    #[test]
    fn rejects_local_source_containing_symlink() {
        let upstream = tempdir().expect("upstream");
        std::fs::write(upstream.path().join("real.txt"), b"x").expect("real");
        std::os::unix::fs::symlink("real.txt", upstream.path().join("link.txt")).expect("symlink");
        let root_dir = tempdir().expect("root");
        let mut workspace = workspace_with(root_dir.path());
        workspace.data_sources.push(DataSourceConfig {
            source_type: "local".to_owned(),
            name: Some("dataset".to_owned()),
            path: Some(upstream.path().display().to_string()),
            url: None,
            expected_sha256: None,
            max_download_bytes: None,
            max_extracted_bytes: None,
            bucket: None,
            prefix: None,
            region: None,
            access_key_ref: None,
            secret_key_ref: None,
        });
        let secrets = empty_secret_store();
        let err = materialize_workspace(&workspace, &secrets, None).expect_err("symlink");
        assert!(
            matches!(
                err,
                StackError::WorkspaceMaterializeFailed { ref reason } if reason.contains("symlink")
            ),
            "got: {err:?}"
        );
    }

    #[test]
    fn s3_source_materializes_against_mock_endpoint() {
        // Spin up a small axum server that speaks the S3 wire format we
        // need: a `?list-type=2` GET returns the canned XML, and
        // `/bucket/<key>` GETs return file bodies. Exercises the
        // SigV4-signed request path end-to-end (the mock ignores the
        // Authorization header — equivalent to a localhost MinIO with
        // signing disabled).
        use axum::Router;
        use axum::extract::{Path as AxumPath, Query};
        use axum::http::header;
        use axum::response::{IntoResponse, Response};
        use axum::routing::get;
        use std::collections::HashMap;
        use std::sync::Arc;
        use std::sync::Mutex;

        struct MockState {
            objects: Vec<(String, Vec<u8>)>,
        }
        let state = Arc::new(Mutex::new(MockState {
            objects: vec![
                ("data/one.txt".to_owned(), b"alpha".to_vec()),
                ("data/sub/two.bin".to_owned(), b"beta-body".to_vec()),
            ],
        }));

        let list_state = Arc::clone(&state);
        let get_state = Arc::clone(&state);
        let app = Router::new()
            .route(
                "/{bucket}",
                get(move |Query(query): Query<HashMap<String, String>>| {
                    let state = Arc::clone(&list_state);
                    async move {
                        let prefix = query.get("prefix").cloned().unwrap_or_default();
                        let body = {
                            let state = state.lock().expect("state");
                            let mut xml = String::from(
                                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<ListBucketResult>",
                            );
                            for (key, bytes) in &state.objects {
                                if !key.starts_with(&prefix) {
                                    continue;
                                }
                                xml.push_str(&format!(
                                    "<Contents><Key>{}</Key><Size>{}</Size><ETag>\"x\"</ETag></Contents>",
                                    key,
                                    bytes.len()
                                ));
                            }
                            xml.push_str("<IsTruncated>false</IsTruncated></ListBucketResult>");
                            xml
                        };
                        Response::builder()
                            .header(header::CONTENT_TYPE, "application/xml")
                            .body(axum::body::Body::from(body))
                            .unwrap()
                    }
                }),
            )
            .route(
                "/{bucket}/{*key}",
                get(move |AxumPath((_bucket, key)): AxumPath<(String, String)>| {
                    let state = Arc::clone(&get_state);
                    async move {
                        let state = state.lock().expect("state");
                        for (object_key, bytes) in &state.objects {
                            if object_key == &key {
                                return Response::builder()
                                    .header(header::CONTENT_TYPE, "application/octet-stream")
                                    .body(axum::body::Body::from(bytes.clone()))
                                    .unwrap();
                            }
                        }
                        (axum::http::StatusCode::NOT_FOUND, "missing").into_response()
                    }
                }),
            );

        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let handle = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build runtime");
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("bind");
                let addr = listener.local_addr().expect("addr");
                tx.send(addr).expect("send addr");
                axum::serve(listener, app).await.expect("serve");
            });
        });
        let addr = rx.recv().expect("mock addr");

        let home = tempdir().expect("home");
        let mut secrets = SecretStore::open_or_create(home.path()).expect("store");
        secrets
            .set_many([
                ("AWS_ACCESS_KEY_ID", "AKIAIOSFODNN7EXAMPLE"),
                (
                    "AWS_SECRET_ACCESS_KEY",
                    "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
                ),
            ])
            .expect("set secrets");

        let root_dir = tempdir().expect("root");
        let mut workspace = workspace_with(root_dir.path());
        workspace.data_sources.push(DataSourceConfig {
            source_type: "s3".to_owned(),
            name: Some("research".to_owned()),
            path: None,
            url: None,
            expected_sha256: None,
            max_download_bytes: None,
            max_extracted_bytes: None,
            bucket: Some("example".to_owned()),
            prefix: Some("data/".to_owned()),
            region: Some("us-east-1".to_owned()),
            access_key_ref: Some("AWS_ACCESS_KEY_ID".to_owned()),
            secret_key_ref: Some("AWS_SECRET_ACCESS_KEY".to_owned()),
        });

        let log_root = tempdir().expect("log root");
        let log_paths = WorkspaceLogPaths::for_run(log_root.path(), "irun_s3_mock");

        // SAFETY: tests in this binary share env; mock URL is per-test.
        // We unset on the way out so a panic mid-test still cleans up.
        unsafe {
            std::env::set_var("ACP_STACK_S3_ENDPOINT_OVERRIDE", format!("http://{addr}"));
        }
        let result = materialize_workspace(&workspace, &secrets, Some(&log_paths));
        unsafe {
            std::env::remove_var("ACP_STACK_S3_ENDPOINT_OVERRIDE");
        }

        let report = result.expect("s3 materialize");
        let dest = root_dir.path().join(DATA_LANE_DIR).join("research");
        assert_eq!(report.data[0].outcome, MaterializeOutcome::Created);
        assert_eq!(
            std::fs::read(dest.join("one.txt")).expect("one.txt"),
            b"alpha"
        );
        assert_eq!(
            std::fs::read(dest.join("sub/two.bin")).expect("two.bin"),
            b"beta-body"
        );
        assert!(dest.join(SOURCE_SENTINEL_FILE).is_file());
        let source_log_dir = report.data[0].log_dir.as_ref().expect("s3 log dir");
        assert!(source_log_dir.starts_with(&log_paths.run_dir));
        let captures = capture_names(source_log_dir);
        assert!(has_capture(&captures, CAPTURE_TAG_S3_DOWNLOAD, ".stdout"));
        assert!(has_capture(&captures, CAPTURE_TAG_S3_DOWNLOAD, ".stderr"));

        // Rerun must skip cleanly.
        unsafe {
            std::env::set_var("ACP_STACK_S3_ENDPOINT_OVERRIDE", format!("http://{addr}"));
        }
        let rerun = materialize_workspace(&workspace, &secrets, None);
        unsafe {
            std::env::remove_var("ACP_STACK_S3_ENDPOINT_OVERRIDE");
        }
        assert_eq!(
            rerun.expect("rerun").data[0].outcome,
            MaterializeOutcome::Verified
        );

        drop(handle); // Worker thread exits when the runtime drops with the
        // axum server attached. Detached thread is fine for tests.
    }

    #[test]
    fn s3_source_fails_when_secret_refs_missing() {
        // With s3 materialization wired but no secrets in the store, the
        // materializer should bail at secret resolution rather than making
        // a real AWS request. This locks in the gating order:
        // sentinel-check → ensure_dest_or_fail → create_dir → resolve
        // creds. Net effect: no network IO and no destination side effects.
        let root_dir = tempdir().expect("root");
        let mut workspace = workspace_with(root_dir.path());
        workspace.data_sources.push(DataSourceConfig {
            source_type: "s3".to_owned(),
            name: Some("dataset".to_owned()),
            path: None,
            url: None,
            expected_sha256: None,
            max_download_bytes: None,
            max_extracted_bytes: None,
            bucket: Some("example".to_owned()),
            prefix: Some("data/".to_owned()),
            region: Some("us-east-1".to_owned()),
            access_key_ref: Some("AWS_ACCESS_KEY_ID".to_owned()),
            secret_key_ref: Some("AWS_SECRET_ACCESS_KEY".to_owned()),
        });
        let secrets = empty_secret_store();
        let err = materialize_workspace(&workspace, &secrets, None).expect_err("missing secrets");
        assert!(
            matches!(err, StackError::SecretNotFound { .. }),
            "got: {err:?}"
        );
    }

    #[test]
    fn askpass_helper_is_executable() {
        let path = write_askpass_helper().expect("ok");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        let _ = std::fs::remove_file(&path);
    }
}
