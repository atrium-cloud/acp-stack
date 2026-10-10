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
    latest_version_at(REGISTRY_BASE, package)
}

fn latest_version_at(registry_base: &str, package: &str) -> Result<String> {
    let fetch_error = |source, body| StackError::NpmRegistryFetch {
        package: package.to_owned(),
        source,
        body,
    };
    let client = crate::http_client::blocking_client_builder()
        .timeout(REQUEST_TIMEOUT)
        .user_agent(USER_AGENT)
        .build()
        .map_err(|source| fetch_error(source, None))?;
    let url = format!("{registry_base}/{package}/latest");
    let response = client
        .get(&url)
        .header("Accept", "application/json")
        .send()
        .map_err(|source| fetch_error(source, None))?;
    if let Err(source) = response.error_for_status_ref() {
        let body = crate::http_client::blocking_error_response_body(response);
        return Err(fetch_error(source, Some(body)));
    }
    let parsed: LatestResponse = response
        .json()
        .map_err(|source| fetch_error(source, None))?;
    if parsed.version.trim().is_empty() {
        return Err(StackError::NpmRegistryEmptyVersion {
            package: package.to_owned(),
        });
    }
    Ok(parsed.version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::http::StatusCode;
    use axum::routing::get;

    #[test]
    fn non_success_status_keeps_the_redacted_response_body() {
        let router = Router::new().route(
            "/missing-package/latest",
            get(|| async {
                (
                    StatusCode::NOT_FOUND,
                    r#"{"error":"Not found","token":"sk-npmregistry0123456789"}"#,
                )
            }),
        );
        let server = crate::http_client::test_server::spawn(router);
        let error =
            latest_version_at(&server.base_url(), "missing-package").expect_err("404 must fail");
        let StackError::NpmRegistryFetch { body, .. } = &error else {
            panic!("unexpected error: {error}");
        };
        let body = body.as_deref().expect("body kept");
        assert!(body.contains("Not found"), "{body}");
        assert!(!body.contains("sk-npmregistry"), "{body}");
        let display = error.to_string();
        assert!(display.contains("404"), "{display}");
        assert!(display.contains("Not found"), "{display}");
    }
}
