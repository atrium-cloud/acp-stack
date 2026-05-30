//! Agent install/registry/release-asset error helpers.
//!
//! Covers the install-time half of the `agent.*` namespace plus `init.*` (the
//! init-run state which exists to coordinate installation).

use http::StatusCode;

use super::StackError;

pub(super) fn error_code(err: &StackError) -> Option<&'static str> {
    use StackError::*;
    Some(match err {
        AgentConfigProvision { .. } => "agent.config_provision_failed",
        AgentNotConfigured => "agent.not_configured",
        AgentInstallerFailed { .. } => "agent.installer_failed",
        AgentInstallerCreatesMissing { .. } => "agent.installer_creates_missing",
        AgentInstallerTimeout => "agent.installer_timeout",
        AgentInstallerLogPersist { .. } => "agent.installer_log_persist_failed",
        AgentRegistryMissing { .. } => "agent.registry_missing",
        AgentPlaceholderConfigured => "agent.placeholder_configured",
        InitRunCorrupted { .. } => "init.run_corrupted",
        AgentUnsupported { .. } => "agent.unsupported",
        AgentCheckStale => "agent.check_stale",
        RegistryLoad { .. } => "agent.registry_load_failed",
        SkillInstallInvalidSource { .. } => "agent.skill_install_invalid_source",
        SkillInstallSourceMissing { .. } => "agent.skill_install_source_missing",
        SkillInstallInvalidName { .. } => "agent.skill_install_invalid_name",
        SkillInstallSkillMissing { .. } => "agent.skill_install_missing_skill",
        SkillInstallTargetConflict { .. } => "agent.skill_install_target_conflict",
        SkillInstallFailed { .. } => "agent.skill_install_failed",
        GithubReleaseFetch { .. } => "agent.github_release_fetch_failed",
        NpmRegistryFetch { .. } => "agent.npm_registry_fetch_failed",
        NpmRegistryEmptyVersion { .. } => "agent.npm_registry_empty_version",
        GithubReleaseAssetNotFound { .. } => "agent.github_release_asset_not_found",
        GithubReleaseAssetAmbiguous { .. } => "agent.github_release_asset_ambiguous",
        GithubReleaseArchiveExtract { .. } => "agent.github_release_archive_extract_failed",
        GithubReleaseChecksumMismatch { .. } => "agent.github_release_checksum_mismatch",
        UnsupportedHostArch { .. } => "agent.unsupported_host_arch",
        AgentSha256Mismatch { .. } => "agent.sha256_mismatch",
        _ => return None,
    })
}

pub(super) fn public_message(err: &StackError) -> Option<String> {
    use StackError::*;
    Some(match err {
        AgentConfigProvision { .. } => "failed to provision agent config".to_owned(),
        AgentNotConfigured => {
            "agent is not configured; declare [agent].id matching a registry entry, or provide an [agent.install] shell recipe"
                .to_owned()
        }
        AgentInstallerFailed { exit, .. } => match exit {
            Some(code) => format!("agent installer exited with status {code}"),
            None => "agent installer terminated without an exit status".to_owned(),
        },
        AgentInstallerCreatesMissing { name } => {
            format!("agent installer ran but `creates = {name}` did not resolve afterwards")
        }
        AgentInstallerTimeout => "agent installer hit the configured timeout".to_owned(),
        AgentInstallerLogPersist { path, .. } => {
            format!("failed to persist installer log at {}", path.display())
        }
        AgentRegistryMissing { id } => format!("ACP registry does not contain agent `{id}`"),
        AgentPlaceholderConfigured => {
            "config has legacy placeholder agent; select a real supported agent before starting the runtime".to_owned()
        }
        InitRunCorrupted { reason } => format!("init run state is corrupted: {reason}"),
        AgentUnsupported { name } => {
            format!("{name} is not currently supported. Please try a different agent.")
        }
        AgentCheckStale => {
            "one or more managed agent components are stale or missing; re-run `acps agent install` to upgrade".to_owned()
        }
        RegistryLoad { reason } => format!("agent registry could not be loaded: {reason}"),
        SkillInstallInvalidSource { source_id } => format!("invalid skill source `{source_id}`"),
        SkillInstallSourceMissing { source_id } => {
            format!("skill source `{source_id}` is not available")
        }
        SkillInstallInvalidName { name } => format!("invalid skill name `{name}`"),
        SkillInstallSkillMissing { source_id, skill } => {
            format!("skill `{skill}` was not found in source `{source_id}`")
        }
        SkillInstallTargetConflict { path, reason } => {
            format!("skill install target conflict at {}: {reason}", path.display())
        }
        SkillInstallFailed { reason } => format!("skill install failed: {reason}"),
        GithubReleaseFetch { repo, .. } => format!("failed to query GitHub Releases for {repo}"),
        NpmRegistryFetch { package, .. } => {
            format!("failed to query npm registry for `{package}`")
        }
        NpmRegistryEmptyVersion { package } => {
            format!("npm registry returned an empty version for `{package}`")
        }
        GithubReleaseAssetNotFound { repo, pattern } => {
            format!("no release asset for {repo} matched pattern `{pattern}`")
        }
        GithubReleaseAssetAmbiguous {
            repo,
            pattern,
            matches,
        } => format!(
            "{matches} release assets for {repo} matched pattern `{pattern}`; expected exactly one"
        ),
        GithubReleaseArchiveExtract { repo, reason } => {
            format!("failed to extract release archive from {repo}: {reason}")
        }
        GithubReleaseChecksumMismatch {
            repo,
            asset,
            expected,
            actual,
        } => format!(
            "release asset `{asset}` from {repo} failed sha256 verification: expected {expected}, got {actual}"
        ),
        UnsupportedHostArch { arch } => {
            format!("unsupported host architecture `{arch}` for GitHub Release install")
        }
        AgentSha256Mismatch { expected, actual } => {
            format!("agent binary sha256 mismatch: expected {expected}, got {actual}")
        }
        _ => return None,
    })
}

pub(super) fn http_status(err: &StackError) -> Option<StatusCode> {
    use StackError::*;
    Some(match err {
        AgentConfigProvision { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        AgentNotConfigured => StatusCode::BAD_REQUEST,
        AgentPlaceholderConfigured => StatusCode::BAD_REQUEST,
        AgentUnsupported { .. } => StatusCode::BAD_REQUEST,
        AgentCheckStale => StatusCode::CONFLICT,
        SkillInstallInvalidSource { .. }
        | SkillInstallInvalidName { .. }
        | SkillInstallSkillMissing { .. } => StatusCode::BAD_REQUEST,
        SkillInstallTargetConflict { .. } => StatusCode::CONFLICT,
        AgentInstallerFailed { .. }
        | AgentInstallerCreatesMissing { .. }
        | AgentInstallerTimeout
        | AgentInstallerLogPersist { .. }
        | AgentRegistryMissing { .. }
        | InitRunCorrupted { .. }
        | RegistryLoad { .. }
        | SkillInstallSourceMissing { .. }
        | SkillInstallFailed { .. }
        | GithubReleaseFetch { .. }
        | NpmRegistryFetch { .. }
        | NpmRegistryEmptyVersion { .. }
        | GithubReleaseAssetNotFound { .. }
        | GithubReleaseAssetAmbiguous { .. }
        | GithubReleaseArchiveExtract { .. }
        | GithubReleaseChecksumMismatch { .. }
        | UnsupportedHostArch { .. }
        | AgentSha256Mismatch { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        _ => return None,
    })
}
