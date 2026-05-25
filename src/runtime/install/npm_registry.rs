//! Minimal HTTP-only npm registry client used by `acps agent check`.
//!
//! The installer flow (`agent_installer::resolve_npm_package_version`) shells
//! out to `npm view <pkg> version --json` so it inherits the operator's npm
//! configuration. `acps agent check` is a read-only freshness probe that
//! deliberately avoids spawning npm — operators may run check from a
//! minimal container without npm installed, and a stuck npm process would
//! poison the freshness report.
//!
//! The endpoint contract: `https://registry.npmjs.org/<package>/latest`
//! returns a JSON document whose `.version` field carries the latest
//! published version. We surface only that field; the rest of the payload
//! is opaque.

use std::time::Duration;

use serde::Deserialize;

use crate::error::{Result, StackError};

const REGISTRY_BASE: &str = "https://registry.npmjs.org";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const USER_AGENT: &str = concat!("acp-stack/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Deserialize)]
struct LatestResponse {
    version: String,
}

/// Return the latest published version for `package` per the public npm
/// registry. Scoped packages (`@scope/name`) work without additional escaping;
/// reqwest URL-encodes the path segment.
pub fn latest_version(package: &str) -> Result<String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .user_agent(USER_AGENT)
        .build()
        .map_err(|source| StackError::NpmRegistryFetch {
            package: package.to_owned(),
            source,
        })?;
    let url = format!("{REGISTRY_BASE}/{package}/latest");
    let response = client
        .get(&url)
        .header("Accept", "application/json")
        .send()
        .map_err(|source| StackError::NpmRegistryFetch {
            package: package.to_owned(),
            source,
        })?;
    let response = response
        .error_for_status()
        .map_err(|source| StackError::NpmRegistryFetch {
            package: package.to_owned(),
            source,
        })?;
    let parsed: LatestResponse =
        response
            .json()
            .map_err(|source| StackError::NpmRegistryFetch {
                package: package.to_owned(),
                source,
            })?;
    if parsed.version.trim().is_empty() {
        return Err(StackError::NpmRegistryEmptyVersion {
            package: package.to_owned(),
        });
    }
    Ok(parsed.version)
}
