use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::Utc;
use semver::Version;
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
const BINARIES: &[&str] = &["acps", "acpctl"];

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
    let recent = state
        .query_stack_update_runs(20)?
        .into_iter()
        .find(|run| run.operation == STACK_UPDATE_OPERATION_INSTALL && run.auto);
    let Some(recent) = recent else {
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
                "manifest version `{}` is not valid semver",
                manifest.version
            ),
        }
    })?;
    let tag_version =
        parse_version(&manifest.tag).ok_or_else(|| StackError::GithubReleaseArchiveExtract {
            repo: REPOSITORY.to_owned(),
            reason: format!(
                "manifest tag `{}` does not contain valid semver",
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

fn is_major_upgrade(current: &str, target: &str) -> bool {
    let Some(current) = parse_version(current) else {
        return false;
    };
    let Some(target) = parse_version(target) else {
        return false;
    };
    target.major > current.major
}

fn parse_version(value: &str) -> Option<Version> {
    Version::parse(normalize_version(value)).ok()
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
mod tests {
    use super::*;
    use crate::state::{
        LogFilter, NewStackUpdateRun, STACK_UPDATE_OPERATION_INSTALL, STACK_UPDATE_STATUS_SKIPPED,
        STACK_UPDATE_STATUS_SUCCEEDED, StateStore,
    };

    fn manifest(version: &str, classification: StackReleaseClassification) -> StackReleaseManifest {
        StackReleaseManifest {
            schema_version: 1,
            repository: REPOSITORY.to_owned(),
            tag: format!("v{version}"),
            version: version.to_owned(),
            classification,
            breaking: false,
            artifacts: Vec::new(),
        }
    }

    fn test_config() -> Config {
        crate::config::load_config_from_str(
            r#"
[api]
bind = "127.0.0.1:7700"
public_url = "http://127.0.0.1:7700"
max_request_bytes = 1048576

[security.http]
max_request_bytes = 1048576
rate_limit_per_minute = 120
burst = 30
auth_failures_per_minute = 5
auth_block_duration = "15m"
allowed_origins = []
trust_proxy_headers = false
trusted_proxies = []

[workspace]
root = "/workspace"
uploads = "/workspace/uploads"
default_shell = "/bin/bash"
runtime_user = "acp"
max_file_bytes = 8388608

[logging]
level = "info"
local_retention_days = 30

[agent]
id = "placebo"
name = "Placebo"
command = "placebo-agent"
args = []
cwd = "/workspace"
env = []
restart = "on-crash"
"#,
        )
        .expect("test config should parse")
    }

    fn test_store() -> (tempfile::TempDir, StateStore) {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let store = StateStore::open(tempdir.path().join("state.sqlite")).expect("state open");
        store.migrate().expect("migrate");
        (tempdir, store)
    }

    #[test]
    fn compatible_policy_installs_same_major_regular_release() {
        let release = manifest("0.2.0", StackReleaseClassification::Regular);
        let decision = update_decision(
            StackUpdatePolicy::Compatible,
            "0.1.0",
            &release,
            false,
            false,
            false,
        );
        assert_eq!(decision, StackUpdateDecision::Install);
    }

    #[test]
    fn security_policy_installs_only_security_critical_release() {
        let regular = manifest("0.1.1", StackReleaseClassification::Regular);
        let security = manifest("0.1.2", StackReleaseClassification::SecurityCritical);
        assert_eq!(
            update_decision(
                StackUpdatePolicy::SecurityCritical,
                "0.1.0",
                &regular,
                false,
                false,
                false,
            ),
            StackUpdateDecision::ManualOnly
        );
        assert_eq!(
            update_decision(
                StackUpdatePolicy::SecurityCritical,
                "0.1.0",
                &security,
                false,
                false,
                false,
            ),
            StackUpdateDecision::Install
        );
    }

    #[test]
    fn major_or_breaking_release_is_blocked_without_override() {
        let major = manifest("1.0.0", StackReleaseClassification::SecurityCritical);
        let mut breaking = manifest("0.1.1", StackReleaseClassification::SecurityCritical);
        breaking.breaking = true;
        assert_eq!(
            update_decision(
                StackUpdatePolicy::SecurityCritical,
                "0.1.0",
                &major,
                true,
                false,
                false,
            ),
            StackUpdateDecision::Blocked
        );
        assert_eq!(
            update_decision(
                StackUpdatePolicy::SecurityCritical,
                "0.1.0",
                &breaking,
                false,
                false,
                false,
            ),
            StackUpdateDecision::Blocked
        );
    }

    #[test]
    fn manual_policy_does_not_auto_select_release() {
        let release = manifest("0.1.1", StackReleaseClassification::SecurityCritical);
        assert_eq!(
            update_decision(
                StackUpdatePolicy::Manual,
                "0.1.0",
                &release,
                false,
                false,
                false,
            ),
            StackUpdateDecision::ManualOnly
        );
    }

    #[test]
    fn manual_policy_never_auto_installs_with_breaking_override() {
        let release = manifest("0.1.1", StackReleaseClassification::SecurityCritical);
        assert_eq!(
            update_decision(
                StackUpdatePolicy::Manual,
                "0.1.0",
                &release,
                false,
                true,
                true,
            ),
            StackUpdateDecision::ManualOnly
        );
    }

    #[test]
    fn manifest_version_must_match_tag_semver() {
        let release = ReleaseResponse {
            tag_name: "v0.1.1".to_owned(),
            prerelease: false,
            assets: Vec::new(),
        };
        let mut manifest = manifest("0.1.1", StackReleaseClassification::Regular);
        manifest.version = "9.9.9".to_owned();
        let err = validate_manifest(&manifest, &release).expect_err("mismatch should fail");
        assert!(err.to_string().contains("does not match tag"));

        manifest.version = "not-semver".to_owned();
        let err = validate_manifest(&manifest, &release).expect_err("invalid semver should fail");
        assert!(err.to_string().contains("not valid semver"));
    }

    #[test]
    fn major_upgrade_detection_normalizes_v_prefix() {
        assert!(is_major_upgrade("v0.9.0", "v1.0.0"));
        assert!(!is_major_upgrade("v1.2.0", "1.3.0"));
    }

    #[test]
    fn failed_update_attempt_writes_run_and_event() {
        let (_tempdir, store) = test_store();
        let result = Err(StackError::InvalidParam {
            field: "test",
            reason: "broken".to_owned(),
        });
        let err = persist_update_result(&store, STACK_UPDATE_OPERATION_CHECK, false, &result)
            .expect_err("failure should be returned after logging");
        assert!(err.to_string().contains("acp-stack update failed"));

        let runs = store.query_stack_update_runs(10).expect("runs");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].operation, STACK_UPDATE_OPERATION_CHECK);
        assert_eq!(runs[0].status, STACK_UPDATE_STATUS_FAILED);
        assert_eq!(
            runs[0].message.as_deref(),
            Some("query parameter `test` is invalid: broken")
        );

        let events = store
            .query_events(LogFilter {
                limit: 10,
                kind: Some("stack.update.failed"),
                ..LogFilter::default()
            })
            .expect("events");
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn auto_frequency_skip_writes_run_and_event_without_network() {
        let (_tempdir, store) = test_store();
        store
            .append_stack_update_run(NewStackUpdateRun {
                operation: STACK_UPDATE_OPERATION_INSTALL,
                status: STACK_UPDATE_STATUS_SUCCEEDED,
                current_version: "0.1.0",
                target_version: Some("0.1.0"),
                target_tag: Some("v0.1.0"),
                classification: Some("regular"),
                breaking: false,
                major_upgrade: false,
                policy: "security-critical",
                auto: true,
                message: Some("previous"),
                payload_json: "{}",
            })
            .expect("seed previous run");

        let report = install_stack_update(
            &test_config(),
            &store,
            StackUpdateOptions {
                target: StackUpdateTarget::Latest,
                version: None,
                allow_breaking: false,
                auto: true,
            },
        )
        .expect("frequency skip should not hit network");
        assert_eq!(report.status, StackUpdateStatus::Skipped);

        let runs = store.query_stack_update_runs(10).expect("runs");
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].status, STACK_UPDATE_STATUS_SKIPPED);
        assert!(runs[0].auto);
        let events = store
            .query_events(LogFilter {
                limit: 10,
                kind: Some("stack.update.skipped"),
                ..LogFilter::default()
            })
            .expect("events");
        assert_eq!(events.len(), 1);
    }

    fn make_archive(contents: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::default(),
        ));
        for (name, body) in contents {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, name, *body)
                .expect("append archive entry");
        }
        builder
            .into_inner()
            .expect("finish tar")
            .finish()
            .expect("finish gzip")
    }

    #[test]
    fn install_archive_swaps_existing_binaries() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Seed the destination with old binaries to prove they are replaced.
        for binary in BINARIES {
            fs::write(dir.path().join(binary), b"old").expect("seed old binary");
        }
        let archive = make_archive(&[("acps", b"new-acps"), ("acpctl", b"new-acpctl")]);

        install_archive(&archive, dir.path()).expect("install archive");

        assert_eq!(
            fs::read(dir.path().join("acps")).expect("read acps"),
            b"new-acps"
        );
        assert_eq!(
            fs::read(dir.path().join("acpctl")).expect("read acpctl"),
            b"new-acpctl"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for binary in BINARIES {
                let mode = fs::metadata(dir.path().join(binary))
                    .expect("metadata")
                    .permissions()
                    .mode();
                assert!(
                    mode & 0o111 != 0,
                    "{binary} should be executable after swap"
                );
            }
        }
    }

    #[test]
    fn install_archive_missing_binary_leaves_destination_intact() {
        let dir = tempfile::tempdir().expect("tempdir");
        for binary in BINARIES {
            fs::write(dir.path().join(binary), b"old").expect("seed old binary");
        }
        // `acpctl` is absent, so the extract step must fail before any swap.
        let archive = make_archive(&[("acps", b"new-acps")]);

        let err =
            install_archive(&archive, dir.path()).expect_err("missing binary should fail install");
        assert!(err.to_string().contains("acpctl"));

        // The pre-existing binaries are untouched because the swap never began.
        for binary in BINARIES {
            assert_eq!(
                fs::read(dir.path().join(binary)).expect("read seeded binary"),
                b"old"
            );
        }
    }
}

