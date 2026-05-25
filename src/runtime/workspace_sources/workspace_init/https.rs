//! Data lane: HTTPS download + archive extraction (with a sane file-copy
//! fallback for non-archive payloads). The sha256 pin in `expected_sha256`
//! is honored on every rerun; a diverging pin forces a re-fetch rather than
//! trusting a sentinel-skip.

use std::path::Path;

use crate::config::DataSourceConfig;
use crate::error::{Result, StackError};
use crate::runtime::workspace_sources::safe_download::{DownloadOpts, download_to_file};
use crate::runtime::workspace_sources::safe_extract::{ExtractOpts, extract_archive};

use super::common::{
    Sentinel, SentinelBody, capture_error, cleanup_partial_destination, ensure_dest_or_fail,
    ensure_destination_not_symlink, sentinel_if_present, write_operation_capture,
};
use super::{
    CAPTURE_TAG_COPY, CAPTURE_TAG_DOWNLOAD, CAPTURE_TAG_EXTRACT, MaterializeOutcome, SourceReport,
};

pub(super) fn materialize_https(
    index: usize,
    source: &DataSourceConfig,
    name: &str,
    dest: &Path,
    log_dir: Option<&Path>,
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
                log_dir: None,
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
    let report = match download_to_file(url, tmp.path(), &opts) {
        Ok(report) => report,
        Err(err) => {
            capture_error(log_dir, CAPTURE_TAG_DOWNLOAD, &err);
            return Err(err);
        }
    };
    write_operation_capture(
        log_dir,
        CAPTURE_TAG_DOWNLOAD,
        &format!(
            "url={url}\nfinal_url={}\nbytes={}\nsha256={}\ncontent_type={}\n",
            report.final_url,
            report.bytes_written,
            report.sha256,
            report.content_type.as_deref().unwrap_or(""),
        ),
        "",
    )?;
    let bytes = report.bytes_written;
    let sha256 = report.sha256.clone();

    let mut extract_opts = ExtractOpts::default();
    if let Some(limit) = source.max_extracted_bytes {
        extract_opts.max_total_bytes = limit;
    }
    let extracted = match extract_archive(tmp.path(), dest, &extract_opts) {
        Ok(report) => {
            write_operation_capture(
                log_dir,
                CAPTURE_TAG_EXTRACT,
                &format!(
                    "archive={}\ndestination={}\nentries={}\nbytes={}\ntop_level_dir={}\n",
                    tmp.path().display(),
                    dest.display(),
                    report.entries_written,
                    report.bytes_written,
                    report.top_level_dir.as_deref().unwrap_or(""),
                ),
                "",
            )
            .map_err(|err| cleanup_partial_destination(dest, err))?;
            true
        }
        Err(StackError::ArchiveUnsupportedFormat) => {
            write_operation_capture(
                log_dir,
                CAPTURE_TAG_EXTRACT,
                &format!(
                    "archive={}\ndestination={}\nunsupported_format=true\nfallback=copy\n",
                    tmp.path().display(),
                    dest.display(),
                ),
                "",
            )?;
            // Not an archive: place the file at <dest>/<basename> instead.
            let leaf = derive_leaf_from_url(url);
            let target = dest.join(leaf);
            let copied = match std::fs::copy(tmp.path(), &target) {
                Ok(bytes) => bytes,
                Err(source_err) => {
                    let err = StackError::WorkspaceMaterializeFailed {
                        reason: format!(
                            "copy downloaded body to `{}`: {source_err}",
                            target.display()
                        ),
                    };
                    capture_error(log_dir, CAPTURE_TAG_COPY, &err);
                    return Err(cleanup_partial_destination(dest, err));
                }
            };
            write_operation_capture(
                log_dir,
                CAPTURE_TAG_COPY,
                &format!(
                    "source={}\ndestination={}\nbytes={copied}\nentries=1\n",
                    tmp.path().display(),
                    target.display(),
                ),
                "",
            )
            .map_err(|err| cleanup_partial_destination(dest, err))?;
            false
        }
        Err(err) => {
            capture_error(log_dir, CAPTURE_TAG_EXTRACT, &err);
            return Err(cleanup_partial_destination(dest, err));
        }
    };

    let sentinel = Sentinel::new(SentinelBody::Https {
        url: url.to_owned(),
        sha256,
        bytes,
        extracted,
    });
    if let Err(err) = sentinel.write(dest) {
        return Err(cleanup_partial_destination(dest, err));
    }

    Ok(SourceReport {
        name: name.to_owned(),
        destination: dest.to_path_buf(),
        outcome: MaterializeOutcome::Created,
        log_dir: log_dir.map(Path::to_path_buf),
    })
}

pub(super) fn derive_leaf_from_url(url: &str) -> String {
    let trimmed = url.trim_end_matches('/');
    let leaf = trimmed
        .rsplit('/')
        .next()
        .and_then(|seg| seg.split('?').next())
        .filter(|seg| !seg.is_empty())
        .unwrap_or("payload");
    leaf.to_owned()
}
