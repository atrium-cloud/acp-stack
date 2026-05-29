//! GitHub Release-driven installer for agent harnesses and adapter binaries.
//!
//! Used by [`agent_installer`] when a registry entry's install spec is
//! `InstallSpec::GithubRelease`. Fetches release metadata via the public
//! GitHub API, matches the asset name against a glob pattern with `{arch}`
//! substituted from the runtime architecture, downloads + optionally
//! checksum-verifies + extracts, then drops the binary into `dest_dir` with
//! `chmod +x` so it can satisfy the existing `creates` postcheck.
//!
//! No shell is spawned for this install type; everything happens in-process.
//! That keeps the timeout/output-cap/process-group hardening from
//! `run_program_install` out of scope here — the failure modes for a stuck
//! HTTP request or a malformed archive are bounded by reqwest's timeout and
//! the in-process extraction APIs respectively.

use std::collections::HashMap;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::error::{Result, StackError};
use crate::runtime::install::agent_registry::ArchiveKind;

const GITHUB_API_BASE: &str = "https://api.github.com";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const USER_AGENT: &str = concat!("acp-stack/", env!("CARGO_PKG_VERSION"));

/// Debug-build-only base URL override for the GitHub Releases API. Tests bind a
/// local axum mock to `127.0.0.1:0` and set this var so the same
/// install machinery drives against the mock,
/// rather than reaching out to the live api.github.com (which would
/// rate-limit CI and is not deterministic). The empty/unset case falls
/// back to the upstream constant.
fn github_api_base() -> String {
    #[cfg(debug_assertions)]
    {
        if let Ok(value) = std::env::var("ACP_STACK_GITHUB_API_BASE")
            && !value.trim().is_empty()
        {
            return value.trim_end_matches('/').to_owned();
        }
    }
    GITHUB_API_BASE.to_owned()
}

/// True when the resolved base URL is the canonical `api.github.com`.
/// Used to decide whether to forward `GITHUB_TOKEN` on outgoing
/// requests — a redirected base (test mock or, in the worst case, a
/// misconfigured override) must never receive the operator's PAT.
fn base_is_upstream(base: &str) -> bool {
    base.trim_end_matches('/')
        .eq_ignore_ascii_case(GITHUB_API_BASE)
}

#[derive(Debug, Clone)]
pub struct GithubReleaseInstall<'a> {
    pub repo: &'a str,
    pub asset_pattern: &'a str,
    pub archive: ArchiveKind,
    pub archive_binary_name: Option<&'a str>,
    pub binary_name: &'a str,
    pub checksums_asset: Option<&'a str>,
}

/// Outcome of a single github_release install. The log is shaped as
/// newline-separated stdout-like lines so the surrounding installer
/// machinery can persist it directly into `installer_runs.stdout`.
#[derive(Debug, Clone)]
pub struct GithubReleaseOutcome {
    pub binary_path: PathBuf,
    pub log: String,
    pub asset_name: String,
    pub release_tag: String,
}

/// Resolve the tag of the latest published release for a GitHub repo. Used
/// by `acps agent check` to compare an installed adapter/harness against
/// upstream without downloading the asset. Returns the raw release tag (e.g.
/// `v0.11.1`) so callers can do a stringly compare against
/// `installer_runs.version`.
pub fn latest_release_tag(repo: &str) -> Result<String> {
    let client = build_client()?;
    // Resolve the base ONCE and reuse it. Reading the env twice (once
    // for the token decision, once inside `fetch_release` to build the
    // URL) would let a concurrent env mutation flip the security
    // decision between those reads, which would defeat the
    // "redirected base must never receive GITHUB_TOKEN" invariant.
    let base = github_api_base();
    let token = if base_is_upstream(&base) {
        resolve_token()
    } else {
        // The base has been redirected (test mock, mis-set override).
        // Forwarding GITHUB_TOKEN to a non-upstream endpoint would
        // hand a PAT to an attacker-controlled host, so we drop it.
        None
    };
    let release = fetch_release(&client, &base, repo, None, token.as_deref())?;
    Ok(release.tag_name)
}

