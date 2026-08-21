//! Machine-readable JSON Schema for the `/v1` HTTP contract and the config
//! file. Dev-tools only: the `generate-api-schema` bin is the sole caller, and
//! nothing in the shipped binary references this module.
//!
//! # Why three generator passes
//!
//! schemars keys its definition cache by (type, [`Contract`]), and serde's
//! `#[serde(default)]` / `skip_serializing_if` attributes put a field in
//! `required` under one contract but not the other. Request bodies are what a
//! client *sends* (deserialize contract); response bodies are what the server
//! *emits* (serialize contract). Generating both under one contract would
//! publish a wrong `required` list for one side. So `acps_schema` runs three
//! passes into disjoint `$defs` namespaces and merges them:
//!
//! - requests  → `#/$defs/request/*`   (deserialize)
//! - responses → `#/$defs/response/*`  (serialize)
//! - config    → `#/$defs/config/*`    (deserialize)
//!
//! A type that appears on both sides (e.g. `Config`, which is both imported and
//! exported) gets one correct entry per namespace; the entries do not collide.
//!
//! # Regenerating
//!
//! ```sh
//! cargo run --features dev-tools --bin generate-api-schema
//! ```
//!
//! The checked-in files are byte-pinned by the drift test below and by the
//! `--all-features` test run in CI and the release gate, so a DTO change that
//! is not regenerated fails the build. A schemars minor upgrade may legitimately
//! reshuffle key order and trip the same test; the fix is to regenerate.

use schemars::generate::{Contract, SchemaGenerator, SchemaSettings};
use schemars::transform::ReplaceBoolSchemas;
use serde_json::{Map, Value, json};

mod config;
mod coverage;
mod requests;
mod responses;

pub use coverage::{CoverageReport, NamespaceCoverage, coverage_report};

// CONSTANTS
/// In-repo location of the generated schema, relative to the crate manifest.
pub const SCHEMA_PATH: &str = "docs/specs/api/acps-schema.json";
/// In-repo location of the generated schema version/definition-count sidecar.
pub const META_PATH: &str = "docs/specs/api/acps-schema.meta.json";
/// `$id` of the published schema — the durable stable-release download URL.
const SCHEMA_ID: &str =
    "https://github.com/atrium-cloud/acp-stack/releases/latest/download/acps-schema.json";
const SCHEMA_TITLE: &str = "acp-stack /v1 API and configuration contract";
/// Root `description` of the published schema. Consumers fetch this file
/// standalone from the release asset, so it has to explain what the document
/// is, how the three namespaces differ, that the root is a container rather
/// than a validatable body, and what is deliberately absent.
const SCHEMA_DESCRIPTION: &str = concat!(
    "Generated from the Rust wire types of acp-stack; do not edit by hand — regenerate with ",
    "`cargo run --features dev-tools --bin generate-api-schema`. This root is a container of ",
    "definitions, not a validatable body: validate a payload against ",
    "`#/$defs/<namespace>/<TypeName>`. `$defs` splits into three namespaces because serde's ",
    "`default` and `skip_serializing_if` attributes place a field in `required` under one ",
    "direction but not the other — `request` is the deserialize contract (what a client sends), ",
    "`response` is the serialize contract (what the server emits), and `config` is the ",
    "deserialize contract for `acps-config.toml`. A type used on both sides appears once per ",
    "namespace. Conventions: timestamp fields are RFC 3339 strings; an optional response field ",
    "is omitted when absent rather than emitted as null, so its nullable type means absent and ",
    "null are equivalent. Not covered, by design: the `/v1/ws` `LiveEvent` frames and the ",
    "`acps init serve` streaming frames and state signals (hand-built and byte-pinned by golden ",
    "tests), the envelope-bypassing binary download handler and the `health/ready` handler ",
    "(whose readiness body is hand-built, not a typed DTO), and the untyped `config` import ",
    "response. Cross-field rules (mutually-required or mutually-exclusive ",
    "fields, exactly-one-of constraints, blank-as-absent) are mostly inexpressible in JSON Schema ",
    "and stay enforced in code; `ApiError.code` is likewise an open dotted-namespace string ",
    "rather than a closed enum. See docs/specs/api/api.md for both."
);
const META_SCHEMA_VERSION: u16 = 1;
/// Draft the schema is emitted in; also the `$schema` value of the merged root.
const META_SCHEMA_DRAFT: &str = "https://json-schema.org/draft/2020-12/schema";

