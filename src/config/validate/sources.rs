//! Workspace `[[code_sources]]` / `[[data_sources]]` validators plus the
//! shared `derive_*_name` helpers that init/materializers also consume.

use std::collections::HashSet;
use std::path::Path;

use crate::config::schema::{CodeSourceConfig, DataSourceConfig};
use crate::config::validate::primitives::{
    validate_expected_sha256, validate_secret_ref_name_value,
};
use crate::error::{Result, StackError};

pub(crate) fn validate_code_sources(sources: &[CodeSourceConfig]) -> Result<()> {
    let mut seen_names: HashSet<String> = HashSet::new();
    for (index, source) in sources.iter().enumerate() {
        validate_code_source(index, source, &mut seen_names)?;
    }
    Ok(())
}

fn validate_code_source(
    index: usize,
    source: &CodeSourceConfig,
    seen_names: &mut HashSet<String>,
) -> Result<()> {
    let invalid = |reason: String| StackError::WorkspaceCodeSourceInvalid { index, reason };

    if source.source_type.as_str() != "git" {
        return Err(invalid(format!(
            "type must be `git`; got `{}`",
            source.source_type
        )));
    }
    let repo = source
        .repo
        .as_deref()
        .ok_or_else(|| invalid("repo is required when type is git".to_owned()))?;
    require_workspace_field("repo", repo, invalid)?;
    // `git+https://` (Cargo-style) is not accepted because the host `git`
    // binary does not understand it; operators must use a bare `https://`
    // URL or an `ssh://`/`git@…:…` reference.
    require_url_with_scheme("repo", repo, &["https", "ssh"], invalid)?;
    if let Some(branch) = source.branch.as_deref() {
        require_nonempty_trimmed("branch", branch, invalid)?;
    }
    if let Some(credential_ref) = source.credential_ref.as_deref() {
        require_nonempty_trimmed("credential_ref", credential_ref, invalid)?;
        validate_secret_ref_name_value(credential_ref).map_err(|err| {
            invalid(format!(
                "credential_ref `{credential_ref}` is not a valid secret reference: {err}"
            ))
        })?;
    }
    let derived = derive_code_source_name(source).map_err(invalid)?;
    if !seen_names.insert(derived.clone()) {
        return Err(invalid(format!(
            "duplicate destination name `{derived}` (override with `name = ...`)"
        )));
    }
    Ok(())
}

pub(crate) fn derive_code_source_name(
    source: &CodeSourceConfig,
) -> std::result::Result<String, String> {
    if let Some(name) = source.name.as_deref() {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err("name must not be blank".to_owned());
        }
        ensure_safe_dir_name(trimmed).map(|s| s.to_owned())
    } else {
        let repo = source
            .repo
            .as_deref()
            .ok_or_else(|| "repo is required to derive a directory name".to_owned())?;
        let derived = derive_repo_name(repo)?;
        ensure_safe_dir_name(&derived).map(|s| s.to_owned())
    }
}

fn derive_repo_name(repo: &str) -> std::result::Result<String, String> {
    let trimmed = repo.trim().trim_end_matches('/');
    let leaf = trimmed.rsplit(['/', ':'].as_slice()).next().unwrap_or("");
    let stem = leaf.strip_suffix(".git").unwrap_or(leaf);
    if stem.is_empty() {
        return Err(format!(
            "could not derive a directory name from repo `{repo}`"
        ));
    }
    Ok(stem.to_owned())
}

pub(crate) fn validate_data_sources(sources: &[DataSourceConfig]) -> Result<()> {
    let mut seen_names: HashSet<String> = HashSet::new();
    for (index, source) in sources.iter().enumerate() {
        validate_data_source(index, source, &mut seen_names)?;
    }
    Ok(())
}

