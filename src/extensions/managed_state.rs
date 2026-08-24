//! The managed-state extension contract: DTOs and apply orchestration for
//! `POST /v1/admin/extensions/{name}/apply`. Revision and ownership semantics live in
//! [`SecretStore`] so no endpoint can bypass them.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::error::{Result, StackError};
use crate::secrets::{CredentialSource, ManagedCredentialSelection, SecretStore};

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

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApplyRequest {
    #[schemars(range(min = 1, max = 1))]
    pub schema_version: u16,
    #[schemars(range(min = 1))]
    pub revision: i64,
    pub desired: DesiredState,
}

/// The desired payload, discriminated by `kind`. A second kind later is an
/// additive change; unknown kinds fail deserialization.
#[derive(Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
#[schemars(transform = require_selection_key)]
pub enum DesiredState {
    ProviderCredential {
        // `selection` is a required key that may be null: defaulting an absent key to `None`
        // would read a malformed body as a destructive clear, so a missing key must parse-error.
        #[serde(deserialize_with = "deserialize_required_selection")]
        selection: Option<CredentialSelection>,
    },
}

/// Force `selection` into each variant's `required` list. `selection` is an
/// `Option`, so schemars omits it from `required`; but the deserializer treats
/// an absent key as a parse error (a missing key must not read as a destructive
/// clear). The field schema is left untouched — it stays `CredentialSelection |
/// null` — so `selection: null` remains valid while omitting the key does not.
/// Applied per `oneOf` variant that actually has a `selection` property, so a
/// future `kind` without one is unaffected.
fn require_selection_key(schema: &mut schemars::Schema) {
    const SELECTION: &str = "selection";
    let Some(variants) = schema
        .ensure_object()
        .get_mut("oneOf")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return;
    };
    for variant in variants {
        let Some(variant) = variant.as_object_mut() else {
            continue;
        };
        let has_selection = variant
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .is_some_and(|properties| properties.contains_key(SELECTION));
        if !has_selection {
            continue;
        }
        if let serde_json::Value::Array(required) = variant
            .entry("required")
            .or_insert_with(|| serde_json::Value::Array(Vec::new()))
            && !required.iter().any(|field| field == SELECTION)
        {
            required.push(serde_json::Value::String(SELECTION.to_owned()));
        }
    }
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

#[derive(Deserialize, schemars::JsonSchema)]
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
    /// Endpoint base the agent must send this provider's traffic to instead of
    /// the vendor default. Written into the agent's own native config, so the
    /// configured agent must declare `set_provider_base_url` in the registry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

impl std::fmt::Debug for CredentialSelection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never leak values via Debug; env names, ref names, and the base URL are not secret.
        f.debug_struct("CredentialSelection")
            .field("provider_id", &self.provider_id)
            .field("env_names", &self.values.keys().collect::<Vec<_>>())
            .field("source_refs", &self.source_refs)
            .field("base_url", &self.base_url)
            .finish()
    }
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ApplyResponse {
    pub applied_revision: i64,
    #[schemars(extend("enum" = ["applied", "cleared", "noop"]))]
    pub outcome: &'static str,
}

