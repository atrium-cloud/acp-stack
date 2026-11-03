//! The managed-state extension contract: DTOs and apply orchestration for
//! `POST /v1/admin/extensions/{name}/apply`. Revision and ownership semantics live in
//! [`SecretStore`] so no endpoint can bypass them.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::error::{Result, StackError};
use crate::secrets::{ManagedCredentialSelection, SecretStore};

// CONSTANTS

/// Request wire schema this seam enforces.
pub const MANAGED_STATE_SCHEMA_VERSION: u16 = 1;

/// Each `desired` kind is also the `capability` value an extension declares;
/// a namespace accepts only the kind its declaration names.
pub const KIND_PROVIDER_CREDENTIAL: &str = "provider-credential";
pub const KIND_SECRET_ENVIRONMENT: &str = "secret-environment";

const MAX_PROVIDER_ID_BYTES: usize = 128;
const MAX_VALUE_COUNT: usize = 8;
const MAX_ENV_NAME_BYTES: usize = 128;
const MAX_VALUE_BYTES: usize = 16 * 1024;
const MAX_ENVIRONMENT_ENTRY_COUNT: usize = 128;
const ENVIRONMENT_FIELD: &str = "desired.entries";

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApplyRequest {
    #[schemars(range(min = 1, max = 1))]
    pub schema_version: u16,
    #[schemars(range(min = 1))]
    pub revision: i64,
    pub desired: DesiredState,
}

/// The desired payload, discriminated by `kind`. Kinds are additive; unknown
/// kinds fail deserialization.
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
    /// Workload env var names, each carrying the value of a secret-store ref.
    /// Refs resolve at every workload spawn (the agent, its terminals, and
    /// mediated commands); an empty map clears the namespace.
    SecretEnvironment { entries: BTreeMap<String, String> },
}

/// Force `selection` into each variant's `required` list. `selection` is an
/// `Option`, so schemars omits it from `required`; but the deserializer treats
/// an absent key as a parse error (a missing key must not read as a destructive
/// clear). The field schema is left untouched, staying `CredentialSelection |
/// null`, so `selection: null` remains valid while omitting the key does not.
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
            // Env names and ref names are not secret.
            Self::SecretEnvironment { entries } => f
                .debug_struct("SecretEnvironment")
                .field("entries", entries)
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

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ApplyResponse {
    pub applied_revision: i64,
    #[schemars(extend("enum" = ["applied", "cleared", "noop"]))]
    pub outcome: &'static str,
}

/// Fill the managed-state secret environment beneath a workload spawn's own env, so an
/// explicit value for a name wins; the caller layers the network provider's declaration
/// after this, which still wins over both.
pub fn layer_environment(env: &mut HashMap<String, String>, home: &Path) -> Result<()> {
    for (name, value) in crate::secrets::managed_environment_for_home(home)? {
        env.entry(name).or_insert(value);
    }
    Ok(())
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
    let outcome = match request.desired {
        DesiredState::ProviderCredential { selection } => {
            require_declared_kind(config, namespace, KIND_PROVIDER_CREDENTIAL)?;
            let selection = selection
                .map(|selection| resolve_selection(store, config, selection))
                .transpose()?;
            store.apply_managed_state_credential(
                namespace,
                KIND_PROVIDER_CREDENTIAL,
                request.revision,
                selection,
            )?
        }
        DesiredState::SecretEnvironment { entries } => {
            require_declared_kind(config, namespace, KIND_SECRET_ENVIRONMENT)?;
            validate_environment(store, config, namespace, &entries)?;
            store.apply_managed_state_environment(
                namespace,
                KIND_SECRET_ENVIRONMENT,
                request.revision,
                entries,
            )?
        }
    };
    Ok(ApplyResponse {
        applied_revision: request.revision,
        outcome: outcome.as_str(),
    })
}