fn validate_data_source(
    index: usize,
    source: &DataSourceConfig,
    seen_names: &mut HashSet<String>,
) -> Result<()> {
    let invalid = |reason: String| StackError::WorkspaceDataSourceInvalid { index, reason };
    let allowed_types = ["local", "https", "s3"];
    if !allowed_types.contains(&source.source_type.as_str()) {
        return Err(invalid(format!(
            "type must be one of {}; got `{}`",
            allowed_types.join(", "),
            source.source_type
        )));
    }

    let reject_for_type = |field: &'static str, value: Option<&str>| -> Result<()> {
        if value.map(|v| !v.trim().is_empty()).unwrap_or(false) {
            Err(invalid(format!(
                "{field} is not valid when type is {}",
                source.source_type
            )))
        } else {
            Ok(())
        }
    };

    // Numeric caps that may be set on the source. We validate non-zero
    // and reject them on source types that ignore them so operators do
    // not write configs that load cleanly but silently no-op the cap.
    let reject_numeric_for_type = |field: &'static str, value: Option<u64>| -> Result<()> {
        match value {
            Some(0) => Err(invalid(format!("{field} must be greater than zero"))),
            Some(_) => Err(invalid(format!(
                "{field} is not valid when type is {}",
                source.source_type
            ))),
            None => Ok(()),
        }
    };
    let require_nonzero = |field: &'static str, value: Option<u64>| -> Result<()> {
        if let Some(0) = value {
            return Err(invalid(format!("{field} must be greater than zero")));
        }
        Ok(())
    };

    match source.source_type.as_str() {
        "local" => {
            let path = source
                .path
                .as_deref()
                .ok_or_else(|| invalid("path is required when type is local".to_owned()))?;
            require_nonempty_trimmed("path", path, invalid)?;
            if !Path::new(path).is_absolute() {
                return Err(invalid(format!("path `{path}` must be absolute")));
            }
            for component in Path::new(path).components() {
                if matches!(component, std::path::Component::ParentDir) {
                    return Err(invalid(format!(
                        "path `{path}` must not contain `..` segments"
                    )));
                }
            }
            reject_for_type("url", source.url.as_deref())?;
            reject_for_type("expected_sha256", source.expected_sha256.as_deref())?;
            reject_for_type("bucket", source.bucket.as_deref())?;
            reject_for_type("prefix", source.prefix.as_deref())?;
            reject_for_type("region", source.region.as_deref())?;
            reject_for_type("access_key_ref", source.access_key_ref.as_deref())?;
            reject_for_type("secret_key_ref", source.secret_key_ref.as_deref())?;
            reject_numeric_for_type("max_download_bytes", source.max_download_bytes)?;
            reject_numeric_for_type("max_extracted_bytes", source.max_extracted_bytes)?;
        }
        "https" => {
            let url = source
                .url
                .as_deref()
                .ok_or_else(|| invalid("url is required when type is https".to_owned()))?;
            require_nonempty_trimmed("url", url, invalid)?;
            if !url.starts_with("https://") {
                return Err(invalid("url must start with https://".to_owned()));
            }
            if let Some(sha) = source.expected_sha256.as_deref() {
                validate_expected_sha256(sha)
                    .map_err(|err| invalid(format!("expected_sha256 is invalid: {err}")))?;
            }
            require_nonzero("max_download_bytes", source.max_download_bytes)?;
            require_nonzero("max_extracted_bytes", source.max_extracted_bytes)?;
            reject_for_type("path", source.path.as_deref())?;
            reject_for_type("bucket", source.bucket.as_deref())?;
            reject_for_type("prefix", source.prefix.as_deref())?;
            reject_for_type("region", source.region.as_deref())?;
            reject_for_type("access_key_ref", source.access_key_ref.as_deref())?;
            reject_for_type("secret_key_ref", source.secret_key_ref.as_deref())?;
        }
        "s3" => {
            let bucket = source
                .bucket
                .as_deref()
                .ok_or_else(|| invalid("bucket is required when type is s3".to_owned()))?;
            require_nonempty_trimmed("bucket", bucket, invalid)?;
            let region = source
                .region
                .as_deref()
                .ok_or_else(|| invalid("region is required when type is s3".to_owned()))?;
            require_nonempty_trimmed("region", region, invalid)?;
            let access = source
                .access_key_ref
                .as_deref()
                .ok_or_else(|| invalid("access_key_ref is required when type is s3".to_owned()))?;
            require_nonempty_trimmed("access_key_ref", access, invalid)?;
            validate_secret_ref_name_value(access)
                .map_err(|err| invalid(format!("access_key_ref `{access}` is not valid: {err}")))?;
            let secret = source
                .secret_key_ref
                .as_deref()
                .ok_or_else(|| invalid("secret_key_ref is required when type is s3".to_owned()))?;
            require_nonempty_trimmed("secret_key_ref", secret, invalid)?;
            validate_secret_ref_name_value(secret)
                .map_err(|err| invalid(format!("secret_key_ref `{secret}` is not valid: {err}")))?;
            if let Some(prefix) = source.prefix.as_deref() {
                require_nonempty_trimmed("prefix", prefix, invalid)?;
            }
            require_nonzero("max_download_bytes", source.max_download_bytes)?;
            // S3 ingest does not extract archives, so the extracted cap
            // would be silently ignored. Reject it explicitly.
            reject_numeric_for_type("max_extracted_bytes", source.max_extracted_bytes)?;
            reject_for_type("path", source.path.as_deref())?;
            reject_for_type("url", source.url.as_deref())?;
            reject_for_type("expected_sha256", source.expected_sha256.as_deref())?;
        }
        _ => unreachable!("source_type already validated"),
    }

    if let Some(name) = source.name.as_deref() {
        require_nonempty_trimmed("name", name, invalid)?;
        ensure_safe_dir_name(name.trim()).map_err(invalid)?;
    }

    let derived = derive_data_source_name(source).map_err(invalid)?;
    if !seen_names.insert(derived.clone()) {
        return Err(invalid(format!(
            "duplicate destination name `{derived}` (override with `name = ...`)"
        )));
    }
    Ok(())
}

