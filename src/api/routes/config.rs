use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::header::IF_MATCH;
use axum::http::{HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::super::core::AppState;
use crate::envelope::ApiSuccess;
use crate::error::StackError;
use crate::runtime::agent::native_config_import::sha256_hex;

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct ConfigExportResponse {
    toml: String,
    /// SHA-256 of `toml`; `POST /v1/config/import` takes it quoted as an `If-Match` entity tag.
    revision: String,
}

pub(crate) async fn config_export_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<ConfigExportResponse>, StackError> {
    let toml = on_disk_canonical_toml(&state)?;
    let revision = sha256_hex(toml.as_bytes());
    Ok(ApiSuccess::new(ConfigExportResponse { toml, revision }))
}

fn on_disk_canonical_toml(state: &AppState) -> crate::error::Result<String> {
    crate::config::Config::load_from_path(&state.runtime_paths.config_path)?.to_canonical_toml()
}

/// Whether an RFC 9110 `If-Match` holds for `revision`: `*`, or a list of one or more entity
/// tags naming it as a strong tag. A weak tag never matches, empty list elements are skipped
/// (RFC 9110 section 5.6.1.2), and a value outside the grammar is refused.
fn if_match_holds(header: &HeaderValue, revision: &str) -> crate::error::Result<bool> {
    let malformed = || StackError::InvalidParam {
        field: "If-Match",
        reason: "must be `*` or a comma-separated list of quoted entity tags".to_owned(),
    };
    let value = header.as_bytes().trim_ascii();
    if value == b"*" {
        return Ok(true);
    }
    let mut rest = value;
    let mut tags = 0;
    let mut holds = false;
    loop {
        while let [b' ' | b'\t' | b',', tail @ ..] = rest {
            rest = tail;
        }
        if rest.is_empty() {
            break;
        }
        let (weak, tag) = match rest.strip_prefix(b"W/") {
            Some(tag) => (true, tag),
            None => (false, rest),
        };
        let quoted = tag.strip_prefix(b"\"").ok_or_else(malformed)?;
        let end = quoted
            .iter()
            .position(|byte| *byte == b'"')
            .ok_or_else(malformed)?;
        let opaque = &quoted[..end];
        if opaque.iter().any(|byte| matches!(byte, b' ' | b'\t')) {
            return Err(malformed());
        }
        holds |= !weak && opaque == revision.as_bytes();
        tags += 1;
        rest = quoted[end + 1..].trim_ascii_start();
        if !matches!(rest, [] | [b',', ..]) {
            return Err(malformed());
        }
    }
    if tags == 0 {
        return Err(malformed());
    }
    Ok(holds)
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct ConfigValidateResponse {
    valid: bool,
}

/// POST /v1/config/validate accepts the canonical TOML in the raw request
/// body (any content type). Returning `{valid:true}` on parse + validate
/// success matches the read-only contract; on failure the standard envelope
/// surfaces the underlying `config.invalid` (or related) code.
pub(crate) async fn config_validate_handler(
    body: String,
) -> std::result::Result<ApiSuccess<ConfigValidateResponse>, StackError> {
    crate::config::load_config_from_str(&body)?;
    Ok(ApiSuccess::new(ConfigValidateResponse { valid: true }))
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct ConfigImportQuery {
    #[serde(default)]
    dry_run: bool,
}

/// POST /v1/config/import (admin-tier). Parses TOML from the raw body,
/// atomically writes the canonical form to the default config path, and records
/// a `server.config_imported` audit event.
/// Most of the running daemon retains its old `AppState`; the client must
/// restart the daemon for general config changes to take effect. Local
/// session-tier auth and `[logging].acp_trace` are the exceptions because they
/// are runtime gates that this handler updates immediately after the canonical
/// config write succeeds.
///
/// Query params:
/// - `dry_run=true`: validates, canonicalizes, and reports metadata without
///   writing or auditing.
///
/// An `If-Match` header naming an exported `revision` as an entity tag refuses the write
/// when the on-disk config changed since that export.
pub(crate) async fn config_import_handler(
    State(state): State<AppState>,
    query: Query<ConfigImportQuery>,
    headers: HeaderMap,
    body: String,
) -> std::result::Result<ApiSuccess<Value>, StackError> {
    let input_size = body.len();
    if input_size > crate::config::IMPORT_SIZE_LIMIT {
        return Err(StackError::ImportTooLarge {
            limit: crate::config::IMPORT_SIZE_LIMIT,
            actual: input_size,
        });
    }

    let incoming = crate::config::load_config_from_str(&body)?;
    let canonical = incoming.to_canonical_toml()?;
    let target = state.runtime_paths.config_path.clone();

    if query.dry_run {
        return Ok(ApiSuccess::new(serde_json::json!({
            "dry_run": true,
            "config_version": incoming.config_version,
            "canonical_toml_size": canonical.len(),
            "input_size": input_size,
            "target": target.to_string_lossy(),
            "target_exists": target.exists(),
        })));
    }

    let _mutation = state.lock_agent_config_mutation().await?;
    // Read under the config mutation lock, so no writer holding it lands between the check
    // and the write.
    if let Some(if_match) = headers.get(IF_MATCH) {
        let revision = sha256_hex(on_disk_canonical_toml(&state)?.as_bytes());
        if !if_match_holds(if_match, &revision)? {
            return Err(StackError::ConfigRevisionMismatch);
        }
    }

    if let Some(parent) = target.parent() {
        crate::fs_util::create_dir_owner_only(parent)?;
    }
    crate::fs_util::atomic_write_owner_only(&target, canonical.as_bytes())?;
    state
        .set_local_session_auth(incoming.local.session_auth)
        .await;
    crate::runtime::agent::acp_trace::set_enabled(incoming.logging.acp_trace);

    let payload = serde_json::json!({
        "path": target.to_string_lossy(),
        "size_bytes": canonical.len(),
        "local_session_auth": incoming.local.session_auth,
    });
    match serde_json::to_string(&payload) {
        Ok(payload_text) => {
            let recorded = state
                .state
                .write(move |store| {
                    store.append_event_with_source(
                        "info",
                        "server.config_imported",
                        crate::state::EVENT_SOURCE_API,
                        "config imported via /v1/config/import",
                        &payload_text,
                    )?;
                    Ok(())
                })
                .await;
            if let Err(err) = recorded {
                tracing::warn!(
                    error = %crate::error::report(&err),
                    "failed to record server.config_imported audit event"
                );
            }
        }
        Err(err) => {
            tracing::warn!(error = %err, "failed to serialize config-import audit payload");
        }
    }

    Ok(ApiSuccess::new(serde_json::json!({
        "imported": true,
        "local_session_auth": incoming.local.session_auth,
        "restart_required": true,
    })))
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct SecretsListResponse {
    names: Vec<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub(crate) struct SecretsSetBody {
    name: String,
    value: String,
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct SecretsSetResponse {
    name: String,
    action: &'static str,
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct SecretsDeleteResponse {
    name: String,
    deleted: bool,
}

pub(crate) async fn secrets_list_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<SecretsListResponse>, StackError> {
    let home = state.runtime_paths.home.clone();
    let store = crate::secrets::SecretStore::open(&home)?;
    let names = store.list_names().iter().map(|s| (*s).to_owned()).collect();
    Ok(ApiSuccess::new(SecretsListResponse { names }))
}

pub(crate) async fn secrets_set_handler(
    State(state): State<AppState>,
    Json(body): Json<SecretsSetBody>,
) -> std::result::Result<ApiSuccess<SecretsSetResponse>, StackError> {
    crate::secrets::reject_auth_ref_mutation(&body.name)?;
    // Serialize with other secret-store writers: the store is a whole-file read-modify-write, so
    // an unlocked writer silently drops a concurrent mutation.
    let _mutation = state.lock_agent_config_mutation().await?;
    let home = state.runtime_paths.home.clone();
    let mut store = crate::secrets::SecretStore::open(&home)?;
    let action = if store.contains(&body.name) {
        "updated"
    } else {
        "set"
    };
    store.set(&body.name, &body.value)?;
    Ok(ApiSuccess::new(SecretsSetResponse {
        name: body.name,
        action,
    }))
}

pub(crate) async fn secrets_delete_handler(
    Path(name): Path<String>,
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<SecretsDeleteResponse>, StackError> {
    crate::secrets::reject_auth_ref_mutation(&name)?;
    let _mutation = state.lock_agent_config_mutation().await?;
    let home = state.runtime_paths.home.clone();
    let mut store = crate::secrets::SecretStore::open(&home)?;
    store.delete(&name)?;
    Ok(ApiSuccess::new(SecretsDeleteResponse {
        name,
        deleted: true,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const REVISION: &str = "5e2b";

    fn holds(value: &str) -> crate::error::Result<bool> {
        if_match_holds(&HeaderValue::from_str(value).expect("header"), REVISION)
    }

    #[test]
    fn if_match_takes_the_revision_as_a_strong_entity_tag() {
        assert!(holds("\"5e2b\"").expect("parses"));
        assert!(holds("\"stale\", \"5e2b\"").expect("parses"));
        assert!(holds("\"5e2b\",").expect("parses"));
        assert!(holds("*").expect("parses"));
        assert!(!holds("\"stale\"").expect("parses"));
        assert!(!holds("W/\"5e2b\"").expect("parses"));
    }

    #[test]
    fn if_match_outside_the_grammar_is_refused() {
        for value in ["5e2b", "\"5e2b", "\"5e2b\" \"stale\"", ",", "*, \"5e2b\""] {
            assert!(
                matches!(
                    holds(value),
                    Err(StackError::InvalidParam {
                        field: "If-Match",
                        ..
                    })
                ),
                "{value}"
            );
        }
    }
}
