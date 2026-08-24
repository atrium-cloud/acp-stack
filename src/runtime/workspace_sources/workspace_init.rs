//! Seeds `<workspace.root>/usr/code/<repo>/` and `<workspace.root>/usr/data/<name>/` from the
//! configured code and data sources during `acps init`. Materialization is not transactional across
//! sources: each completed source drops a sentinel, and a non-empty destination without a matching
//! sentinel is a hard failure rather than a best-effort merge.

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
use self::common::{
    ensure_lane_root, ensure_workspace_base_dir, ensure_workspace_log_dir, sanitize_segment,
};
use self::https::materialize_https;
use self::local::materialize_local;
use self::s3::materialize_s3;

/// Sentinel written into each materialized destination so reruns can skip completed lanes.
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

/// Cap on stored stderr from materializer subprocesses, matching `agent_installer::tail_bytes`.
pub(super) const WORKSPACE_STDERR_TAIL_BYTES: usize = 2 * 1024;

/// Canonical on-disk root for workspace materialization logs.
pub fn default_workspace_init_log_base(home: &Path) -> PathBuf {
    home.join(".local")
        .join("share")
        .join("acp-stack")
        .join("workspace-init-logs")
}

/// Per-run capture location for workspace materialization, one per `init_runs.id`.
#[derive(Debug, Clone)]
pub struct WorkspaceLogPaths {
    /// `<log_base>/<init_run_id>/`, recorded as the init step row's `log_dir`.
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
    /// Directory created and source content fetched from scratch.
    Created,
    /// Sentinel matched existing config; skipped without touching the filesystem.
    Verified,
}

#[derive(Debug, Clone)]
pub struct SourceReport {
    pub name: String,
    pub destination: PathBuf,
    pub outcome: MaterializeOutcome,
    /// Capture directory under `WorkspaceLogPaths.run_dir`; `None` on a verifier-only skip.
    pub log_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Default)]
pub struct MaterializeReport {
    pub root: PathBuf,
    pub uploads: PathBuf,
    pub code: Vec<SourceReport>,
    pub data: Vec<SourceReport>,
    /// Root capture directory shared by every source in this run, pre-created even if none ran.
    pub log_dir: Option<PathBuf>,
}

/// Resume verifier for `workspace_materialize`: true when every declared source already has its sentinel.
pub fn all_sources_have_sentinel(workspace: &WorkspaceConfig) -> Result<bool> {
    if !workspace_base_dirs_exist(workspace) {
        return Ok(false);
    }
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

fn workspace_base_dirs_exist(workspace: &WorkspaceConfig) -> bool {
    Path::new(&workspace.root).is_dir() && Path::new(&workspace.uploads).is_dir()
}

pub fn prepare_workspace_base_dirs(workspace: &WorkspaceConfig) -> Result<()> {
    let root = Path::new(&workspace.root);
    if !root.is_absolute() {
        return Err(StackError::WorkspaceMaterializeFailed {
            reason: format!(
                "workspace.root `{}` must be absolute for materialization",
                workspace.root
            ),
        });
    }

    ensure_workspace_base_dir(root, "workspace.root")?;
    ensure_workspace_base_dir(Path::new(&workspace.uploads), "workspace.uploads")
}

/// Prepare the workspace root/uploads directories and materialize every declared code and data source.
/// With `log_paths`, each operation writes capture pairs under `run_dir/<source-tag>/`; without it,
/// failures only carry a stderr tail.
pub fn materialize_workspace(
    workspace: &WorkspaceConfig,
    secrets: &SecretStore,
    log_paths: Option<&WorkspaceLogPaths>,
) -> Result<MaterializeReport> {
    let root = Path::new(&workspace.root);
    let code_root = root.join(CODE_LANE_DIR);
    let data_root = root.join(DATA_LANE_DIR);
    let uploads = Path::new(&workspace.uploads);

    let mut report = MaterializeReport {
        root: root.to_path_buf(),
        uploads: uploads.to_path_buf(),
        log_dir: log_paths.map(|p| p.run_dir.clone()),
        ..MaterializeReport::default()
    };

    if let Some(paths) = log_paths {
        ensure_workspace_log_dir(&paths.run_dir)?;
    }

    prepare_workspace_base_dirs(workspace)?;

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
mod tests;