pub fn install(
    spec: GithubReleaseInstall<'_>,
    version: Option<&str>,
    dest_dir: &Path,
    _agent_env: &HashMap<String, String>,
) -> Result<GithubReleaseOutcome> {
    let mut log = LogBuf::new();
    let client = build_client()?;
    // Resolve the base ONCE and reuse it (see `latest_release_tag` —
    // the token decision and the URL target must read the same value).
    let base = github_api_base();
    let token = if base_is_upstream(&base) {
        resolve_token()
    } else {
        None
    };

    let release = fetch_release(&client, &base, spec.repo, version, token.as_deref())?;
    log.line(format!(
        "resolved {} release `{}`",
        spec.repo, release.tag_name
    ));

    let resolved_pattern = if spec.asset_pattern.contains("{arch}") {
        let arch = host_arch_token()?;
        spec.asset_pattern.replace("{arch}", arch)
    } else {
        spec.asset_pattern.to_owned()
    };
    let asset = pick_asset(&release.assets, &resolved_pattern, spec.repo)?;
    log.line(format!(
        "matched asset `{}` ({} bytes)",
        asset.name, asset.size
    ));

    let asset_bytes = download_bytes(
        &client,
        &asset.browser_download_url,
        token.as_deref(),
        spec.repo,
    )?;
    log.line(format!("downloaded {} bytes", asset_bytes.len()));

    if let Some(checksums_name) = spec.checksums_asset {
        let checksums_asset = release
            .assets
            .iter()
            .find(|a| a.name == checksums_name)
            .ok_or_else(|| StackError::GithubReleaseAssetNotFound {
                repo: spec.repo.to_owned(),
                pattern: checksums_name.to_owned(),
            })?;
        let checksums_bytes = download_bytes(
            &client,
            &checksums_asset.browser_download_url,
            token.as_deref(),
            spec.repo,
        )?;
        verify_checksum(spec.repo, &asset.name, &asset_bytes, &checksums_bytes)?;
        log.line(format!("verified sha256 against `{checksums_name}`"));
    }

    fs::create_dir_all(dest_dir).map_err(|source| StackError::GithubReleaseArchiveExtract {
        repo: spec.repo.to_owned(),
        reason: format!(
            "failed to create destination dir {}: {source}",
            dest_dir.display()
        ),
    })?;
    let binary_path = dest_dir.join(spec.binary_name);
    let archive_binary_name = spec.archive_binary_name.unwrap_or(spec.binary_name);

    match spec.archive {
        ArchiveKind::None => {
            write_binary(&binary_path, &asset_bytes, spec.repo)?;
            log.line(format!("wrote raw binary to {}", binary_path.display()));
        }
        ArchiveKind::TarGz => {
            extract_tar_gz(
                &asset_bytes,
                dest_dir,
                archive_binary_name,
                spec.binary_name,
                spec.repo,
            )?;
            log.line(format!(
                "extracted tar.gz; binary at {}",
                binary_path.display()
            ));
        }
        ArchiveKind::Zip => {
            extract_zip(
                &asset_bytes,
                dest_dir,
                archive_binary_name,
                spec.binary_name,
                spec.repo,
            )?;
            log.line(format!(
                "extracted zip; binary at {}",
                binary_path.display()
            ));
        }
    }

    set_executable(&binary_path, spec.repo)?;

    Ok(GithubReleaseOutcome {
        binary_path,
        log: log.into_string(),
        asset_name: asset.name,
        release_tag: release.tag_name,
    })
}

fn build_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .user_agent(USER_AGENT)
        .build()
        .map_err(|source| StackError::GithubReleaseFetch {
            repo: String::new(),
            source,
        })
}

fn resolve_token() -> Option<String> {
    std::env::var("GITHUB_TOKEN")
        .ok()
        .filter(|v| !v.trim().is_empty())
}

