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
}

#[derive(Debug, Clone, Default)]
pub struct MaterializeReport {
    pub code: Vec<SourceReport>,
    pub data: Vec<SourceReport>,
}

/// Materialize every declared code and data source. No-op when both
/// vectors are empty.
pub fn materialize_workspace(
    workspace: &WorkspaceConfig,
    secrets: &SecretStore,
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

    let mut report = MaterializeReport::default();

    for (index, source) in workspace.code_sources.iter().enumerate() {
        ensure_lane_root(&code_root)?;
        report
            .code
            .push(materialize_code_source(index, source, &code_root, secrets)?);
    }
    for (index, source) in workspace.data_sources.iter().enumerate() {
        ensure_lane_root(&data_root)?;
        report
            .data
            .push(materialize_data_source(index, source, &data_root, secrets)?);
    }

    Ok(report)
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

    let outcome = run_git_clone(repo, source.branch.as_deref(), credential.as_deref(), &dest);
    if let Err(err) = outcome {
        // Clean up the partially-clobbered destination so a rerun can retry.
        return Err(cleanup_partial_destination(&dest, err));
    }

    let commit = run_git_rev_parse(&dest)?;
    Sentinel::new(SentinelBody::Git {
        repo: repo.to_owned(),
        branch: source.branch.clone(),
        commit,
    })
    .write(&dest)?;

    Ok(SourceReport {
        name,
        destination: dest,
        outcome: MaterializeOutcome::Created,
    })
}

fn run_git_clone(
    repo: &str,
    branch: Option<&str>,
    credential: Option<&str>,
    dest: &Path,
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
    if !output.status.success() {
        return Err(StackError::WorkspaceMaterializeFailed {
            reason: format!(
                "`git clone` exited {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }
    Ok(())
}

fn run_git_rev_parse(dest: &Path) -> Result<String> {
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
    if !output.status.success() {
        return Err(StackError::WorkspaceMaterializeFailed {
            reason: format!(
                "`git rev-parse HEAD` exited {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }
    let commit = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if commit.is_empty() {
        return Err(StackError::WorkspaceMaterializeFailed {
            reason: "`git rev-parse HEAD` returned empty output".to_owned(),
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
) -> Result<SourceReport> {
    let name = derive_data_source_name(source)
        .map_err(|reason| StackError::WorkspaceDataSourceInvalid { index, reason })?;
    let dest = data_root.join(&name);

    match source.source_type.as_str() {
        "local" => materialize_local(index, source, &name, &dest),
        "https" => materialize_https(index, source, &name, &dest),
        "s3" => materialize_s3(index, source, &name, &dest, secrets),
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
            });
        }
    }
    ensure_dest_or_fail(dest)?;
    std::fs::create_dir_all(dest).map_err(|source_err| StackError::WorkspaceMaterializeFailed {
        reason: format!("create dest `{}`: {source_err}", dest.display()),
    })?;

    let copy = copy_tree(&canonical_src, dest)?;

    Sentinel::new(SentinelBody::Local {
        path: canonical_src.display().to_string(),
        bytes: copy.bytes,
        entries: copy.entries,
    })
    .write(dest)?;

    Ok(SourceReport {
        name: name.to_owned(),
        destination: dest.to_path_buf(),
        outcome: MaterializeOutcome::Created,
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
    let report = download_to_file(url, tmp.path(), &opts)?;
    let bytes = report.bytes_written;
    let sha256 = report.sha256.clone();

    let mut extract_opts = ExtractOpts::default();
    if let Some(limit) = source.max_extracted_bytes {
        extract_opts.max_total_bytes = limit;
    }
    let extracted = match extract_archive(tmp.path(), dest, &extract_opts) {
        Ok(_) => true,
        Err(StackError::ArchiveUnsupportedFormat) => {
            // Not an archive: place the file at <dest>/<basename> instead.
            let leaf = derive_leaf_from_url(url);
            let target = dest.join(leaf);
            std::fs::copy(tmp.path(), &target).map_err(|source_err| {
                StackError::WorkspaceMaterializeFailed {
                    reason: format!(
                        "copy downloaded body to `{}`: {source_err}",
                        target.display()
                    ),
                }
            })?;
            false
        }
        Err(err) => {
            return Err(cleanup_partial_destination(dest, err));
        }
    };

    Sentinel::new(SentinelBody::Https {
        url: url.to_owned(),
        sha256,
        bytes,
        extracted,
    })
    .write(dest)?;

    Ok(SourceReport {
        name: name.to_owned(),
        destination: dest.to_path_buf(),
        outcome: MaterializeOutcome::Created,
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
        Ok(value) => value,
        Err(err) => {
            return Err(cleanup_partial_destination(dest, err));
        }
    };

    Sentinel::new(SentinelBody::S3 {
        bucket: bucket.to_owned(),
        prefix,
        region: region.to_owned(),
        bytes,
        objects,
    })
    .write(dest)?;

    Ok(SourceReport {
        name: name.to_owned(),
        destination: dest.to_path_buf(),
        outcome: MaterializeOutcome::Created,
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

    #[test]
    fn no_op_when_both_lanes_empty() {
        let root_dir = tempdir().expect("root");
        let workspace = workspace_with(root_dir.path());
        let secrets = empty_secret_store();
        let report = materialize_workspace(&workspace, &secrets).expect("ok");
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
        let report = materialize_workspace(&workspace, &secrets).expect("ok");
        let entry = &report.code[0];
        assert_eq!(entry.name, "upstream");
        assert_eq!(entry.outcome, MaterializeOutcome::Created);
        let dest = root_dir.path().join(CODE_LANE_DIR).join("upstream");
        assert!(dest.join(".git").is_dir());
        assert!(dest.join("README.md").is_file());
        assert!(dest.join(SOURCE_SENTINEL_FILE).is_file());

        // Rerun is idempotent.
        let report2 = materialize_workspace(&workspace, &secrets).expect("rerun");
        assert_eq!(report2.code[0].outcome, MaterializeOutcome::Verified);
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
        let err = materialize_workspace(&workspace, &secrets).expect_err("non-empty");
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
        let report = materialize_workspace(&workspace, &secrets).expect("ok");
        assert_eq!(report.data[0].outcome, MaterializeOutcome::Created);
        let dest = root_dir.path().join(DATA_LANE_DIR).join("dataset");
        assert_eq!(std::fs::read(dest.join("a.txt")).expect("a"), b"alpha");
        assert_eq!(
            std::fs::read(dest.join("inner").join("b.txt")).expect("b"),
            b"beta"
        );
        assert!(dest.join(SOURCE_SENTINEL_FILE).is_file());
        let rerun = materialize_workspace(&workspace, &secrets).expect("rerun");
        assert_eq!(rerun.data[0].outcome, MaterializeOutcome::Verified);
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
        let err = materialize_workspace(&workspace, &secrets).expect_err("symlink");
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

        // SAFETY: tests in this binary share env; mock URL is per-test.
        // We unset on the way out so a panic mid-test still cleans up.
        unsafe {
            std::env::set_var("ACP_STACK_S3_ENDPOINT_OVERRIDE", format!("http://{addr}"));
        }
        let result = materialize_workspace(&workspace, &secrets);
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

        // Rerun must skip cleanly.
        unsafe {
            std::env::set_var("ACP_STACK_S3_ENDPOINT_OVERRIDE", format!("http://{addr}"));
        }
        let rerun = materialize_workspace(&workspace, &secrets);
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
        let err = materialize_workspace(&workspace, &secrets).expect_err("missing secrets");
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
