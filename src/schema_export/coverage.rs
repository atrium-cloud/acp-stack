//! Measures how completely the published schema covers the `/v1` HTTP surface
//! by re-deriving the wire-type set independently from the axum handler
//! signatures and diffing it against the generated `$defs`.
//!
//! Init responses are a known blind spot: those handlers return `-> Response`
//! and build bodies via `ApiSuccess::new(value)`, so they are not textually
//! discoverable and stay guarded only by the `InitResponseTypes` umbrella.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::Value;

// CONSTANTS
/// Handler source roots scanned for wire-type usage, relative to the manifest.
const HANDLER_ROOTS: &[&str] = &["src/api/routes", "src/cli/init/serve"];
/// Individual handler source files scanned (not whole directories).
const HANDLER_FILES: &[&str] = &["src/api/ws.rs"];
/// Extractor/return markers and the namespace each contributes to.
const REQUEST_MARKERS: &[&str] = &["Json<", "Query<"];
const RESPONSE_MARKERS: &[&str] = &["ApiSuccess<", "ApiResult<"];
/// Payload type names intentionally outside the typed contract: the untyped
/// `config_import` response and the envelope-bypassing raw handlers.
const UNTYPED_PAYLOADS: &[&str] = &["Value", "Bytes", "Body", "Response", "String"];

/// A namespace's coverage: the handler wire types found and which are missing
/// from the generated schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespaceCoverage {
    pub namespace: &'static str,
    pub used: BTreeSet<String>,
    pub uncovered: BTreeSet<String>,
}

impl NamespaceCoverage {
    pub fn covered(&self) -> usize {
        self.used.len() - self.uncovered.len()
    }

    /// Fraction in `[0, 1]`; no discovered types counts as fully covered.
    pub fn ratio(&self) -> f64 {
        if self.used.is_empty() {
            return 1.0;
        }
        self.covered() as f64 / self.used.len() as f64
    }
}

/// Coverage across both request and response namespaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverageReport {
    pub request: NamespaceCoverage,
    pub response: NamespaceCoverage,
}

impl CoverageReport {
    pub fn is_complete(&self) -> bool {
        self.request.uncovered.is_empty() && self.response.uncovered.is_empty()
    }
}

/// Build the coverage report by scanning the handler sources under
/// `CARGO_MANIFEST_DIR` and diffing against `super::acps_schema`.
pub fn coverage_report() -> CoverageReport {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let sources = handler_sources(manifest);

    let used_requests = extract_payloads(&sources, REQUEST_MARKERS);
    let used_responses = extract_payloads(&sources, RESPONSE_MARKERS);

    let schema = super::acps_schema();
    let request_defs = defs_keys(&schema, "request");
    let response_defs = defs_keys(&schema, "response");

    CoverageReport {
        request: namespace_coverage("request", used_requests, &request_defs),
        response: namespace_coverage("response", used_responses, &response_defs),
    }
}

fn namespace_coverage(
    namespace: &'static str,
    used: BTreeSet<String>,
    defs: &BTreeSet<String>,
) -> NamespaceCoverage {
    let uncovered = used.difference(defs).cloned().collect();
    NamespaceCoverage {
        namespace,
        used,
        uncovered,
    }
}

fn defs_keys(schema: &Value, namespace: &str) -> BTreeSet<String> {
    schema
        .get("$defs")
        .and_then(|defs| defs.get(namespace))
        .and_then(Value::as_object)
        .map(|map| map.keys().cloned().collect())
        .unwrap_or_default()
}

/// Read every handler source once.
fn handler_sources(manifest: &Path) -> Vec<String> {
    let mut files = Vec::new();
    for root in HANDLER_ROOTS {
        collect_rust_files(&manifest.join(root), &mut files);
    }
    for file in HANDLER_FILES {
        files.push(manifest.join(file));
    }
    files
        .iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .collect()
}

fn collect_rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rust_files(&path, out);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// Pull the simple type name out of every `MARKER<...>` occurrence.
fn extract_payloads(sources: &[String], markers: &[&str]) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for source in sources {
        for marker in markers {
            let mut rest = source.as_str();
            while let Some(index) = rest.find(marker) {
                rest = &rest[index + marker.len()..];
                if let Some(name) = leading_type_name(rest)
                    && !UNTYPED_PAYLOADS.contains(&name.as_str())
                {
                    found.insert(name);
                }
            }
        }
    }
    found
}

/// Read the type name beginning `text`, resolving `a::b::Type` to its last
/// segment; `None` for a nested generic opener that is not a named payload.
fn leading_type_name(text: &str) -> Option<String> {
    let end = text
        .find(|c: char| !c.is_alphanumeric() && c != '_' && c != ':')
        .unwrap_or(text.len());
    let path = &text[..end];
    let name = path.rsplit("::").next().unwrap_or(path);
    if name.is_empty() {
        return None;
    }
    // A leading uppercase distinguishes a type from a lifetime/borrow artifact.
    if name.chars().next().is_some_and(char::is_uppercase) {
        Some(name.to_owned())
    } else {
        None
    }
}