fn fetch_release(
    client: &reqwest::blocking::Client,
    base: &str,
    repo: &str,
    version: Option<&str>,
    token: Option<&str>,
) -> Result<ReleaseResponse> {
    let url = match version {
        Some(tag) => format!("{base}/repos/{repo}/releases/tags/{tag}"),
        None => format!("{base}/repos/{repo}/releases/latest"),
    };
    let mut request = client
        .get(&url)
        .header("Accept", "application/vnd.github+json");
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let response = request
        .send()
        .map_err(|source| StackError::GithubReleaseFetch {
            repo: repo.to_owned(),
            source,
        })?;
    let response =
        response
            .error_for_status()
            .map_err(|source| StackError::GithubReleaseFetch {
                repo: repo.to_owned(),
                source,
            })?;
    response
        .json::<ReleaseResponse>()
        .map_err(|source| StackError::GithubReleaseFetch {
            repo: repo.to_owned(),
            source,
        })
}

fn download_bytes(
    client: &reqwest::blocking::Client,
    url: &str,
    token: Option<&str>,
    repo: &str,
) -> Result<Vec<u8>> {
    let mut request = client.get(url).header("Accept", "application/octet-stream");
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let response = request
        .send()
        .map_err(|source| StackError::GithubReleaseFetch {
            repo: repo.to_owned(),
            source,
        })?;
    let response =
        response
            .error_for_status()
            .map_err(|source| StackError::GithubReleaseFetch {
                repo: repo.to_owned(),
                source,
            })?;
    Ok(response
        .bytes()
        .map_err(|source| StackError::GithubReleaseFetch {
            repo: repo.to_owned(),
            source,
        })?
        .to_vec())
}

fn pick_asset(assets: &[ReleaseAsset], pattern: &str, repo: &str) -> Result<ReleaseAsset> {
    let matches: Vec<&ReleaseAsset> = assets
        .iter()
        .filter(|asset| glob_match(pattern, &asset.name))
        .collect();
    match matches.len() {
        0 => Err(StackError::GithubReleaseAssetNotFound {
            repo: repo.to_owned(),
            pattern: pattern.to_owned(),
        }),
        1 => Ok(matches[0].clone()),
        n => Err(StackError::GithubReleaseAssetAmbiguous {
            repo: repo.to_owned(),
            pattern: pattern.to_owned(),
            matches: n,
        }),
    }
}

/// Map `std::env::consts::ARCH` to the GitHub-Release naming token. We only
/// support headless Linux deployment targets; anything else fails fast rather
/// than silently downloading a wrong-arch binary.
fn host_arch_token() -> Result<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Ok("x86_64"),
        "aarch64" => Ok("aarch64"),
        other => Err(StackError::UnsupportedHostArch {
            arch: leak_arch(other),
        }),
    }
}

/// `consts::ARCH` is already `&'static str`, but Rust can't see that through
/// the match. Re-anchor as the documented constant so the error variant
/// stays `&'static str`.
fn leak_arch(_arch: &str) -> &'static str {
    std::env::consts::ARCH
}

fn verify_checksum(
    repo: &str,
    asset_name: &str,
    asset_bytes: &[u8],
    checksums_bytes: &[u8],
) -> Result<()> {
    let mut hasher = Sha256::new();
    hasher.update(asset_bytes);
    let actual = format!("{:x}", hasher.finalize());

    let body = std::str::from_utf8(checksums_bytes).map_err(|err| {
        StackError::GithubReleaseArchiveExtract {
            repo: repo.to_owned(),
            reason: format!("checksums asset is not UTF-8: {err}"),
        }
    })?;

    // Standard `checksums.txt` shape: `<sha256>  <filename>` per line, with
    // optional `*` before filename for "binary mode". Tolerate both.
    let expected = body
        .lines()
        .find_map(|line| parse_checksum_line(line, asset_name))
        .ok_or_else(|| StackError::GithubReleaseArchiveExtract {
            repo: repo.to_owned(),
            reason: format!("asset `{asset_name}` not listed in checksums file"),
        })?;

    if !expected.eq_ignore_ascii_case(&actual) {
        return Err(StackError::GithubReleaseChecksumMismatch {
            repo: repo.to_owned(),
            asset: asset_name.to_owned(),
            expected,
            actual,
        });
    }
    Ok(())
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
    if name == asset_name {
        Some(digest.to_owned())
    } else {
        None
    }
}