/// Validate the request and apply it to the store. The caller holds the
/// agent-config mutation lock; the store persists the catalog swap and the
/// namespace watermark atomically.
pub fn apply(
    store: &mut SecretStore,
    config: &Config,
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
        .map(|selection| resolve_selection(store, config, selection))
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
/// env-var contract: the canonical mapping for registry providers, or the
/// configured `api_key_ref` for a custom provider declared in the agent
/// config.
///
/// Refs resolve at apply time, so a ref-backed selection is replay-stable
/// only while the referenced secrets are stable: if a ref rotates between an
/// apply and its retry, the retry compares as different content at the same
/// revision and conflicts (409) instead of no-oping — the effective
/// credential really did change, so the orchestrator must advance the
/// revision.
fn resolve_selection(
    store: &SecretStore,
    config: &Config,
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
    if crate::runtime::agent::provider_keys::env_var_for_provider_id(&selection.provider_id)
        .is_some()
    {
        crate::runtime::agent::provider_keys::validate_env_keyed_credential_values(
            &selection.provider_id,
            &values,
            "desired.selection.values",
        )?;
    } else if let Some(api_key_ref) =
        crate::runtime::agent::provider_keys::configured_custom_provider_api_key_ref(
            config,
            &selection.provider_id,
        )
    {
        crate::runtime::agent::provider_keys::validate_custom_provider_credential_values(
            &selection.provider_id,
            api_key_ref,
            &values,
            "desired.selection.values",
        )?;
    } else {
        // Rejecting before any watermark or catalog persist keeps the revision retryable.
        return Err(StackError::InvalidParam {
            field: "desired.selection.provider_id",
            reason: format!(
                "provider `{}` is neither a mapped provider nor a configured custom provider",
                selection.provider_id
            ),
        });
    }
    if let Some(base_url) = selection.base_url.as_deref() {
        validate_base_url(base_url)?;
        require_agent_supports_base_url(config)?;
        require_provider_accepts_base_url(config, &selection.provider_id)?;
        require_single_endpoint_override(store, &selection.provider_id)?;
    }
    Ok(ManagedCredentialSelection {
        provider_id: selection.provider_id,
        values,
        source_refs: selection.source_refs,
        base_url: selection.base_url,
    })
}

/// A provider endpoint base obeys the shared endpoint rule: https, or http to a
/// loopback host (an in-guest relay listener), no credentials, no query or
/// fragment, bounded. Stored verbatim — each agent module appends per its own
/// native convention.
fn validate_base_url(base_url: &str) -> Result<()> {
    use crate::config::{EndpointUrlProblem, MAX_ENDPOINT_URL_BYTES, check_endpoint_url};

    check_endpoint_url(base_url, false).map_err(|problem| StackError::InvalidParam {
        field: "desired.selection.base_url",
        reason: match problem {
            EndpointUrlProblem::Unparseable => "base_url is not a valid URL".to_owned(),
            EndpointUrlProblem::NotHttpsOrLoopback => {
                "base_url must be an https:// URL with a host, or http:// to a loopback host"
                    .to_owned()
            }
            EndpointUrlProblem::ContainsCredentials => {
                "base_url must not include credentials".to_owned()
            }
            EndpointUrlProblem::ContainsQueryOrFragment => {
                "base_url must not carry a query string or fragment".to_owned()
            }
            EndpointUrlProblem::TooLong => {
                format!("base_url exceeds the {MAX_ENDPOINT_URL_BYTES}-byte limit")
            }
        },
    })
}

/// Reject the endpoint override before any watermark or catalog persist when
/// the configured agent has no native config surface to write it into —
/// otherwise the revision applies and the endpoint silently never takes effect.
fn require_agent_supports_base_url(config: &Config) -> Result<()> {
    if crate::runtime::install::agent_supports_provider_base_url(&config.agent.id)? {
        return Ok(());
    }
    Err(StackError::InvalidParam {
        field: "desired.selection.base_url",
        reason: format!(
            "agent `{}` cannot route a provider through a custom endpoint; \
             remove base_url or switch to an agent that supports it",
            config.agent.id
        ),
    })
}

/// The registry endpoint capability is agent-level, but a few agent/provider
/// pairs still have nowhere to write the override. Rejecting here, before any
/// watermark or catalog persist, keeps the revision reusable — otherwise
/// provisioning fails once the store is already durable and every retry at the
/// same revision replays the same failure.
fn require_provider_accepts_base_url(config: &Config, provider_id: &str) -> Result<()> {
    if crate::runtime::agent::provider_keys::agent_provider_accepts_endpoint_override(
        &config.agent.id,
        provider_id,
    ) {
        return Ok(());
    }
    Err(StackError::InvalidParam {
        field: "desired.selection.base_url",
        reason: format!(
            "agent `{}` cannot route provider `{provider_id}` through a custom endpoint; \
             select `openrouter` or a configured custom provider, or remove base_url",
            config.agent.id
        ),
    })
}

/// At most one provider may be rerouted at a time: the agent's native config
/// carries exactly one endpoint override, and two namespaces each rerouting a
/// different provider would have provisioning arbitrarily pick a winner.
/// Rejecting here — before any watermark or catalog persist — keeps the
/// revision reusable once the first namespace's endpoint is cleared.
fn require_single_endpoint_override(store: &SecretStore, provider_id: &str) -> Result<()> {
    for (existing_id, set) in store.provider_credentials() {
        if existing_id == provider_id {
            continue;
        }
        let carries_override = set.sole.as_ref().is_some_and(|credential| {
            credential.base_url.is_some()
                && matches!(&credential.source, CredentialSource::External(_))
        });
        if carries_override {
            return Err(StackError::InvalidParam {
                field: "desired.selection.base_url",
                reason: format!(
                    "provider `{existing_id}` is already routed through a custom endpoint; only \
                     one provider may be rerouted at a time — clear that namespace's credential \
                     endpoint first"
                ),
            });
        }
    }
    Ok(())
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
