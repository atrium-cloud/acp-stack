use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::parse_duration_string;
use crate::config::{Config, StackUpdatePolicy};
use crate::dev_gates::{GITHUB_API_BASE_ENV, INSTALL_BINARY_DIR_ENV, fixture_path, fixture_string};
use crate::error::{Result, StackError};
use crate::state::{
    EVENT_SOURCE_CLI, NewStackUpdateRun, STACK_UPDATE_OPERATION_CHECK,
    STACK_UPDATE_OPERATION_INSTALL, STACK_UPDATE_STATUS_FAILED, STACK_UPDATE_STATUS_SKIPPED,
    STACK_UPDATE_STATUS_SUCCEEDED, StateStore,
};

const GITHUB_API_BASE: &str = "https://api.github.com";
const REPOSITORY: &str = "atrium-cloud/acp-stack";
const MANIFEST_ASSET: &str = "acps-release.json";
const CHECKSUMS_ASSET: &str = "SHA256SUMS";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const USER_AGENT: &str = concat!("acp-stack/", env!("CARGO_PKG_VERSION"));
const BINARIES: &[&str] = &["acps"];
const REMOVED_BINARIES: &[&str] = &["acpctl"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackUpdateTarget {
    Latest,
    Version,
}

#[derive(Debug, Clone)]
pub struct StackUpdateOptions {
    pub target: StackUpdateTarget,
    pub version: Option<String>,
    pub allow_breaking: bool,
    pub auto: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct StackUpdateReport {
    pub current_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub classification: Option<StackReleaseClassification>,
    pub breaking: bool,
    pub major_upgrade: bool,
    pub policy: StackUpdatePolicy,
    pub auto: bool,
    pub decision: StackUpdateDecision,
    pub status: StackUpdateStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StackUpdateDecision {
    Install,
    UpToDate,
    Blocked,
    ManualOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StackUpdateStatus {
    Checked,
    Installed,
    Skipped,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StackReleaseClassification {
    Regular,
    SecurityCritical,
}

impl StackReleaseClassification {
    fn as_str(self) -> &'static str {
        match self {
            Self::Regular => "regular",
            Self::SecurityCritical => "security-critical",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct StackReleaseManifest {
    schema_version: u64,
    repository: String,
    tag: String,
    version: String,
    classification: StackReleaseClassification,
    breaking: bool,
    artifacts: Vec<StackReleaseArtifact>,
}

#[derive(Debug, Clone, Deserialize)]
struct StackReleaseArtifact {
    target: String,
    archive: String,
    sha256: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ReleaseResponse {
    tag_name: String,
    prerelease: bool,
    assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Clone, Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Debug, Clone)]
struct ResolvedStackUpdate {
    report: StackUpdateReport,
    release: Option<ReleaseResponse>,
    manifest: Option<StackReleaseManifest>,
}

pub fn check_stack_update(
    config: &Config,
    state: &StateStore,
    options: StackUpdateOptions,
) -> Result<StackUpdateReport> {
    let result = resolve_update_candidate(config, &options).map(|mut resolved| {
        resolved.report.status = StackUpdateStatus::Checked;
        resolved.report
    });
    persist_update_result(state, STACK_UPDATE_OPERATION_CHECK, options.auto, &result)
}

pub fn install_stack_update(
    config: &Config,
    state: &StateStore,
    options: StackUpdateOptions,
) -> Result<StackUpdateReport> {
    if options.auto
        && let Some(report) = auto_frequency_skip_report(config, state)?
    {
        return persist_update_result(
            state,
            STACK_UPDATE_OPERATION_INSTALL,
            options.auto,
            &Ok(report),
        );
    }
    let result = install_stack_update_inner(config, &options);
    persist_update_result(state, STACK_UPDATE_OPERATION_INSTALL, options.auto, &result)
}

fn auto_frequency_skip_report(
    config: &Config,
    state: &StateStore,
) -> Result<Option<StackUpdateReport>> {
    let frequency = parse_duration_string(&config.updates.acp_stack.frequency).ok_or(
        StackError::InvalidDurationField {
            field: "updates.acp_stack.frequency",
        },
    )?;
    // Skip rows are themselves persisted as INSTALL+auto runs stamped at "now";
    // using one as the frequency reference would re-arm the window on every
    // timer fire and never let an update through once frequency exceeds the
    // timer cadence. Only runs that actually attempted an update count, and
    // the query must not be bounded by a recent-row window that accumulated
    // skip rows could push the real attempt out of.
    let Some(recent) = state.latest_stack_auto_install_attempt()? else {
        return Ok(None);
    };
    let Ok(started_at) = chrono::DateTime::parse_from_rfc3339(&recent.started_at) else {
        return Ok(None);
    };
    let elapsed = Utc::now().signed_duration_since(started_at.with_timezone(&Utc));
    if elapsed.to_std().is_ok_and(|elapsed| elapsed < frequency) {
        return Ok(Some(StackUpdateReport {
            current_version: env!("CARGO_PKG_VERSION").to_owned(),
            target_version: None,
            target_tag: None,
            classification: None,
            breaking: false,
            major_upgrade: false,
            policy: config.updates.acp_stack.policy,
            auto: true,
            decision: StackUpdateDecision::UpToDate,
            status: StackUpdateStatus::Skipped,
            message: Some(format!(
                "auto-update checked recently; next check waits for {}",
                config.updates.acp_stack.frequency
            )),
        }));
    }
    Ok(None)
}

fn install_stack_update_inner(
    config: &Config,
    options: &StackUpdateOptions,
) -> Result<StackUpdateReport> {
    let resolved = resolve_update_candidate(config, options)?;
    let mut report = resolved.report;
    if !options.auto
        && report.decision == StackUpdateDecision::ManualOnly
        && resolved.manifest.is_some()
    {
        report.decision = StackUpdateDecision::Install;
        report.message = report
            .target_tag
            .as_ref()
            .map(|tag| format!("{tag} selected by explicit install command"));
    }
    if report.decision != StackUpdateDecision::Install {
        report.status = StackUpdateStatus::Skipped;
        return Ok(report);
    }
    if running_in_container() {
        report.status = StackUpdateStatus::Skipped;
        report.decision = StackUpdateDecision::ManualOnly;
        report.message = Some(
            "container deployments are check-only; redeploy the Docker/Railway image".to_owned(),
        );
        return Ok(report);
    }
    let release = resolved.release.ok_or_else(|| StackError::InvalidParam {
        field: "acps.update.install",
        reason: "selected release metadata was not available".to_owned(),
    })?;
    let manifest = resolved.manifest.ok_or_else(|| StackError::InvalidParam {
        field: "acps.update.install",
        reason: "selected release manifest was not available".to_owned(),
    })?;
    let artifact = manifest_artifact_for_host(&manifest)?;
    let asset = release
        .assets
        .iter()
        .find(|asset| asset.name == artifact.archive)
        .ok_or_else(|| StackError::GithubReleaseAssetNotFound {
            repo: REPOSITORY.to_owned(),
            pattern: artifact.archive.clone(),
        })?;
    let archive = download_bytes(&asset.browser_download_url)?;
    verify_artifact_sha256(&artifact.archive, &archive, &artifact.sha256)?;
    let binary_dir = install_binary_dir()?;
    if !directory_is_writable(&binary_dir) {
        return Err(StackError::InvalidParam {
            field: "acps.update.install",
            reason: format!(
                "{} is not writable; run the systemd updater as root or install with sudo",
                binary_dir.display()
            ),
        });
    }
    install_archive(&archive, &binary_dir)?;
    report.status = StackUpdateStatus::Installed;
    report.message = Some(format!(
        "installed acp-stack {}",
        report
            .target_tag
            .as_deref()
            .unwrap_or_else(|| report.target_version.as_deref().unwrap_or("unknown"))
    ));
    Ok(report)
}

fn persist_update_result(
    state: &StateStore,
    operation: &'static str,
    auto: bool,
    result: &Result<StackUpdateReport>,
) -> Result<StackUpdateReport> {
    let report = match result {
        Ok(report) => report.clone(),
        Err(err) => failure_report(auto, err.to_string()),
    };
    let status = match report.status {
        StackUpdateStatus::Installed | StackUpdateStatus::Checked => STACK_UPDATE_STATUS_SUCCEEDED,
        StackUpdateStatus::Skipped => STACK_UPDATE_STATUS_SKIPPED,
        StackUpdateStatus::Failed => STACK_UPDATE_STATUS_FAILED,
    };
    let payload = serde_json::to_string(&report).map_err(|source| StackError::ConfigWrite {
        path: PathBuf::from("stack-update-report.json"),
        source: std::io::Error::other(source),
    })?;
    state.append_stack_update_run(NewStackUpdateRun {
        operation,
        status,
        current_version: &report.current_version,
        target_version: report.target_version.as_deref(),
        target_tag: report.target_tag.as_deref(),
        classification: report
            .classification
            .map(StackReleaseClassification::as_str),
        breaking: report.breaking,
        major_upgrade: report.major_upgrade,
        policy: policy_as_str(report.policy),
        auto,
        message: report.message.as_deref(),
        payload_json: &payload,
    })?;
    let event_kind = match report.status {
        StackUpdateStatus::Checked => "stack.update.checked",
        StackUpdateStatus::Installed => "stack.update.installed",
        StackUpdateStatus::Skipped => "stack.update.skipped",
        StackUpdateStatus::Failed => "stack.update.failed",
    };
    let level = if report.status == StackUpdateStatus::Failed {
        "error"
    } else {
        "info"
    };
    state.append_event_with_source(
        level,
        event_kind,
        EVENT_SOURCE_CLI,
        report.message.as_deref().unwrap_or(event_kind),
        &payload,
    )?;
    match result {
        Ok(_) => Ok(report),
        Err(err) => Err(StackError::AgentInitializeFailed {
            reason: format!("acp-stack update failed: {err}"),
        }),
    }
}

fn failure_report(auto: bool, message: String) -> StackUpdateReport {
    StackUpdateReport {
        current_version: env!("CARGO_PKG_VERSION").to_owned(),
        target_version: None,
        target_tag: None,
        classification: None,
        breaking: false,
        major_upgrade: false,
        policy: StackUpdatePolicy::Manual,
        auto,
        decision: StackUpdateDecision::Blocked,
        status: StackUpdateStatus::Failed,
        message: Some(message),
    }
}

fn resolve_update_candidate(
    config: &Config,
    options: &StackUpdateOptions,
) -> Result<ResolvedStackUpdate> {
    let release = fetch_release(options)?;
    if release.prerelease && options.target == StackUpdateTarget::Latest {
        return Ok(ResolvedStackUpdate {
            report: StackUpdateReport {
                current_version: env!("CARGO_PKG_VERSION").to_owned(),
                target_version: None,
                target_tag: Some(release.tag_name),
                classification: None,
                breaking: false,
                major_upgrade: false,
                policy: config.updates.acp_stack.policy,
                auto: options.auto,
                decision: StackUpdateDecision::ManualOnly,
                status: StackUpdateStatus::Checked,
                message: Some(
                    "latest release is a prerelease; exact --version is required".to_owned(),
                ),
            },
            release: None,
            manifest: None,
        });
    }
    let manifest = match fetch_manifest(&release).and_then(|manifest| {
        validate_manifest(&manifest, &release)?;
        Ok(manifest)
    }) {
        Ok(manifest) => manifest,
        Err(StackError::GithubReleaseAssetNotFound { pattern, .. })
            if pattern == MANIFEST_ASSET =>
        {
            return Ok(ResolvedStackUpdate {
                report: StackUpdateReport {
                    current_version: env!("CARGO_PKG_VERSION").to_owned(),
                    target_version: None,
                    target_tag: Some(release.tag_name),
                    classification: None,
                    breaking: false,
                    major_upgrade: false,
                    policy: config.updates.acp_stack.policy,
                    auto: options.auto,
                    decision: StackUpdateDecision::ManualOnly,
                    status: StackUpdateStatus::Checked,
                    message: Some(
                        "release manifest is missing; update requires manual review".to_owned(),
                    ),
                },
                release: None,
                manifest: None,
            });
        }
        Err(err) => {
            return Err(err);
        }
    };
    let current_version = env!("CARGO_PKG_VERSION").to_owned();
    let major_upgrade = is_major_upgrade(&current_version, &manifest.version);
    let decision = update_decision(
        config.updates.acp_stack.policy,
        &current_version,
        &manifest,
        major_upgrade,
        options.allow_breaking,
        options.auto,
    );
    let message = update_message(decision, &manifest, major_upgrade);
    Ok(ResolvedStackUpdate {
        report: StackUpdateReport {
            current_version,
            target_version: Some(manifest.version.clone()),
            target_tag: Some(manifest.tag.clone()),
            classification: Some(manifest.classification),
            breaking: manifest.breaking,
            major_upgrade,
            policy: config.updates.acp_stack.policy,
            auto: options.auto,
            decision,
            status: StackUpdateStatus::Checked,
            message,
        },
        release: Some(release),
        manifest: Some(manifest),
    })
}

fn update_decision(
    policy: StackUpdatePolicy,
    current_version: &str,
    manifest: &StackReleaseManifest,
    major_upgrade: bool,
    allow_breaking: bool,
    auto: bool,
) -> StackUpdateDecision {
    if normalize_version(current_version) == normalize_version(&manifest.version) {
        return StackUpdateDecision::UpToDate;
    }
    // Auto mode must never downgrade: if upstream `latest` resolves below the
    // running version (e.g. a newer release was yanked), leave the rollback
    // decision to an explicit manual install command.
    if auto && is_version_downgrade(current_version, &manifest.version) {
        return StackUpdateDecision::ManualOnly;
    }
    if policy == StackUpdatePolicy::Manual && auto {
        return StackUpdateDecision::ManualOnly;
    }
    if (manifest.breaking || major_upgrade) && !allow_breaking {
        return StackUpdateDecision::Blocked;
    }
    match policy {
        StackUpdatePolicy::Manual => StackUpdateDecision::ManualOnly,
        StackUpdatePolicy::Compatible => StackUpdateDecision::Install,
        StackUpdatePolicy::SecurityCritical => {
            if manifest.classification == StackReleaseClassification::SecurityCritical {
                StackUpdateDecision::Install
            } else {
                StackUpdateDecision::ManualOnly
            }
        }
    }
}

fn update_message(
    decision: StackUpdateDecision,
    manifest: &StackReleaseManifest,
    major_upgrade: bool,
) -> Option<String> {
    match decision {
        StackUpdateDecision::Install => Some(format!("{} is eligible to install", manifest.tag)),
        StackUpdateDecision::UpToDate => Some("acp-stack is up to date".to_owned()),
        StackUpdateDecision::Blocked if manifest.breaking => {
            Some(format!("{} is marked breaking", manifest.tag))
        }
        StackUpdateDecision::Blocked if major_upgrade => {
            Some(format!("{} is a major-version upgrade", manifest.tag))
        }
        StackUpdateDecision::Blocked => Some(format!("{} is blocked by policy", manifest.tag)),
        StackUpdateDecision::ManualOnly => {
            Some(format!("{} requires a manual update command", manifest.tag))
        }
    }
}

fn fetch_release(options: &StackUpdateOptions) -> Result<ReleaseResponse> {
    let base = github_api_base();
    let url = match (options.target, options.version.as_deref()) {
        (StackUpdateTarget::Latest, _) => format!("{base}/repos/{REPOSITORY}/releases/latest"),
        (StackUpdateTarget::Version, Some(tag)) => {
            format!("{base}/repos/{REPOSITORY}/releases/tags/{tag}")
        }
        (StackUpdateTarget::Version, None) => {
            return Err(StackError::InvalidParam {
                field: "--version",
                reason: "version target requires a tag".to_owned(),
            });
        }
    };
    build_client()?
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .map_err(|source| StackError::GithubReleaseFetch {
            repo: REPOSITORY.to_owned(),
            source,
        })?
        .error_for_status()
        .map_err(|source| StackError::GithubReleaseFetch {
            repo: REPOSITORY.to_owned(),
            source,
        })?
        .json::<ReleaseResponse>()
        .map_err(|source| StackError::GithubReleaseFetch {
            repo: REPOSITORY.to_owned(),
            source,
        })
}

fn fetch_manifest(release: &ReleaseResponse) -> Result<StackReleaseManifest> {
    let asset = release
        .assets
        .iter()
        .find(|asset| asset.name == MANIFEST_ASSET)
        .ok_or_else(|| StackError::GithubReleaseAssetNotFound {
            repo: REPOSITORY.to_owned(),
            pattern: MANIFEST_ASSET.to_owned(),
        })?;
    let body = download_bytes(&asset.browser_download_url)?;
    verify_manifest_sha256(release, &body)?;
    serde_json::from_slice(&body).map_err(|source| StackError::GithubReleaseArchiveExtract {
        repo: REPOSITORY.to_owned(),
        reason: format!("release manifest is not valid JSON: {source}"),
    })
}

fn validate_manifest(manifest: &StackReleaseManifest, release: &ReleaseResponse) -> Result<()> {
    if manifest.schema_version != 1 {
        return Err(StackError::GithubReleaseArchiveExtract {
            repo: REPOSITORY.to_owned(),
            reason: format!(
                "unsupported release manifest schema_version {}",
                manifest.schema_version
            ),
        });
    }
    if manifest.repository != REPOSITORY {
        return Err(StackError::GithubReleaseArchiveExtract {
            repo: REPOSITORY.to_owned(),
            reason: format!("manifest repository is `{}`", manifest.repository),
        });
    }
    if manifest.tag != release.tag_name {
        return Err(StackError::GithubReleaseArchiveExtract {
            repo: REPOSITORY.to_owned(),
            reason: format!(
                "manifest tag `{}` does not match release `{}`",
                manifest.tag, release.tag_name
            ),
        });
    }
    let version = parse_version(&manifest.version).ok_or_else(|| {
        StackError::GithubReleaseArchiveExtract {
            repo: REPOSITORY.to_owned(),
            reason: format!(
                "manifest version `{}` is not a valid release version",
                manifest.version
            ),
        }
    })?;
    let tag_version =
        parse_version(&manifest.tag).ok_or_else(|| StackError::GithubReleaseArchiveExtract {
            repo: REPOSITORY.to_owned(),
            reason: format!(
                "manifest tag `{}` does not contain a valid release version",
                manifest.tag
            ),
        })?;
    if version != tag_version {
        return Err(StackError::GithubReleaseArchiveExtract {
            repo: REPOSITORY.to_owned(),
            reason: format!(
                "manifest version `{}` does not match tag `{}`",
                manifest.version, manifest.tag
            ),
        });
    }
    Ok(())
}

fn manifest_artifact_for_host(manifest: &StackReleaseManifest) -> Result<&StackReleaseArtifact> {
    let target = host_target()?;
    manifest
        .artifacts
        .iter()
        .find(|artifact| artifact.target == target)
        .ok_or_else(|| StackError::GithubReleaseAssetNotFound {
            repo: REPOSITORY.to_owned(),
            pattern: format!("artifact target `{target}`"),
        })
}

fn host_target() -> Result<&'static str> {
    if std::env::consts::OS != "linux" {
        return Err(StackError::InvalidParam {
            field: "acps.update",
            reason: format!(
                "acp-stack release binaries are Linux-only; detected {}",
                std::env::consts::OS
            ),
        });
    }
    match std::env::consts::ARCH {
        "x86_64" => Ok("x86_64-unknown-linux-gnu"),
        "aarch64" => Ok("aarch64-unknown-linux-gnu"),
        other => Err(StackError::InvalidParam {
            field: "acps.update",
            reason: format!("unsupported host architecture `{other}`"),
        }),
    }
}

fn verify_artifact_sha256(asset: &str, bytes: &[u8], expected: &str) -> Result<()> {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let actual = format!("{:x}", hasher.finalize());
    if expected.eq_ignore_ascii_case(&actual) {
        return Ok(());
    }
    Err(StackError::GithubReleaseChecksumMismatch {
        repo: REPOSITORY.to_owned(),
        asset: asset.to_owned(),
        expected: expected.to_owned(),
        actual,
    })
}

fn verify_manifest_sha256(release: &ReleaseResponse, manifest_bytes: &[u8]) -> Result<()> {
    let asset = release
        .assets
        .iter()
        .find(|asset| asset.name == CHECKSUMS_ASSET)
        .ok_or_else(|| StackError::GithubReleaseAssetNotFound {
            repo: REPOSITORY.to_owned(),
            pattern: CHECKSUMS_ASSET.to_owned(),
        })?;
    let checksums = download_bytes(&asset.browser_download_url)?;
    let body = std::str::from_utf8(&checksums).map_err(|source| {
        StackError::GithubReleaseArchiveExtract {
            repo: REPOSITORY.to_owned(),
            reason: format!("{CHECKSUMS_ASSET} is not UTF-8: {source}"),
        }
    })?;
    let expected = body
        .lines()
        .find_map(|line| parse_checksum_line(line, MANIFEST_ASSET))
        .ok_or_else(|| StackError::GithubReleaseArchiveExtract {
            repo: REPOSITORY.to_owned(),
            reason: format!("{MANIFEST_ASSET} is not listed in {CHECKSUMS_ASSET}"),
        })?;
    verify_artifact_sha256(MANIFEST_ASSET, manifest_bytes, &expected)
}

fn parse_checksum_line(line: &str, asset_name: &str) -> Option<String> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let mut parts = line.split_whitespace();
    let digest = parts.next()?;
    let mut name = parts.next()?;
    if let Some(stripped) = name.strip_prefix('*') {
        name = stripped;
    }
    (name == asset_name).then(|| digest.to_owned())
}

fn install_archive(bytes: &[u8], binary_dir: &Path) -> Result<()> {
    let stage = tempfile::Builder::new()
        .prefix("acp-stack-update-")
        .tempdir_in(binary_dir)
        .map_err(|source| StackError::DirectoryCreate {
            path: binary_dir.to_path_buf(),
            source,
        })?;
    let decoder = flate2::read::GzDecoder::new(Cursor::new(bytes));
    let mut archive = tar::Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|source| StackError::GithubReleaseArchiveExtract {
            repo: REPOSITORY.to_owned(),
            reason: format!("failed to read release archive: {source}"),
        })?;
    let mut found = Vec::new();
    for entry in entries {
        let mut entry = entry.map_err(|source| StackError::GithubReleaseArchiveExtract {
            repo: REPOSITORY.to_owned(),
            reason: format!("failed to read archive entry: {source}"),
        })?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry
            .path()
            .map_err(|source| StackError::GithubReleaseArchiveExtract {
                repo: REPOSITORY.to_owned(),
                reason: format!("failed to read archive entry path: {source}"),
            })?
            .into_owned();
        let Some(leaf) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !BINARIES.contains(&leaf) || found.iter().any(|binary| binary == leaf) {
            continue;
        }
        let dest = stage.path().join(leaf);
        entry
            .unpack(&dest)
            .map_err(|source| StackError::GithubReleaseArchiveExtract {
                repo: REPOSITORY.to_owned(),
                reason: format!("failed to extract `{leaf}` from release archive: {source}"),
            })?;
        found.push(leaf.to_owned());
    }
    for binary in BINARIES {
        let staged = stage.path().join(binary);
        if !found.iter().any(|found| found.as_str() == *binary) || !staged.is_file() {
            return Err(StackError::GithubReleaseArchiveExtract {
                repo: REPOSITORY.to_owned(),
                reason: format!("release archive missing regular-file `{binary}`"),
            });
        }
        set_executable(&staged)?;
    }
    replace_binaries(stage.path(), binary_dir)?;
    Ok(())
}

fn replace_binaries(stage: &Path, binary_dir: &Path) -> Result<()> {
    let backups = tempfile::Builder::new()
        .prefix("acp-stack-update-backup-")
        .tempdir_in(binary_dir)
        .map_err(|source| StackError::DirectoryCreate {
            path: binary_dir.to_path_buf(),
            source,
        })?;
    let mut backed_up: Vec<(PathBuf, PathBuf)> = Vec::new();
    for binary in BINARIES {
        let dest = binary_dir.join(binary);
        let backup = backups.path().join(binary);
        if let Err(source) = fs::rename(&dest, &backup) {
            let rollback = rollback_binary_swap(&[], &backed_up);
            return Err(binary_swap_error(dest, source, rollback));
        }
        backed_up.push((dest, backup));
    }
    for binary in REMOVED_BINARIES {
        let dest = binary_dir.join(binary);
        let backup = backups.path().join(binary);
        match fs::symlink_metadata(&dest) {
            Ok(_) => {
                if let Err(source) = fs::rename(&dest, &backup) {
                    let rollback = rollback_binary_swap(&[], &backed_up);
                    return Err(binary_swap_error(dest, source, rollback));
                }
                backed_up.push((dest, backup));
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                let rollback = rollback_binary_swap(&[], &backed_up);
                return Err(binary_swap_error(dest, source, rollback));
            }
        }
    }

    let mut installed = Vec::new();
    for binary in BINARIES {
        let staged = stage.join(binary);
        let dest = binary_dir.join(binary);
        if let Err(source) = fs::rename(&staged, &dest) {
            let rollback = rollback_binary_swap(&installed, &backed_up);
            return Err(binary_swap_error(dest, source, rollback));
        }
        installed.push(dest);
    }
    Ok(())
}

fn rollback_binary_swap(installed: &[PathBuf], backed_up: &[(PathBuf, PathBuf)]) -> Vec<String> {
    let mut errors = Vec::new();
    for dest in installed.iter().rev() {
        if dest.exists()
            && let Err(err) = fs::remove_file(dest)
        {
            errors.push(format!("failed to remove {}: {err}", dest.display()));
        }
    }
    for (dest, backup) in backed_up.iter().rev() {
        if backup.exists()
            && let Err(err) = fs::rename(backup, dest)
        {
            errors.push(format!(
                "failed to restore {} from {}: {err}",
                dest.display(),
                backup.display()
            ));
        }
    }
    errors
}

fn binary_swap_error(
    path: PathBuf,
    source: std::io::Error,
    rollback_errors: Vec<String>,
) -> StackError {
    if rollback_errors.is_empty() {
        return StackError::ConfigWrite { path, source };
    }
    StackError::GithubReleaseArchiveExtract {
        repo: REPOSITORY.to_owned(),
        reason: format!(
            "failed to replace {}: {source}; rollback errors: {}",
            path.display(),
            rollback_errors.join("; ")
        ),
    }
}

fn install_binary_dir() -> Result<PathBuf> {
    // Test seam: redirect the install destination to a fixture directory so the
    // end-to-end updater test can swap binaries without touching the real
    // installed path. `fixture_path` returns `None` unless the crate is built
    // with the `test-fixtures` feature, so production always uses `current_exe`.
    if let Some(dir) = fixture_path(INSTALL_BINARY_DIR_ENV) {
        return Ok(dir);
    }
    let exe = std::env::current_exe().map_err(|source| StackError::ConfigRead {
        path: PathBuf::from("current_exe"),
        source,
    })?;
    exe.parent()
        .map(Path::to_path_buf)
        .ok_or(StackError::MissingParentDir { path: exe })
}

fn directory_is_writable(path: &Path) -> bool {
    let probe = path.join(format!(".acps-update-write-test-{}", std::process::id()));
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = fs::remove_file(probe);
            true
        }
        Err(_) => false,
    }
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)
        .map_err(|source| StackError::ConfigRead {
            path: path.to_path_buf(),
            source,
        })?
        .permissions();
    perms.set_mode(perms.mode() | 0o755);
    fs::set_permissions(path, perms).map_err(|source| StackError::PermissionSet {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

fn running_in_container() -> bool {
    let railway = [
        "RAILWAY_PROJECT_ID",
        "RAILWAY_ENVIRONMENT_ID",
        "RAILWAY_SERVICE_ID",
    ]
    .iter()
    .all(|name| std::env::var_os(name).is_some());
    railway || Path::new("/.dockerenv").exists()
}

// A release version is the strict semver core from Cargo.toml plus an
// optional nightly component that exists only in tags and packaging names
// (v0.1.1.2). A nightly orders after its base release (0.1.1 < 0.1.1.1) and
// before the next patch release (0.1.1.9 < 0.1.2); the derived Ord gives
// exactly this because Option orders None before Some.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct ReleaseVersion {
    major: u64,
    minor: u64,
    patch: u64,
    nightly: Option<u64>,
}

fn is_major_upgrade(current: &str, target: &str) -> bool {
    let Some(current) = parse_version(current) else {
        return false;
    };
    let Some(target) = parse_version(target) else {
        return false;
    };
    target.major > current.major
}

fn is_version_downgrade(current: &str, target: &str) -> bool {
    let Some(current) = parse_version(current) else {
        return false;
    };
    let Some(target) = parse_version(target) else {
        return false;
    };
    target < current
}

fn parse_version(value: &str) -> Option<ReleaseVersion> {
    let mut parts = normalize_version(value).split('.');
    let major = parse_version_component(parts.next()?)?;
    let minor = parse_version_component(parts.next()?)?;
    let patch = parse_version_component(parts.next()?)?;
    let nightly = match parts.next() {
        Some(part) => Some(parse_version_component(part)?),
        None => None,
    };
    if parts.next().is_some() {
        return None;
    }
    Some(ReleaseVersion {
        major,
        minor,
        patch,
        nightly,
    })
}

fn parse_version_component(part: &str) -> Option<u64> {
    // Leading zeros are rejected for parity with the producer-side tag
    // regexes: a non-canonical component must never alias a canonical one.
    if part.is_empty()
        || !part.bytes().all(|byte| byte.is_ascii_digit())
        || (part.len() > 1 && part.starts_with('0'))
    {
        return None;
    }
    part.parse().ok()
}

fn normalize_version(value: &str) -> &str {
    value
        .trim()
        .strip_prefix('v')
        .unwrap_or_else(|| value.trim())
}

fn build_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .user_agent(USER_AGENT)
        .build()
        .map_err(|source| StackError::GithubReleaseFetch {
            repo: REPOSITORY.to_owned(),
            source,
        })
}

fn download_bytes(url: &str) -> Result<Vec<u8>> {
    let response = build_client()?
        .get(url)
        .header("Accept", "application/octet-stream")
        .send()
        .map_err(|source| StackError::GithubReleaseFetch {
            repo: REPOSITORY.to_owned(),
            source,
        })?
        .error_for_status()
        .map_err(|source| StackError::GithubReleaseFetch {
            repo: REPOSITORY.to_owned(),
            source,
        })?;
    Ok(response
        .bytes()
        .map_err(|source| StackError::GithubReleaseFetch {
            repo: REPOSITORY.to_owned(),
            source,
        })?
        .to_vec())
}

fn github_api_base() -> String {
    if let Some(value) = fixture_string(GITHUB_API_BASE_ENV) {
        return value.trim_end_matches('/').to_owned();
    }
    GITHUB_API_BASE.to_owned()
}

fn policy_as_str(policy: StackUpdatePolicy) -> &'static str {
    match policy {
        StackUpdatePolicy::Compatible => "compatible",
        StackUpdatePolicy::SecurityCritical => "security-critical",
        StackUpdatePolicy::Manual => "manual",
    }
}

#[cfg(test)]
mod tests;

// End-to-end self-update apply test. Stands up a local HTTP fixture standing in
// for the GitHub Releases API and drives `install_stack_update` through the full
// fetch -> verify -> extract -> swap path, asserting the binaries on disk are
// actually replaced. Gated to `test-fixtures` because the `GITHUB_API_BASE` /
// install-dir redirection seams (and thus the binary swap) only activate under
// that feature; the test body itself skips on non-Linux hosts since
// `host_target` rejects them.
#[cfg(all(test, feature = "test-fixtures"))]
mod apply_e2e_tests;