fn write_binary(path: &Path, bytes: &[u8], repo: &str) -> Result<()> {
    fs::write(path, bytes).map_err(|source| StackError::GithubReleaseArchiveExtract {
        repo: repo.to_owned(),
        reason: format!("failed to write binary to {}: {source}", path.display()),
    })
}

fn extract_tar_gz(
    bytes: &[u8],
    dest_dir: &Path,
    archive_binary_name: &str,
    binary_name: &str,
    repo: &str,
) -> Result<()> {
    let decoder = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(decoder);
    archive.set_preserve_permissions(true);
    let entries = archive
        .entries()
        .map_err(|source| StackError::GithubReleaseArchiveExtract {
            repo: repo.to_owned(),
            reason: format!("failed to read tar entries: {source}"),
        })?;
    let mut found = false;
    for entry in entries {
        let mut entry = entry.map_err(|source| StackError::GithubReleaseArchiveExtract {
            repo: repo.to_owned(),
            reason: format!("tar entry read failed: {source}"),
        })?;
        let path = entry
            .path()
            .map_err(|source| StackError::GithubReleaseArchiveExtract {
                repo: repo.to_owned(),
                reason: format!("tar entry path read failed: {source}"),
            })?
            .into_owned();
        let leaf = match path.file_name() {
            Some(name) => name.to_owned(),
            None => continue,
        };
        if leaf == std::ffi::OsStr::new(archive_binary_name) {
            let dest = dest_dir.join(binary_name);
            entry
                .unpack(&dest)
                .map_err(|source| StackError::GithubReleaseArchiveExtract {
                    repo: repo.to_owned(),
                    reason: format!("failed to unpack `{archive_binary_name}` from tar: {source}"),
                })?;
            found = true;
            break;
        }
    }
    if !found {
        return Err(StackError::GithubReleaseArchiveExtract {
            repo: repo.to_owned(),
            reason: format!("`{archive_binary_name}` not found in tar archive"),
        });
    }
    Ok(())
}

fn extract_zip(
    bytes: &[u8],
    dest_dir: &Path,
    archive_binary_name: &str,
    binary_name: &str,
    repo: &str,
) -> Result<()> {
    let cursor = Cursor::new(bytes);
    let mut archive =
        zip::ZipArchive::new(cursor).map_err(|source| StackError::GithubReleaseArchiveExtract {
            repo: repo.to_owned(),
            reason: format!("failed to open zip: {source}"),
        })?;
    for i in 0..archive.len() {
        let mut entry =
            archive
                .by_index(i)
                .map_err(|source| StackError::GithubReleaseArchiveExtract {
                    repo: repo.to_owned(),
                    reason: format!("zip entry read failed: {source}"),
                })?;
        let entry_name = match entry.enclosed_name() {
            Some(p) => p.to_path_buf(),
            None => continue,
        };
        let leaf = match entry_name.file_name() {
            Some(n) => n.to_owned(),
            None => continue,
        };
        if leaf == std::ffi::OsStr::new(archive_binary_name) {
            let dest = dest_dir.join(binary_name);
            let mut out = fs::File::create(&dest).map_err(|source| {
                StackError::GithubReleaseArchiveExtract {
                    repo: repo.to_owned(),
                    reason: format!("failed to create destination {}: {source}", dest.display()),
                }
            })?;
            std::io::copy(&mut entry, &mut out).map_err(|source| {
                StackError::GithubReleaseArchiveExtract {
                    repo: repo.to_owned(),
                    reason: format!("failed to extract `{archive_binary_name}` from zip: {source}"),
                }
            })?;
            return Ok(());
        }
    }
    Err(StackError::GithubReleaseArchiveExtract {
        repo: repo.to_owned(),
        reason: format!("`{archive_binary_name}` not found in zip archive"),
    })
}

