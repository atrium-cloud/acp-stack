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

mod code_git;
mod common;
mod https;
mod local;
mod s3;

use std::path::{Path, PathBuf};

use crate::config::{
    DataSourceConfig, WorkspaceConfig, derive_code_source_name, derive_data_source_name,
};
use crate::error::{Result, StackError};
use crate::secrets::SecretStore;

use self::code_git::materialize_code_source;
use self::common::{ensure_lane_root, ensure_workspace_log_dir, sanitize_segment};
use self::https::materialize_https;
use self::local::materialize_local;
use self::s3::materialize_s3;

/// Sentinel filename written into each materialized destination so reruns
/// of `acps init` can detect "already done" lanes and skip cleanly.
pub const SOURCE_SENTINEL_FILE: &str = ".acp-stack-source.json";

/// Subdirectory under `workspace.root` for code lanes.
pub const CODE_LANE_DIR: &str = "usr/code";
/// Subdirectory under `workspace.root` for data lanes.
pub const DATA_LANE_DIR: &str = "usr/data";

pub(super) const CAPTURE_TAG_GIT_CLONE: &str = "git-clone";
pub(super) const CAPTURE_TAG_GIT_REV_PARSE: &str = "git-rev-parse";
pub(super) const CAPTURE_TAG_DOWNLOAD: &str = "download";
pub(super) const CAPTURE_TAG_EXTRACT: &str = "extract";
pub(super) const CAPTURE_TAG_COPY: &str = "copy";
pub(super) const CAPTURE_TAG_S3_DOWNLOAD: &str = "s3-download";

/// Cap stored stderr from materializer subprocesses (git, curl, tar) so a
/// chatty failure does not poison the error variant. Matches the 2 KiB tail
/// used by `agent_installer::tail_bytes` for installer-step stderr; operators
/// expecting consistent failure ergonomics get the same envelope here.
pub(super) const WORKSPACE_STDERR_TAIL_BYTES: usize = 2 * 1024;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CodeSourceConfig, DataSourceConfig, WorkspaceConfig};
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Stdio};
    use std::sync::OnceLock;
    use tempfile::tempdir;

    use super::code_git::write_askpass_helper;
    use super::common::write_command_capture;
    use super::https::materialize_https;
    use super::local::materialize_local;

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