/// One generator pass at the default `/$defs` definitions path. The default
/// path is deliberate: schemars' transforms only recurse through the literal
/// `$defs` key, so a nested path (`/$defs/request`) would leave subschemas
/// (e.g. bare-`true` `serde_json::Value` fields) untransformed. Namespacing
/// happens after generation, in [`pass_defs`], by re-keying and ref-rewriting.
fn generator(contract: Contract) -> SchemaGenerator {
    let mut settings = SchemaSettings::draft2020_12();
    settings.contract = contract;
    settings.untagged_enum_variant_titles = true;
    // `serde_json::Value` / `Map` fields render as the bare boolean schema
    // `true`; normalize those to `{}`, which every client generator accepts,
    // while leaving `additionalProperties: false` (from `deny_unknown_fields`)
    // as-is. `ReplaceBoolSchemas` is `#[non_exhaustive]`: build with
    // `default()`, then set the field.
    let mut replace_bool_schemas = ReplaceBoolSchemas::default();
    replace_bool_schemas.skip_additional_properties = true;
    settings
        .with_transform(replace_bool_schemas)
        .into_generator()
}

/// Run one umbrella pass and return its (flat) `$defs` re-keyed under
/// `namespace`: every intra-pass `#/$defs/X` ref is rewritten to
/// `#/$defs/<namespace>/X` so the three passes coexist after the merge. The
/// umbrella root itself is discarded — only the definitions it pulled in are
/// kept.
///
/// `pub(crate)` so modules that own module-private DTOs (e.g. the bootstrap
/// init API) can run their own umbrella through it without exposing the DTO
/// types to this module — the generic is monomorphized at the call site, where
/// the umbrella and its members are visible.
pub(crate) fn pass_defs<T: schemars::JsonSchema>(contract: Contract, namespace: &str) -> Value {
    let schema = generator(contract).into_root_schema_for::<T>().to_value();
    let mut defs = schema
        .get("$defs")
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()));
    rewrite_refs(&mut defs, namespace);
    defs
}

/// Rewrite every `{"$ref": "#/$defs/X"}` in `value` to
/// `{"$ref": "#/$defs/<namespace>/X"}`, in place and recursively.
fn rewrite_refs(value: &mut Value, namespace: &str) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(reference)) = map.get_mut("$ref")
                && let Some(name) = reference.strip_prefix("#/$defs/")
            {
                *reference = format!("#/$defs/{namespace}/{name}");
            }
            map.values_mut()
                .for_each(|child| rewrite_refs(child, namespace));
        }
        Value::Array(items) => items
            .iter_mut()
            .for_each(|item| rewrite_refs(item, namespace)),
        _ => {}
    }
}

/// Merge a set of (ref-rewritten) definitions into `accumulator`. schemars keys
/// `$defs` by a type's short name. One type legitimately arrives twice when it
/// is reached from two umbrellas in the same namespace (e.g.
/// `NativeConfigInspection`, pulled by both the main-API and init response
/// passes) — those definitions are byte-identical and collapse harmlessly. But
/// two *different* types sharing a short name would overwrite each other and the
/// published schema would describe the wrong type. This is a dev-tools-only
/// generator, so a hard panic on a *conflicting* redefinition is the correct
/// failure.
fn merge_defs(defs: Value, accumulator: &mut Map<String, Value>) {
    if let Value::Object(map) = defs {
        for (name, definition) in map {
            if let Some(existing) = accumulator.get(&name) {
                assert!(
                    existing == &definition,
                    "conflicting schema definitions for short name `{name}`: two different types \
                     share it within one namespace. Rename one, or split the umbrella."
                );
                continue;
            }
            accumulator.insert(name, definition);
        }
    }
}

/// Merge one umbrella pass's definitions into `accumulator`. A namespace can be
/// built from several umbrellas — e.g. the init API's module-private DTOs are
/// contributed separately by `crate::cli::init_*_defs`.
fn merge_pass<T: schemars::JsonSchema>(
    contract: Contract,
    namespace: &str,
    accumulator: &mut Map<String, Value>,
) {
    merge_defs(pass_defs::<T>(contract, namespace), accumulator);
}