pub(crate) fn derive_data_source_name(
    source: &DataSourceConfig,
) -> std::result::Result<String, String> {
    if let Some(name) = source.name.as_deref() {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err("name must not be blank".to_owned());
        }
        return ensure_safe_dir_name(trimmed).map(|s| s.to_owned());
    }
    let derived = match source.source_type.as_str() {
        "local" => {
            // Local paths can point at either a file (`/data/dataset.tar.gz`)
            // or a directory (`/data/reports.v1`). We cannot tell which at
            // validation time, and stripping the extension blindly would
            // mangle directory names like `reports.v1` into `reports`.
            // Preserve the basename as-is; operators who want stripping
            // should set `name = "..."` explicitly.
            let path = source.path.as_deref().unwrap_or("");
            let leaf = Path::new(path)
                .file_name()
                .and_then(|f| f.to_str())
                .unwrap_or("");
            if leaf.is_empty() {
                return Err(format!(
                    "could not derive a directory name from path `{path}`"
                ));
            }
            leaf.to_owned()
        }
        "https" => {
            let url = source.url.as_deref().unwrap_or("");
            let trimmed = url.trim_end_matches('/');
            let leaf = trimmed
                .rsplit('/')
                .next()
                .and_then(|seg| seg.split('?').next())
                .unwrap_or("");
            if leaf.is_empty() {
                return Err(format!(
                    "could not derive a directory name from url `{url}`"
                ));
            }
            strip_archive_extension(leaf).to_owned()
        }
        "s3" => {
            let bucket = source.bucket.as_deref().unwrap_or("");
            let prefix = source.prefix.as_deref().unwrap_or("");
            let trimmed = prefix.trim_end_matches('/');
            if trimmed.is_empty() {
                if bucket.is_empty() {
                    return Err("could not derive a directory name (empty bucket)".to_owned());
                }
                bucket.to_owned()
            } else {
                let leaf = trimmed.rsplit('/').next().unwrap_or(bucket);
                leaf.to_owned()
            }
        }
        _ => unreachable!("source_type already validated"),
    };
    ensure_safe_dir_name(&derived).map(|s| s.to_owned())
}

fn ensure_safe_dir_name(name: &str) -> std::result::Result<&str, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("derived directory name is empty".to_owned());
    }
    if trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed.contains('\0')
        || trimmed == "."
        || trimmed == ".."
    {
        return Err(format!(
            "directory name `{trimmed}` is not safe; override with `name = ...`"
        ));
    }
    Ok(trimmed)
}

fn strip_archive_extension(name: &str) -> &str {
    for ext in [
        ".tar.gz", ".tar.bz2", ".tar.xz", ".tar.zst", ".tgz", ".tbz2", ".txz",
    ] {
        if let Some(stripped) = name.strip_suffix(ext)
            && !stripped.is_empty()
        {
            return stripped;
        }
    }
    // Only strip a trailing extension when the prefix is non-empty;
    // otherwise names like `.tmpXYZ` would collapse to "".
    if let Some(dot) = name.rfind('.')
        && dot > 0
    {
        return &name[..dot];
    }
    name
}

fn require_workspace_field<F>(field: &'static str, value: &str, build: F) -> Result<()>
where
    F: FnOnce(String) -> StackError,
{
    if value.trim().is_empty() {
        Err(build(format!("{field} is required")))
    } else {
        Ok(())
    }
}

fn require_nonempty_trimmed<F>(field: &'static str, value: &str, build: F) -> Result<()>
where
    F: FnOnce(String) -> StackError,
{
    if value.trim().is_empty() || value.len() != value.trim().len() {
        Err(build(format!("{field} must be non-empty and trimmed")))
    } else {
        Ok(())
    }
}

fn require_url_with_scheme<F>(
    field: &'static str,
    value: &str,
    allowed_prefixes: &[&str],
    build: F,
) -> Result<()>
where
    F: FnOnce(String) -> StackError,
{
    let has_known_scheme = allowed_prefixes
        .iter()
        .any(|scheme| value.starts_with(&format!("{scheme}://")));
    let looks_like_git_ssh = value.contains('@')
        && value.contains(':')
        && !value.starts_with("http://")
        && !value.starts_with("https://");
    // Absolute filesystem paths are valid git "URLs" (file:// shorthand) and
    // are useful for tests and on-host mirrors. We accept them only when
    // the caller's allowlist includes a path-shaped scheme — git is the
    // only one today.
    let looks_like_path = allowed_prefixes.contains(&"https")
        && Path::new(value).is_absolute()
        && !value.contains("://");
    if has_known_scheme
        || (allowed_prefixes.contains(&"ssh") && looks_like_git_ssh)
        || looks_like_path
    {
        Ok(())
    } else {
        Err(build(format!(
            "{field} must use one of these schemes: {}",
            allowed_prefixes.join(", ")
        )))
    }
}