// End-to-end self-update apply test. Stands up a local HTTP fixture standing in
// for the GitHub Releases API and drives `install_stack_update` through the full
// fetch -> verify -> extract -> swap path, asserting the binaries on disk are
// actually replaced. Gated to `test-fixtures` because the `GITHUB_API_BASE` /
// install-dir redirection seams (and thus the binary swap) only activate under
// that feature; the test body itself skips on non-Linux hosts since
// `host_target` rejects them.
#[cfg(all(test, feature = "test-fixtures"))]
mod apply_e2e_tests {
    use super::*;
    use crate::config::load_config_from_str;
    use crate::state::{LogFilter, STACK_UPDATE_OPERATION_INSTALL, STACK_UPDATE_STATUS_SUCCEEDED};
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::thread;

    const TARGET_VERSION: &str = "0.1.2";
    const TARGET_TAG: &str = "v0.1.2";

    const CONFIG: &str = r#"
[api]
bind = "127.0.0.1:7700"
public_url = "http://127.0.0.1:7700"
max_request_bytes = 1048576

[security.http]
max_request_bytes = 1048576
rate_limit_per_minute = 120
burst = 30
auth_failures_per_minute = 5
auth_block_duration = "15m"
allowed_origins = []
trust_proxy_headers = false
trusted_proxies = []

[workspace]
root = "/workspace"
uploads = "/workspace/uploads"
default_shell = "/bin/bash"
runtime_user = "acp"
max_file_bytes = 8388608

[logging]
level = "info"
local_retention_days = 30

[agent]
id = "placebo"
name = "Placebo"
command = "placebo-agent"
args = []
cwd = "/workspace"
env = []
restart = "on-crash"

[updates.acp_stack]
policy = "compatible"
frequency = "1d"
"#;