#[cfg(unix)]
fn set_executable(path: &Path, repo: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)
        .map_err(|source| StackError::GithubReleaseArchiveExtract {
            repo: repo.to_owned(),
            reason: format!("failed to stat {}: {source}", path.display()),
        })?
        .permissions();
    let mode = perms.mode() | 0o111;
    perms.set_mode(mode);
    fs::set_permissions(path, perms).map_err(|source| StackError::GithubReleaseArchiveExtract {
        repo: repo.to_owned(),
        reason: format!(
            "failed to set executable bit on {}: {source}",
            path.display()
        ),
    })
}

#[cfg(not(unix))]
fn set_executable(_path: &Path, _repo: &str) -> Result<()> {
    Ok(())
}

/// Simple glob: `*` matches any (possibly empty) byte sequence; everything
/// else is a literal byte match. Sufficient for asset_pattern matching, which
/// only ever needs `*` (no character classes or `?`). Operates on bytes so a
/// pattern containing `*` works regardless of UTF-8 boundaries in the input.
fn glob_match(pattern: &str, target: &str) -> bool {
    let pattern = pattern.as_bytes();
    let target = target.as_bytes();
    glob_match_inner(pattern, target)
}

fn glob_match_inner(pattern: &[u8], target: &[u8]) -> bool {
    let mut p_idx = 0;
    let mut t_idx = 0;
    let mut star_p: Option<usize> = None;
    let mut star_t: usize = 0;
    while t_idx < target.len() {
        if p_idx < pattern.len() && pattern[p_idx] == b'*' {
            star_p = Some(p_idx);
            star_t = t_idx;
            p_idx += 1;
        } else if p_idx < pattern.len() && pattern[p_idx] == target[t_idx] {
            p_idx += 1;
            t_idx += 1;
        } else if let Some(sp) = star_p {
            p_idx = sp + 1;
            star_t += 1;
            t_idx = star_t;
        } else {
            return false;
        }
    }
    while p_idx < pattern.len() && pattern[p_idx] == b'*' {
        p_idx += 1;
    }
    p_idx == pattern.len()
}

#[derive(Debug, Deserialize)]
struct ReleaseResponse {
    tag_name: String,
    assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Clone, Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
    size: u64,
}

#[derive(Debug, Default)]
struct LogBuf {
    lines: Vec<String>,
}