/// Deposit flat secrets and apply a managed-state credential selection in one transaction. Mirrors
/// [`apply`], but the deposited secrets and the catalog swap commit together, so a validation failure
/// (stale revision, ownership conflict, invalid selection) leaves the store untouched rather than
/// orphaning the secrets a bare `set_many` would already have written. `source_refs` still resolve
/// against the flat store, so a selection may reference a secret this same body deposits.
pub fn deposit_and_apply<'a, I>(
    store: &mut SecretStore,
    config: &Config,
    namespace: &str,
    secrets: I,
    request: ApplyRequest,
) -> Result<ApplyResponse>
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    if request.schema_version != MANAGED_STATE_SCHEMA_VERSION {
        return Err(StackError::InvalidParam {
            field: "schema_version",
            reason: format!(
                "unsupported schema version {}; expected {MANAGED_STATE_SCHEMA_VERSION}",
                request.schema_version
            ),
        });
    }
    let revision = request.revision;
    let DesiredState::ProviderCredential { selection } = request.desired else {
        return Err(StackError::InvalidParam {
            field: "desired.kind",
            reason: format!("a credential deposit applies `{KIND_PROVIDER_CREDENTIAL}` only"),
        });
    };
    require_declared_kind(config, namespace, KIND_PROVIDER_CREDENTIAL)?;
    let outcome = store.deposit_and_apply_managed_credential(
        secrets,
        namespace,
        KIND_PROVIDER_CREDENTIAL,
        revision,
        |store| {
            selection
                .map(|selection| resolve_selection(store, config, selection))
                .transpose()
        },
    )?;
    Ok(ApplyResponse {
        applied_revision: revision,
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
/// revision and conflicts (409) instead of no-oping, because the effective
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
        let value = store
            .get(ref_name)
            .map_err(|error| source_ref_error("desired.selection.source_refs", ref_name, error))?;
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
    Ok(ManagedCredentialSelection {
        provider_id: selection.provider_id,
        values,
        source_refs: selection.source_refs,
    })
}

/// A missing ref is the caller's input error; any other store error propagates as itself.
fn source_ref_error(field: &'static str, ref_name: &str, error: StackError) -> StackError {
    match error {
        StackError::SecretNotFound { .. } => StackError::InvalidParam {
            field,
            reason: format!("secret ref `{ref_name}` is not in the secret store"),
        },
        other => other,
    }
}

/// The namespace must be declared with the capability matching the desired kind, so each
/// namespace keeps one kind of record.
fn require_declared_kind(config: &Config, namespace: &str, kind: &str) -> Result<()> {
    let declared = config
        .extensions
        .get(namespace)
        .and_then(|extension| extension.capability.as_deref());
    if declared == Some(kind) {
        return Ok(());
    }
    Err(StackError::InvalidParam {
        field: "desired.kind",
        reason: format!(
            "extension `{namespace}` declares capability `{}`, not `{kind}`",
            declared.unwrap_or_default()
        ),
    })
}

/// Bound-check the entries, require every ref in the store now (each spawn needs it too),
/// and refuse a name the runtime or another namespace writes. A name another source also
/// sets is left to the spawn precedence, so the apply never depends on agent config.
fn validate_environment(
    store: &SecretStore,
    config: &Config,
    namespace: &str,
    entries: &BTreeMap<String, String>,
) -> Result<()> {
    let invalid = |reason: String| StackError::InvalidParam {
        field: ENVIRONMENT_FIELD,
        reason,
    };
    if entries.len() > MAX_ENVIRONMENT_ENTRY_COUNT {
        return Err(invalid(format!(
            "entry count exceeds the {MAX_ENVIRONMENT_ENTRY_COUNT}-entry limit"
        )));
    }
    for (name, ref_name) in entries {
        for value in [name, ref_name] {
            validate_bounded(ENVIRONMENT_FIELD, value, MAX_ENV_NAME_BYTES)?;
            if !crate::config::is_valid_secret_ref_name(value) {
                return Err(invalid(format!(
                    "`{value}` must match [A-Za-z_][A-Za-z0-9_]*"
                )));
            }
        }
        if let Some(reason) = crate::config::runtime_written_conflict(config, name) {
            return Err(invalid(format!("`{name}` cannot be set: {reason}")));
        }
        if let Some((holder, _)) = store
            .managed_state()
            .iter()
            .find(|(other, record)| *other != namespace && record.environment.contains_key(name))
        {
            return Err(invalid(format!(
                "`{name}` cannot be set: managed-state namespace `{holder}` already sets it"
            )));
        }
        store
            .get(ref_name)
            .map_err(|error| source_ref_error(ENVIRONMENT_FIELD, ref_name, error))?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ExtensionConfig, ExtensionType, SandboxProviderStderr};

    const NAMESPACE: &str = "connections";

    fn extension(extension_type: ExtensionType, capability: Option<&str>) -> ExtensionConfig {
        ExtensionConfig {
            extension_type,
            provider: Vec::new(),
            provider_timeout: None,
            provider_stderr: SandboxProviderStderr::default(),
            workload_env: Default::default(),
            capability: capability.map(str::to_owned),
        }
    }

    /// A config declaring the environment namespace beside a network provider that sets the
    /// proxy and CA names, as the orchestrator's declaration does.
    fn config() -> Config {
        let mut config = crate::config::load_config_from_str(include_str!(
            "../../tests/fixtures/valid-opencode-stack.toml"
        ))
        .expect("fixture parses");
        config.extensions.insert(
            NAMESPACE.to_owned(),
            extension(ExtensionType::ManagedState, Some(KIND_SECRET_ENVIRONMENT)),
        );
        let mut egress = extension(ExtensionType::NetworkProvider, None);
        egress.workload_env.insert(
            "SSL_CERT_FILE".to_owned(),
            "/etc/ssl/certs/bundle.crt".to_owned(),
        );
        config.extensions.insert("egress".to_owned(), egress);
        config
    }

    fn request(entries: &[(&str, &str)]) -> ApplyRequest {
        ApplyRequest {
            schema_version: MANAGED_STATE_SCHEMA_VERSION,
            revision: 1,
            desired: DesiredState::SecretEnvironment {
                entries: entries
                    .iter()
                    .map(|(name, secret_ref)| ((*name).to_owned(), (*secret_ref).to_owned()))
                    .collect(),
            },
        }
    }

    #[test]
    fn a_secret_environment_applies_when_every_ref_is_stored() {
        let home = tempfile::tempdir().expect("home");
        let mut store = SecretStore::open_or_create(home.path()).expect("store");
        store
            .set("SEARCH_REF", "atc1.key.payload")
            .expect("deposit");
        // A provider credential variable is left to the spawn precedence: the
        // selected provider's value wins in the agent process.
        let response = apply(
            &mut store,
            &config(),
            NAMESPACE,
            request(&[("SEARCH_API_KEY", "SEARCH_REF"), ("HF_TOKEN", "SEARCH_REF")]),
        )
        .expect("apply");
        assert_eq!(response.outcome, "applied");
    }

    #[test]
    fn a_secret_environment_refuses_names_other_sources_set_and_missing_refs() {
        let home = tempfile::tempdir().expect("home");
        let mut store = SecretStore::open_or_create(home.path()).expect("store");
        store
            .set("SEARCH_REF", "atc1.key.payload")
            .expect("deposit");
        for (entries, reason) in [
            (
                &[("SSL_CERT_FILE", "SEARCH_REF")][..],
                "network-provider extension `egress`",
            ),
            (&[("HOME", "SEARCH_REF")][..], "the runtime sets it"),
            (
                &[("SEARCH_API_KEY", "MISSING_REF")][..],
                "not in the secret store",
            ),
            (&[("search-key", "SEARCH_REF")][..], "must match"),
        ] {
            let error =
                apply(&mut store, &config(), NAMESPACE, request(entries)).expect_err("refused");
            assert_eq!(error.error_code(), "request.invalid_param", "{entries:?}");
            assert!(
                error.public_message().contains(reason),
                "{entries:?}: {error}"
            );
        }
        assert!(store.managed_state_record(NAMESPACE).is_none());
    }

    #[test]
    fn a_namespace_takes_only_the_kind_it_declares() {
        let home = tempfile::tempdir().expect("home");
        let mut store = SecretStore::open_or_create(home.path()).expect("store");
        let mut config = config();
        config.extensions.insert(
            NAMESPACE.to_owned(),
            extension(ExtensionType::ManagedState, Some(KIND_PROVIDER_CREDENTIAL)),
        );
        let error = apply(&mut store, &config, NAMESPACE, request(&[])).expect_err("kind mismatch");
        assert!(
            error
                .public_message()
                .contains("declares capability `provider-credential`")
        );
    }

    #[test]
    fn source_ref_error_maps_not_found_to_invalid_param() {
        let error = source_ref_error(
            "desired.selection.source_refs",
            "OPENAI_KEY",
            StackError::SecretNotFound {
                name: "OPENAI_KEY".to_owned(),
            },
        );
        assert_eq!(error.error_code(), "request.invalid_param");
        assert!(
            error
                .public_message()
                .contains("secret ref `OPENAI_KEY` is not in the secret store")
        );
    }

    #[test]
    fn source_ref_error_passes_store_errors_through() {
        let error = source_ref_error(
            "desired.selection.source_refs",
            "OPENAI_KEY",
            StackError::SecretStoreRead {
                path: std::path::PathBuf::from("/state/secrets.age"),
                source: std::io::ErrorKind::PermissionDenied.into(),
            },
        );
        assert_eq!(error.error_code(), "secrets.read_failed");
    }
}
