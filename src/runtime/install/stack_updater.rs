use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::parse_duration_string;
use crate::config::{Config, StackUpdatePolicy};
use crate::dev_gates::{GITHUB_API_BASE_ENV, INSTALL_BINARY_DIR_ENV, fixture_path, fixture_string};
use crate::error::{Result, StackError};
use crate::runtime::install::github_release::build_client;
use crate::state::{
    EVENT_SOURCE_CLI, NewStackUpdateRun, STACK_UPDATE_OPERATION_CHECK,
    STACK_UPDATE_OPERATION_INSTALL, STACK_UPDATE_STATUS_FAILED, STACK_UPDATE_STATUS_SKIPPED,
    STACK_UPDATE_STATUS_SUCCEEDED, StateStore,
};

mod apply;
mod release;

use apply::{directory_is_writable, install_archive, install_binary_dir};
use release::{
    download_bytes, fetch_manifest, fetch_release, manifest_artifact_for_host, validate_manifest,
};

const GITHUB_API_BASE: &str = "https://api.github.com";
const REPOSITORY: &str = "atrium-cloud/acp-stack";
const MANIFEST_ASSET: &str = "acps-release.json";
const CHECKSUMS_ASSET: &str = "SHA256SUMS";
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
    release::verify_artifact_sha256(&artifact.archive, &archive, &artifact.sha256)?;
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