impl LogBuf {
    fn new() -> Self {
        Self::default()
    }
    fn line<S: Into<String>>(&mut self, line: S) {
        self.lines.push(line.into());
    }
    fn into_string(self) -> String {
        self.lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_match_literal() {
        assert!(glob_match(
            "codex-x86_64-linux.tar.gz",
            "codex-x86_64-linux.tar.gz"
        ));
        assert!(!glob_match(
            "codex-x86_64-linux.tar.gz",
            "codex-aarch64-linux.tar.gz"
        ));
    }

    #[test]
    fn glob_match_with_star() {
        assert!(glob_match("codex-*-linux*", "codex-x86_64-linux.tar.gz"));
        assert!(glob_match(
            "codex-*-linux*",
            "codex-aarch64-unknown-linux-musl.tar.gz"
        ));
        assert!(!glob_match("codex-*-linux*", "codex-x86_64-darwin.tar.gz"));
    }

    #[test]
    fn glob_match_trailing_star_matches_empty() {
        assert!(glob_match("codex*", "codex"));
        assert!(glob_match("codex*", "codex-x86_64"));
    }

    #[test]
    fn glob_match_leading_star() {
        assert!(glob_match("*linux*", "codex-x86_64-linux.tar.gz"));
        assert!(!glob_match("*darwin*", "codex-x86_64-linux.tar.gz"));
    }

    #[test]
    fn parse_checksum_line_handles_binary_mode_marker() {
        let parsed = parse_checksum_line("abc123  *codex.tar.gz", "codex.tar.gz");
        assert_eq!(parsed.as_deref(), Some("abc123"));
    }

    #[test]
    fn parse_checksum_line_handles_plain_marker() {
        let parsed = parse_checksum_line("abc123  codex.tar.gz", "codex.tar.gz");
        assert_eq!(parsed.as_deref(), Some("abc123"));
    }

    #[test]
    fn parse_checksum_line_ignores_unrelated_entries() {
        assert!(parse_checksum_line("abc123  other.tar.gz", "codex.tar.gz").is_none());
    }

    #[test]
    fn parse_checksum_line_ignores_comments_and_blanks() {
        assert!(parse_checksum_line("# header", "codex").is_none());
        assert!(parse_checksum_line("   ", "codex").is_none());
    }

    #[test]
    fn pick_asset_ambiguous_match_is_reported() {
        let assets = vec![
            ReleaseAsset {
                name: "codex-x86_64-linux.tar.gz".into(),
                browser_download_url: "https://example.invalid/a".into(),
                size: 1,
            },
            ReleaseAsset {
                name: "codex-x86_64-linux.zip".into(),
                browser_download_url: "https://example.invalid/b".into(),
                size: 1,
            },
        ];
        let err =
            pick_asset(&assets, "codex-x86_64-linux*", "openai/codex").expect_err("ambiguous");
        match err {
            StackError::GithubReleaseAssetAmbiguous { matches, .. } => assert_eq!(matches, 2),
            other => panic!("expected ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn pick_asset_no_match_is_reported() {
        let assets = vec![ReleaseAsset {
            name: "codex-x86_64-linux.tar.gz".into(),
            browser_download_url: "https://example.invalid/a".into(),
            size: 1,
        }];
        let err =
            pick_asset(&assets, "codex-aarch64-linux*", "openai/codex").expect_err("no match");
        assert!(matches!(err, StackError::GithubReleaseAssetNotFound { .. }));
    }

    #[test]
    fn verify_checksum_accepts_matching_digest() {
        let asset_bytes = b"hello";
        let mut hasher = Sha256::new();
        hasher.update(asset_bytes);
        let digest = format!("{:x}", hasher.finalize());
        let checksums = format!("{digest}  asset.bin\n");
        verify_checksum("repo", "asset.bin", asset_bytes, checksums.as_bytes()).expect("ok");
    }

    #[test]
    fn verify_checksum_rejects_mismatch() {
        let asset_bytes = b"hello";
        let checksums =
            "0000000000000000000000000000000000000000000000000000000000000000  asset.bin";
        let err = verify_checksum("repo", "asset.bin", asset_bytes, checksums.as_bytes())
            .expect_err("mismatch");
        assert!(matches!(
            err,
            StackError::GithubReleaseChecksumMismatch { .. }
        ));
    }

    #[test]
    fn extract_tar_gz_can_rename_platform_binary() {
        let mut tar_bytes = Vec::new();
        {
            let encoder =
                flate2::write::GzEncoder::new(&mut tar_bytes, flate2::Compression::default());
            let mut builder = tar::Builder::new(encoder);
            let payload = b"#!/bin/sh\n";
            let mut header = tar::Header::new_gnu();
            header.set_size(payload.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, "codex-x86_64-unknown-linux-musl", &payload[..])
                .expect("tar entry should append");
            let encoder = builder.into_inner().expect("tar should finish");
            encoder.finish().expect("gzip should finish");
        }

        let tempdir = tempfile::tempdir().expect("tempdir");
        extract_tar_gz(
            &tar_bytes,
            tempdir.path(),
            "codex-x86_64-unknown-linux-musl",
            "codex",
            "openai/codex",
        )
        .expect("platform binary should extract");

        assert_eq!(
            std::fs::read(tempdir.path().join("codex")).expect("codex binary should exist"),
            b"#!/bin/sh\n"
        );
    }
}
