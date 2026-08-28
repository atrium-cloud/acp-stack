//! Minimal HTTP-only npm registry client for `acps agent check`, which never spawns npm: check
//! must work from a container without npm, and a stuck npm would poison the freshness report.

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

/// Return the latest published version for `package`; scoped names need no extra escaping.
pub fn latest_version(package: &str) -> Result<String> {
    let client = crate::http_client::blocking_client_builder()
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
