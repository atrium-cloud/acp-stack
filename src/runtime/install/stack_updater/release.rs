//! Release discovery and integrity verification for the self-updater: the manifest
//! must agree with its tag before any bytes reach the binary-swap path.

use super::*;

pub(super) fn fetch_release(options: &StackUpdateOptions) -> Result<ReleaseResponse> {
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
    build_client(REPOSITORY)?
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

pub(super) fn fetch_manifest(release: &ReleaseResponse) -> Result<StackReleaseManifest> {
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

pub(super) fn validate_manifest(
    manifest: &StackReleaseManifest,
    release: &ReleaseResponse,
) -> Result<()> {
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

pub(super) fn manifest_artifact_for_host(
    manifest: &StackReleaseManifest,
) -> Result<&StackReleaseArtifact> {
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

pub(super) fn host_target() -> Result<&'static str> {
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

pub(super) fn verify_artifact_sha256(asset: &str, bytes: &[u8], expected: &str) -> Result<()> {
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

pub(super) fn verify_manifest_sha256(
    release: &ReleaseResponse,
    manifest_bytes: &[u8],
) -> Result<()> {
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

pub(super) fn parse_checksum_line(line: &str, asset_name: &str) -> Option<String> {
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

pub(super) fn download_bytes(url: &str) -> Result<Vec<u8>> {
    let response = build_client(REPOSITORY)?
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

pub(super) fn github_api_base() -> String {
    if let Some(value) = fixture_string(GITHUB_API_BASE_ENV) {
        return value.trim_end_matches('/').to_owned();
    }
    GITHUB_API_BASE.to_owned()
}