    fn sha256_hex(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        format!("{:x}", hasher.finalize())
    }

    fn make_targz(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::default(),
        ));
        for (name, body) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, name, *body)
                .expect("append archive entry");
        }
        builder
            .into_inner()
            .expect("finish tar")
            .finish()
            .expect("finish gzip")
    }

    // A minimal blocking HTTP/1.1 fixture: one request per connection, routed by
    // path. Runs on a detached thread for the lifetime of the test process.
    fn serve(listener: TcpListener, routes: HashMap<String, Vec<u8>>) {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let reader_stream = stream.try_clone().expect("clone fixture stream");
            let mut reader = BufReader::new(reader_stream);
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).is_err() {
                continue;
            }
            // Drain request headers up to the blank line; bodies are never sent.
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) if line == "\r\n" || line == "\n" => break,
                    Ok(_) => continue,
                    Err(_) => break,
                }
            }
            let path = request_line
                .split_whitespace()
                .nth(1)
                .unwrap_or("/")
                .to_owned();
            let response = match routes.get(&path) {
                Some(body) => {
                    let mut out = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .into_bytes();
                    out.extend_from_slice(body);
                    out
                }
                None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_vec(),
            };
            // Best-effort: a client that hung up early is fine for a fixture.
            if let Err(error) = stream.write_all(&response) {
                eprintln!("fixture server write failed: {error}");
            }
        }
    }

    #[test]
    fn install_stack_update_downloads_and_swaps_binaries() {
        // Release binaries are Linux-only; `host_target` errors elsewhere, so
        // the swap path is only meaningful on Linux. Skip the body off-Linux
        // (the module still compiles everywhere so the test stays type-checked).
        if std::env::consts::OS != "linux" {
            eprintln!("skipping apply e2e test: stack release binaries are Linux-only");
            return;
        }
        // The updater refuses to install inside a container, which would defeat
        // the test. GitHub-hosted runners aren't containers, but guard anyway.
        if std::path::Path::new("/.dockerenv").exists() {
            eprintln!("skipping apply e2e test: running inside a container");
            return;
        }
        let target = match std::env::consts::ARCH {
            "x86_64" => "x86_64-unknown-linux-gnu",
            "aarch64" => "aarch64-unknown-linux-gnu",
            other => {
                eprintln!("skipping apply e2e test: unsupported arch {other}");
                return;
            }
        };
        // Mirror the real release artifact name (`acp-stack-<version>-<target>.tar.gz`,
        // built by scripts/build-release.sh) so the fixture exercises the
        // production naming contract rather than a stand-in.
        let tarball_name = format!("acp-stack-{TARGET_VERSION}-{target}.tar.gz");

        // Build the release artifacts the fixture serves.
        let tarball = make_targz(&[
            ("acps", b"new-acps-binary"),
            ("acpctl", b"new-acpctl-binary"),
        ]);
        let tar_sha = sha256_hex(&tarball);

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture");
        let port = listener.local_addr().expect("fixture addr").port();
        let base = format!("http://127.0.0.1:{port}");

        let manifest = serde_json::json!({
            "schema_version": 1,
            "repository": REPOSITORY,
            "tag": TARGET_TAG,
            "version": TARGET_VERSION,
            "classification": "regular",
            "breaking": false,
            "artifacts": [{ "target": target, "archive": tarball_name, "sha256": tar_sha }],
        });
        let manifest_bytes = serde_json::to_vec(&manifest).expect("serialize manifest");
        let manifest_sha = sha256_hex(&manifest_bytes);
        let checksums = format!("{manifest_sha}  {MANIFEST_ASSET}\n").into_bytes();

        let release = serde_json::json!({
            "tag_name": TARGET_TAG,
            "prerelease": false,
            "assets": [
                { "name": MANIFEST_ASSET, "browser_download_url": format!("{base}/dl/{MANIFEST_ASSET}") },
                { "name": CHECKSUMS_ASSET, "browser_download_url": format!("{base}/dl/{CHECKSUMS_ASSET}") },
                { "name": tarball_name, "browser_download_url": format!("{base}/dl/{tarball_name}") },
            ],
        });
        let release_bytes = serde_json::to_vec(&release).expect("serialize release");

        let mut routes: HashMap<String, Vec<u8>> = HashMap::new();
        routes.insert(
            format!("/repos/{REPOSITORY}/releases/latest"),
            release_bytes,
        );
        routes.insert(format!("/dl/{MANIFEST_ASSET}"), manifest_bytes);
        routes.insert(format!("/dl/{CHECKSUMS_ASSET}"), checksums);
        routes.insert(format!("/dl/{tarball_name}"), tarball);

        // Pre-seed the install dir: the swap renames the existing files aside,
        // so they must already exist.
        let workdir = tempfile::tempdir().expect("tempdir");
        let bin_dir = workdir.path().join("bin");
        std::fs::create_dir(&bin_dir).expect("create bin dir");
        for binary in BINARIES {
            std::fs::write(bin_dir.join(binary), b"old").expect("seed old binary");
        }

        // Activate the fixture seams. SAFETY: set before any other thread is
        // spawned in this test; removed before the test returns. No other test
        // reads these vars (only the install path does, which only this test
        // drives).
        unsafe {
            std::env::set_var(GITHUB_API_BASE_ENV, &base);
            std::env::set_var(
                INSTALL_BINARY_DIR_ENV,
                bin_dir.to_str().expect("utf-8 bin dir"),
            );
        }
        thread::spawn(move || serve(listener, routes));

        let config = load_config_from_str(CONFIG).expect("config parses");
        let store = StateStore::open(workdir.path().join("state.sqlite")).expect("state open");
        store.migrate().expect("migrate");

        let result = install_stack_update(
            &config,
            &store,
            StackUpdateOptions {
                target: StackUpdateTarget::Latest,
                version: None,
                allow_breaking: false,
                auto: false,
            },
        );

        // Clear the seams before asserting so a failed assert can't leak env.
        unsafe {
            std::env::remove_var(GITHUB_API_BASE_ENV);
            std::env::remove_var(INSTALL_BINARY_DIR_ENV);
        }

        let report = result.expect("install should succeed");
        assert_eq!(report.status, StackUpdateStatus::Installed);
        assert_eq!(report.target_version.as_deref(), Some(TARGET_VERSION));
        assert_eq!(report.target_tag.as_deref(), Some(TARGET_TAG));

        assert_eq!(
            std::fs::read(bin_dir.join("acps")).expect("read acps"),
            b"new-acps-binary"
        );
        assert_eq!(
            std::fs::read(bin_dir.join("acpctl")).expect("read acpctl"),
            b"new-acpctl-binary"
        );

        let runs = store.query_stack_update_runs(10).expect("runs");
        assert!(
            runs.iter()
                .any(|run| run.operation == STACK_UPDATE_OPERATION_INSTALL
                    && run.status == STACK_UPDATE_STATUS_SUCCEEDED),
            "expected a succeeded install run, got {runs:?}"
        );

        let events = store
            .query_events(LogFilter {
                limit: 10,
                kind: Some("stack.update.installed"),
                ..LogFilter::default()
            })
            .expect("events");
        assert_eq!(events.len(), 1);
    }
}
