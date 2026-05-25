//! Data lane: S3 bucket/prefix ingestion. SigV4-signed `ListObjectsV2` +
//! `GetObject` against the configured region, with a test-only endpoint
//! override for local MinIO and VPC-internal targets.

use std::path::{Path, PathBuf};

use crate::config::DataSourceConfig;
use crate::error::{Result, StackError};
use crate::secrets::SecretStore;

use super::common::{
    Sentinel, SentinelBody, capture_error, cleanup_partial_destination, ensure_dest_or_fail,
    ensure_destination_not_symlink, sentinel_if_present, write_operation_capture,
};
use super::{CAPTURE_TAG_S3_DOWNLOAD, MaterializeOutcome, SourceReport};

pub(super) fn materialize_s3(
    index: usize,
    source: &DataSourceConfig,
    name: &str,
    dest: &Path,
    secrets: &SecretStore,
    log_dir: Option<&Path>,
) -> Result<SourceReport> {
    use crate::runtime::workspace_sources::s3_client::{Credentials, S3Client};

    let bucket =
        source
            .bucket
            .as_deref()
            .ok_or_else(|| StackError::WorkspaceDataSourceInvalid {
                index,
                reason: "bucket is required for s3 sources".to_owned(),
            })?;
    let region =
        source
            .region
            .as_deref()
            .ok_or_else(|| StackError::WorkspaceDataSourceInvalid {
                index,
                reason: "region is required for s3 sources".to_owned(),
            })?;
    let access_ref =
        source
            .access_key_ref
            .as_deref()
            .ok_or_else(|| StackError::WorkspaceDataSourceInvalid {
                index,
                reason: "access_key_ref is required for s3 sources".to_owned(),
            })?;
    let secret_ref =
        source
            .secret_key_ref
            .as_deref()
            .ok_or_else(|| StackError::WorkspaceDataSourceInvalid {
                index,
                reason: "secret_key_ref is required for s3 sources".to_owned(),
            })?;
    let prefix = source.prefix.clone();
    let max_total_bytes = source
        .max_download_bytes
        .unwrap_or(crate::runtime::workspace_sources::safe_download::DEFAULT_MAX_DOWNLOAD_BYTES);

    ensure_destination_not_symlink(dest)?;
    if let Some(existing) = sentinel_if_present(dest)?
        && let SentinelBody::S3 {
            bucket: existing_bucket,
            prefix: existing_prefix,
            region: existing_region,
            ..
        } = &existing.body
        && existing_bucket == bucket
        && existing_prefix.as_deref() == prefix.as_deref()
        && existing_region == region
    {
        return Ok(SourceReport {
            name: name.to_owned(),
            destination: dest.to_path_buf(),
            outcome: MaterializeOutcome::Verified,
            log_dir: None,
        });
    }
    ensure_dest_or_fail(dest)?;
    std::fs::create_dir_all(dest).map_err(|source_err| StackError::WorkspaceMaterializeFailed {
        reason: format!("create dest `{}`: {source_err}", dest.display()),
    })?;

    let access_key = secrets.get(access_ref)?.to_owned();
    let secret_key = secrets.get(secret_ref)?.to_owned();
    let mut client = S3Client::new(
        region.to_owned(),
        Credentials {
            access_key,
            secret_key,
        },
    )?;
    if let Ok(endpoint) = std::env::var("ACP_STACK_S3_ENDPOINT_OVERRIDE")
        && !endpoint.is_empty()
    {
        // Test/operator escape hatch for hitting a local MinIO mock or a
        // VPC-internal endpoint without baking it into TOML. Production
        // deployments leave the env var unset.
        client = client.with_endpoint_base(endpoint);
    }

    let outcome = download_s3_objects(&client, bucket, prefix.as_deref(), dest, max_total_bytes);
    let (bytes, objects) = match outcome {
        Ok(value) => {
            write_operation_capture(
                log_dir,
                CAPTURE_TAG_S3_DOWNLOAD,
                &format!(
                    "bucket={bucket}\nprefix={}\nregion={region}\ndestination={}\nbytes={}\nobjects={}\n",
                    prefix.as_deref().unwrap_or(""),
                    dest.display(),
                    value.0,
                    value.1,
                ),
                "",
            )
            .map_err(|err| cleanup_partial_destination(dest, err))?;
            value
        }
        Err(err) => {
            capture_error(log_dir, CAPTURE_TAG_S3_DOWNLOAD, &err);
            return Err(cleanup_partial_destination(dest, err));
        }
    };

    let sentinel = Sentinel::new(SentinelBody::S3 {
        bucket: bucket.to_owned(),
        prefix,
        region: region.to_owned(),
        bytes,
        objects,
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

pub(super) fn download_s3_objects(
    client: &crate::runtime::workspace_sources::s3_client::S3Client,
    bucket: &str,
    prefix: Option<&str>,
    dest: &Path,
    max_total_bytes: u64,
) -> Result<(u64, u64)> {
    let normalized_prefix = prefix.map(|p| p.trim_start_matches('/').to_owned());
    let mut total_bytes: u64 = 0;
    let mut total_objects: u64 = 0;
    let mut continuation: Option<String> = None;
    loop {
        let page = client.list_objects_v2(
            bucket,
            normalized_prefix.as_deref(),
            continuation.as_deref(),
        )?;
        for object in &page.objects {
            if object.key.ends_with('/') {
                continue;
            }
            let relative = match normalized_prefix.as_deref() {
                Some(prefix) => object
                    .key
                    .strip_prefix(prefix)
                    .unwrap_or(&object.key)
                    .trim_start_matches('/'),
                None => object.key.as_str(),
            };
            let safe = safe_object_path(relative)?;
            let target = dest.join(&safe);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|source_err| {
                    StackError::WorkspaceMaterializeFailed {
                        reason: format!("create dir `{}`: {source_err}", parent.display()),
                    }
                })?;
            }
            let projected = total_bytes.saturating_add(object.size);
            if projected > max_total_bytes {
                return Err(StackError::WorkspaceMaterializeFailed {
                    reason: format!(
                        "s3 ingest would exceed the {max_total_bytes}-byte size limit \
                         (after object `{}`)",
                        object.key
                    ),
                });
            }
            let bytes = client.get_object(bucket, &object.key, max_total_bytes - total_bytes)?;
            if total_bytes.saturating_add(bytes.len() as u64) > max_total_bytes {
                return Err(StackError::WorkspaceMaterializeFailed {
                    reason: format!("s3 object `{}` exceeded remaining size budget", object.key),
                });
            }
            std::fs::write(&target, &bytes).map_err(|source_err| {
                StackError::WorkspaceMaterializeFailed {
                    reason: format!("write s3 object `{}`: {source_err}", target.display()),
                }
            })?;
            total_bytes = total_bytes.saturating_add(bytes.len() as u64);
            total_objects = total_objects.saturating_add(1);
        }
        if !page.is_truncated || page.next_continuation_token.is_none() {
            break;
        }
        continuation = page.next_continuation_token;
    }
    Ok((total_bytes, total_objects))
}

pub(super) fn safe_object_path(relative: &str) -> Result<PathBuf> {
    let mut out = PathBuf::new();
    for component in Path::new(relative).components() {
        match component {
            std::path::Component::Normal(part) => out.push(part),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                return Err(StackError::WorkspaceMaterializeFailed {
                    reason: format!("s3 object key `{relative}` contains a parent-dir segment"),
                });
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                return Err(StackError::WorkspaceMaterializeFailed {
                    reason: format!("s3 object key `{relative}` resolved to an absolute path"),
                });
            }
        }
    }
    if out.as_os_str().is_empty() {
        return Err(StackError::WorkspaceMaterializeFailed {
            reason: format!("s3 object key `{relative}` is empty after sanitization"),
        });
    }
    Ok(out)
}
