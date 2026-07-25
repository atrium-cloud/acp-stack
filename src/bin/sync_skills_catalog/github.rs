use std::path::Path;
use std::process::Command;
use std::time::Duration;

use acp_stack::runtime::workspace_sources::safe_download::{DownloadOpts, download_to_file};
use reqwest::blocking::Client;
use serde::Deserialize;

use crate::{
    CURL_JSON_ATTEMPTS, CURL_RETRY_COUNT, CURL_RETRY_DELAY_SECONDS, CURL_TIMEOUT_SECONDS,
    GITHUB_ARCHIVE_MAX_BYTES, REQUEST_TIMEOUT,
};

pub(crate) fn download_archive(
    url: &str,
    destination: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    // GitHub codeload has repeatedly stalled in the rustls-backed client used
    // by the development hook, while curl succeeds against the same endpoint.
    // Prefer the already-required curl path and retain the safe downloader as
    // a bounded fallback for environments without a working curl transport.
    let curl_error = match curl_archive(url, destination) {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };
    let options = DownloadOpts {
        max_bytes: GITHUB_ARCHIVE_MAX_BYTES,
        connect_timeout: Duration::from_secs(15),
        read_timeout: Duration::from_secs(60),
        ..DownloadOpts::default()
    };
    download_to_file(url, destination, &options)
        .map(|_| ())
        .map_err(|error| {
            format!("archive download failed with curl ({curl_error}) and reqwest ({error})").into()
        })
}

fn curl_archive(url: &str, destination: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let max_filesize = GITHUB_ARCHIVE_MAX_BYTES.to_string();
    let output = Command::new("curl")
        .args([
            "-fsSL",
            "--http1.1",
            "--max-time",
            CURL_TIMEOUT_SECONDS,
            "--max-filesize",
            max_filesize.as_str(),
            "--retry",
            CURL_RETRY_COUNT,
            "--retry-all-errors",
            "--retry-delay",
            CURL_RETRY_DELAY_SECONDS,
            "-H",
            "Accept-Encoding: identity",
            "-H",
            concat!(
                "User-Agent: acp-stack-skills-sync/",
                env!("CARGO_PKG_VERSION")
            ),
            "-o",
        ])
        .arg(destination)
        .arg(url)
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "archive download failed for `{url}` with curl status {}: {}",
            output
                .status
                .code()
                .map_or_else(|| "unknown".to_owned(), |code| code.to_string()),
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    // curl only enforces `--max-filesize` when the response advertises a
    // length; codeload streams chunked, so this on-disk check is the bound
    // that actually holds.
    let bytes = std::fs::metadata(destination)?.len();
    if bytes > GITHUB_ARCHIVE_MAX_BYTES {
        return Err(format!(
            "archive download for `{url}` exceeded {GITHUB_ARCHIVE_MAX_BYTES} bytes"
        )
        .into());
    }
    Ok(())
}

pub(crate) struct GithubClient {
    client: Client,
}

impl GithubClient {
    pub(crate) fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let client = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .user_agent(concat!("acp-stack-skills-sync/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self { client })
    }

    pub(crate) fn commit(
        &self,
        repo: &str,
        branch: &str,
    ) -> Result<GithubCommit, Box<dyn std::error::Error>> {
        self.github_json(&format!(
            "https://api.github.com/repos/{repo}/commits/{}",
            branch.trim_matches('/')
        ))
    }

    fn github_json<T: for<'de> Deserialize<'de>>(
        &self,
        url: &str,
    ) -> Result<T, Box<dyn std::error::Error>> {
        let reqwest_result = self
            .client
            .get(url)
            .header(reqwest::header::ACCEPT_ENCODING, "identity")
            .send()
            .and_then(|response| response.error_for_status())
            .and_then(|response| response.json());
        match reqwest_result {
            Ok(parsed) => Ok(parsed),
            Err(reqwest_error) => curl_json(url).map_err(|curl_error| {
                format!("request failed with reqwest ({reqwest_error}) and curl ({curl_error})")
                    .into()
            }),
        }
    }
}

fn curl_json<T: for<'de> Deserialize<'de>>(url: &str) -> Result<T, Box<dyn std::error::Error>> {
    let mut last_error = None;
    for _ in 0..CURL_JSON_ATTEMPTS {
        let output = Command::new("curl")
            .args([
                "-fsSL",
                "--http1.1",
                "--max-time",
                CURL_TIMEOUT_SECONDS,
                "--retry",
                CURL_RETRY_COUNT,
                "--retry-all-errors",
                "--retry-delay",
                CURL_RETRY_DELAY_SECONDS,
                "-H",
                "Accept-Encoding: identity",
                "-H",
                concat!(
                    "User-Agent: acp-stack-skills-sync/",
                    env!("CARGO_PKG_VERSION")
                ),
                url,
            ])
            .output()?;
        if !output.status.success() {
            last_error = Some(format!("curl exited with status {}", output.status));
            continue;
        }
        match serde_json::from_slice(&output.stdout) {
            Ok(parsed) => return Ok(parsed),
            Err(source) => last_error = Some(source.to_string()),
        }
    }
    Err(format!(
        "curl response was not valid JSON after {CURL_JSON_ATTEMPTS} attempts: {}",
        last_error.unwrap_or_else(|| "unknown error".to_owned())
    )
    .into())
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct GithubCommit {
    pub(crate) sha: String,
}
