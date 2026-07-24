//! The managed-state extension contract.
//!
//! A `type = "managed-state"` instance grants an external orchestrator
//! ownership of one named state namespace, applied through the fixed admin
//! endpoint `POST /v1/admin/extensions/{name}/apply`. The request carries a
//! monotonically increasing registry revision and a `desired` payload limited
//! to concepts acp-stack already models generically; revision semantics
//! (idempotent replay, stale rejection) and ownership enforcement live in the
//! secret store so no endpoint can bypass them.
//!
//! This module is transport-free: DTOs plus the apply orchestration against
//! [`SecretStore`], unit-testable without the HTTP layer.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::{Result, StackError};
use crate::secrets::{ManagedCredentialSelection, SecretStore};

// CONSTANTS

/// Request wire schema this seam enforces.
pub const MANAGED_STATE_SCHEMA_VERSION: u16 = 1;

/// The only `desired` kind today; also the required `capability` value on the
/// extension declaration.
pub const KIND_PROVIDER_CREDENTIAL: &str = "provider-credential";

const MAX_PROVIDER_ID_BYTES: usize = 128;
const MAX_VALUE_COUNT: usize = 8;
const MAX_ENV_NAME_BYTES: usize = 128;
const MAX_VALUE_BYTES: usize = 16 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplyRequest {
    pub schema_version: u16,
    pub revision: i64,
    pub desired: DesiredState,
}

/// The desired payload, discriminated by `kind`. A second kind later is an
/// additive change; unknown kinds fail deserialization.
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum DesiredState {
    ProviderCredential {
        // `selection` is a required key that may be null: silently defaulting
        // an absent key to `None` would read a malformed body as a
        // destructive clear. The `deserialize_with` marker removes serde's
        // implicit Option default so a missing key is a parse error instead.
        #[serde(deserialize_with = "deserialize_required_selection")]
        selection: Option<CredentialSelection>,
    },
}

impl std::fmt::Debug for DesiredState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ProviderCredential { selection } => f
                .debug_struct("ProviderCredential")
                .field("selection", selection)
                .finish(),
        }
    }
}

fn deserialize_required_selection<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<CredentialSelection>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<CredentialSelection>::deserialize(deserializer)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialSelection {
    pub provider_id: String,
    /// Inline values keyed by env-var name.
    #[serde(default)]
    pub values: BTreeMap<String, String>,
    /// Secret-store refs keyed by env-var name; each resolves into `values`
    /// at apply time and the ref name is retained alongside the value.
    #[serde(default)]
    pub source_refs: BTreeMap<String, String>,
}

impl std::fmt::Debug for CredentialSelection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never leak values via Debug; env names and ref names are not secret.
        f.debug_struct("CredentialSelection")
            .field("provider_id", &self.provider_id)
            .field("env_names", &self.values.keys().collect::<Vec<_>>())
            .field("source_refs", &self.source_refs)
            .finish()
    }
}

#[derive(Debug, Serialize)]
pub struct ApplyResponse {
    pub applied_revision: i64,
    pub outcome: &'static str,
}

/// Validate the request and apply it to the store. The caller holds the
/// agent-config mutation lock; the store persists the catalog swap and the
/// namespace watermark atomically.
pub fn apply(
    store: &mut SecretStore,
    namespace: &str,
    request: ApplyRequest,
) -> Result<ApplyResponse> {
    if request.schema_version != MANAGED_STATE_SCHEMA_VERSION {
        return Err(StackError::InvalidParam {
            field: "schema_version",
            reason: format!(
                "unsupported schema version {}; expected {MANAGED_STATE_SCHEMA_VERSION}",
                request.schema_version
            ),
        });
    }
    let DesiredState::ProviderCredential { selection } = request.desired;
    let selection = selection
        .map(|selection| resolve_selection(store, selection))
        .transpose()?;
    let outcome = store.apply_managed_state_credential(
        namespace,
        KIND_PROVIDER_CREDENTIAL,
        request.revision,
        selection,
    )?;
    Ok(ApplyResponse {
        applied_revision: request.revision,
        outcome: outcome.as_str(),
    })
}

/// Bound-check the selection, resolve `source_refs` against the flat secret
/// store, and validate the merged env-keyed values against the provider's
/// canonical env-var contract.
///
/// Refs resolve at apply time, so a ref-backed selection is replay-stable
/// only while the referenced secrets are stable: if a ref rotates between an
/// apply and its retry, the retry compares as different content at the same
/// revision and conflicts (409) instead of no-oping — the effective
/// credential really did change, so the orchestrator must advance the
/// revision.
fn resolve_selection(
    store: &SecretStore,
    selection: CredentialSelection,
) -> Result<ManagedCredentialSelection> {
    validate_bounded(
        "desired.selection.provider_id",
        &selection.provider_id,
        MAX_PROVIDER_ID_BYTES,
    )?;
    if selection.values.is_empty() && selection.source_refs.is_empty() {
        return Err(StackError::InvalidParam {
            field: "desired.selection",
            reason: "a selection must carry at least one value or source ref".to_owned(),
        });
    }
    if selection.values.len() + selection.source_refs.len() > MAX_VALUE_COUNT {
        return Err(StackError::InvalidParam {
            field: "desired.selection",
            reason: format!("value count exceeds the {MAX_VALUE_COUNT}-entry limit"),
        });
    }
    for (name, value) in &selection.values {
        validate_bounded("desired.selection.values", name, MAX_ENV_NAME_BYTES)?;
        if value.is_empty() || value.len() > MAX_VALUE_BYTES {
            return Err(StackError::InvalidParam {
                field: "desired.selection.values",
                reason: format!(
                    "value for `{name}` must be non-empty and at most {MAX_VALUE_BYTES} bytes"
                ),
            });
        }
    }
    let mut values = selection.values;
    for (env_name, ref_name) in &selection.source_refs {
        validate_bounded(
            "desired.selection.source_refs",
            env_name,
            MAX_ENV_NAME_BYTES,
        )?;
        validate_bounded(
            "desired.selection.source_refs",
            ref_name,
            MAX_ENV_NAME_BYTES,
        )?;
        if values.contains_key(env_name) {
            return Err(StackError::InvalidParam {
                field: "desired.selection.source_refs",
                reason: format!("env var `{env_name}` carries both an inline value and a ref"),
            });
        }
        let value = store.get(ref_name).map_err(|_| StackError::InvalidParam {
            field: "desired.selection.source_refs",
            reason: format!("secret ref `{ref_name}` is not in the secret store"),
        })?;
        values.insert(env_name.clone(), value.to_owned());
    }
    crate::runtime::agent::provider_keys::validate_env_keyed_credential_values(
        &selection.provider_id,
        &values,
        "desired.selection.values",
    )?;
    Ok(ManagedCredentialSelection {
        provider_id: selection.provider_id,
        values,
        source_refs: selection.source_refs,
    })
}

fn validate_bounded(field: &'static str, value: &str, max_bytes: usize) -> Result<()> {
    if value.is_empty() {
        return Err(StackError::InvalidParam {
            field,
            reason: "value must not be empty".to_owned(),
        });
    }
    if value.len() > max_bytes {
        return Err(StackError::InvalidParam {
            field,
            reason: format!("value exceeds the {max_bytes}-byte limit"),
        });
    }
    Ok(())
}