/// The full published schema document: one draft-2020-12 root whose `$defs`
/// carries the `request`, `response`, and `config` namespaces.
pub fn acps_schema() -> Value {
    let mut request = Map::new();
    merge_pass::<requests::AcpsRequestTypes>(Contract::Deserialize, "request", &mut request);
    merge_defs(crate::cli::init_request_defs(), &mut request);

    let mut response = Map::new();
    merge_pass::<responses::AcpsResponseTypes>(Contract::Serialize, "response", &mut response);
    merge_defs(crate::cli::init_response_defs(), &mut response);

    let mut config = Map::new();
    merge_pass::<config::AcpsConfigTypes>(Contract::Deserialize, "config", &mut config);

    json!({
        "$schema": META_SCHEMA_DRAFT,
        "$id": SCHEMA_ID,
        "title": SCHEMA_TITLE,
        "description": SCHEMA_DESCRIPTION,
        "$defs": {
            "request": Value::Object(request),
            "response": Value::Object(response),
            "config": Value::Object(config),
        }
    })
}

/// Version + per-namespace definition counts sidecar. `version` is the
/// three-part `CARGO_PKG_VERSION`; nightlies carry their fourth component only
/// in the git tag, so this stays stable and the drift test can byte-compare.
pub fn acps_schema_meta() -> Value {
    let schema = acps_schema();
    let count = |namespace: &str| -> usize {
        schema
            .get("$defs")
            .and_then(|defs| defs.get(namespace))
            .and_then(Value::as_object)
            .map(serde_json::Map::len)
            .unwrap_or(0)
    };
    json!({
        "schema_version": META_SCHEMA_VERSION,
        "version": env!("CARGO_PKG_VERSION"),
        "schema": "acps-schema.json",
        "definitions": {
            "request": count("request"),
            "response": count("response"),
            "config": count("config"),
        },
    })
}

/// Canonical on-disk rendering: pretty JSON with a trailing newline, matching
/// the byte-comparison the drift test performs.
pub fn render(value: &Value) -> String {
    let mut rendered = serde_json::to_string_pretty(value)
        .expect("serde_json::Value always serializes to a string");
    rendered.push('\n');
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn checked_in(relative: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
    }

    const REGENERATE_HINT: &str =
        "regenerate with: cargo run --features dev-tools --bin generate-api-schema";

    #[test]
    fn schema_matches_checked_in_file() {
        assert_eq!(
            render(&acps_schema()),
            checked_in(SCHEMA_PATH),
            "{SCHEMA_PATH} is stale; {REGENERATE_HINT}"
        );
    }

    #[test]
    fn meta_matches_checked_in_file() {
        assert_eq!(
            render(&acps_schema_meta()),
            checked_in(META_PATH),
            "{META_PATH} is stale; {REGENERATE_HINT}"
        );
    }

    #[test]
    fn schema_covers_every_handler_wire_type() {
        let report = coverage_report();
        assert!(
            report.is_complete(),
            "schema is missing handler wire types — request gaps: {:?}, response gaps: {:?}. \
             Add them to the umbrellas in src/schema_export/{{requests,responses}}.rs, or list \
             them as documented gaps if they are intentionally untyped.",
            report.request.uncovered,
            report.response.uncovered,
        );
    }

    #[test]
    fn every_ref_resolves_within_the_document() {
        let schema = acps_schema();
        let mut refs = Vec::new();
        collect_refs(&schema, &mut refs);
        assert!(!refs.is_empty(), "expected the schema to contain $refs");
        for reference in refs {
            assert!(resolves(&schema, &reference), "dangling $ref: {reference}");
        }
    }

    fn collect_refs(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    if key == "$ref" {
                        if let Some(reference) = child.as_str() {
                            out.push(reference.to_owned());
                        }
                    } else {
                        collect_refs(child, out);
                    }
                }
            }
            Value::Array(items) => items.iter().for_each(|item| collect_refs(item, out)),
            _ => {}
        }
    }

    fn resolves(root: &Value, reference: &str) -> bool {
        let Some(pointer) = reference.strip_prefix('#') else {
            // External refs are not emitted by our generator.
            return false;
        };
        root.pointer(pointer).is_some()
    }
}
