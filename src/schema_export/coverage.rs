//! Measures how completely the published schema covers the real `/v1` HTTP
//! surface. The umbrellas in `super::{requests, responses}` are hand-maintained,
//! so a new endpoint can be added with no schema entry; this check catches that
//! by re-deriving the wire-type set *independently* — from the axum handler
//! signatures in the route sources — and diffing it against the generated
//! `$defs`.
//!
//! Ground truth: every `Json<T>` / `Query<T>` extractor is a request type, and
//! every `ApiSuccess<T>` / `ApiResult<T>` return is a response type. A type used
//! by a handler but absent from `$defs` is an uncovered gap. Types that bypass
//! the envelope (raw byte/`Response` handlers, WebSocket frames) never appear in
//! these patterns, so they are excluded from the denominator by construction —
//! matching the documented gaps, not silently missed.
//!
//! The bootstrap init API (`src/cli/init/serve`) is scanned too, so a new init
//! request body added without a `schema_umbrella` entry fails coverage. Its
//! handlers return `-> Response` and build bodies via `ApiSuccess::new(value)`
//! rather than a typed `ApiSuccess<T>` return, so init *response* types are not
//! textually discoverable here; those stay guarded only by the hand-maintained
//! `InitResponseTypes` umbrella. A constructor-pattern scanner would be the only
//! way to close that, and it is not worth the fragility.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::Value;

// CONSTANTS
/// Handler source roots scanned for wire-type usage, relative to the manifest.
/// Includes the bootstrap init API so its request DTOs are covered too (see the
/// module doc for why init responses are not discoverable here).
const HANDLER_ROOTS: &[&str] = &["src/api/routes", "src/cli/init/serve"];
/// Individual handler source files scanned (not whole directories).
const HANDLER_FILES: &[&str] = &["src/api/ws.rs"];
/// Extractor/return markers and the namespace each contributes to.
const REQUEST_MARKERS: &[&str] = &["Json<", "Query<"];
const RESPONSE_MARKERS: &[&str] = &["ApiSuccess<", "ApiResult<"];
/// Payload type names that are intentionally not part of the typed contract:
/// `Value` is the untyped `config_import` response; `Bytes`/`Body`/`Response`
/// are the envelope-bypassing raw handlers.
const UNTYPED_PAYLOADS: &[&str] = &["Value", "Bytes", "Body", "Response", "String"];

/// A namespace's coverage: how many distinct handler wire types were found and
/// which of them are missing from the generated schema.
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

    /// Fraction in `[0, 1]`; a namespace with no discovered types is fully
    /// covered by definition.
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

/// Read every handler source once (directory roots recursively + single files).
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

/// Pull the simple type name out of every `MARKER<...>` occurrence across all
/// sources, dropping untyped payloads and generic noise.
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

/// Read the type name that begins `text` (immediately after a `<`), resolving
/// `a::b::Type` to its last segment and stopping at the first non-path
/// character. Returns `None` for a nested generic opener (`Vec<...>`, `&...`)
/// that is not itself a named payload.
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
