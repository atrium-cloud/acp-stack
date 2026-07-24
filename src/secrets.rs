//! Age-encrypted secret store.
//!
//! Layout on disk:
//!
//! - `~/.config/acp-stack/age.key` — the bech32-encoded x25519 identity
//!   produced by `age::x25519::Identity::to_string()`. One identity per
//!   instance. Owner-only (0600).
//! - `~/.local/share/acp-stack/secrets.age` — the age-encrypted ciphertext.
//!   Plaintext is TOML containing flat `[secrets]` and the structured mapped-
//!   provider credential catalog.
//!   The store is encrypted to its own public key (single-recipient).
//!
//! Inner format is TOML rather than JSON for consistency with the rest of
//! the project; the existing `toml` dependency already handles round-trip.
//!
//! The store is rewritten in full on every mutation; concurrency is not a
//! goal for 0.0.1 because `acps` runs as a single supervisor.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use age::secrecy::ExposeSecret;
use base64::Engine;
use rand::RngExt;
use serde::{Deserialize, Serialize};

use crate::error::{Result, StackError};
use crate::fs_util::{
    atomic_write_owner_only, parent_dir, set_owner_only_file, validate_owner_only_regular_file,
    write_new_file_owner_only,
};

/// Kept as a no-op guard at mutation call sites. Auth keys are stored as
/// non-recoverable state verifiers, so no secret-store name is reserved for auth.
pub fn reject_auth_ref_mutation(_name: &str) -> Result<()> {
    Ok(())
}

/// Runtime config directory: `~/.config/acp-stack/`. Parent of `age_key_path`
/// and `default_config_path`. Owner-only (0700). Exposed here so callers
/// (e.g. `acps security check`) reach for one helper instead of redoing the
/// `home.join(".config").join("acp-stack")` dance.
pub fn config_dir(home: &Path) -> PathBuf {
    home.join(".config").join("acp-stack")
}

/// Runtime state directory: `~/.local/share/acp-stack/`. Parent of
/// `secret_store_path` and `default_state_path`. Owner-only (0700).
pub fn state_dir(home: &Path) -> PathBuf {
    home.join(".local").join("share").join("acp-stack")
}

pub fn age_key_path(home: &Path) -> PathBuf {
    config_dir(home).join("age.key")
}

pub fn secret_store_path(home: &Path) -> PathBuf {
    state_dir(home).join("secrets.age")
}

/// Loaded, decrypted view of the secret store. Mutations are written through
/// to disk via `atomic_write_owner_only`; the in-memory copy and the
/// ciphertext on disk stay in sync per operation.
pub struct SecretStore {
    identity: age::x25519::Identity,
    secrets: BTreeMap<String, String>,
    provider_credentials: BTreeMap<String, ProviderCredentialSet>,
    managed_state: BTreeMap<String, ManagedStateRecord>,
    store_path: PathBuf,
}

impl fmt::Debug for SecretStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never leak the identity or secret values via Debug. List only the
        // names, which are already public (they appear in config refs).
        f.debug_struct("SecretStore")
            .field("identity", &"<redacted>")
            .field("store_path", &self.store_path)
            .field("secret_names", &self.list_names())
            .field(
                "provider_credential_ids",
                &self.provider_credentials.keys().collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// Provenance of a stored provider credential. `Operator` entries are written
/// by the CLI/import flows; `External` entries are owned by the named
/// managed-state extension namespace and applied through the admin apply
/// endpoint. Overwrite protection across the two owners is enforced by the
/// store itself, not by any one endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CredentialSource {
    #[default]
    Operator,
    External(String),
}

impl CredentialSource {
    pub fn is_operator(&self) -> bool {
        *self == CredentialSource::Operator
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderCredential {
    pub revision: String,
    pub values: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub source_refs: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub migrated: bool,
    /// Absent on disk for operator entries, so pre-provenance stores load
    /// unchanged and operator entries serialize byte-identically.
    #[serde(default, skip_serializing_if = "CredentialSource::is_operator")]
    pub source: CredentialSource,
}

impl fmt::Debug for ProviderCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderCredential")
            .field("revision", &"<redacted>")
            .field("env_names", &self.values.keys().collect::<Vec<_>>())
            .field("source_refs", &self.source_refs)
            .field("source", &self.source)
            .finish()
    }
}

impl ProviderCredential {
    pub fn new(values: BTreeMap<String, String>, source_refs: BTreeMap<String, String>) -> Self {
        Self {
            revision: new_provider_credential_revision(),
            values,
            source_refs,
            migrated: false,
            source: CredentialSource::Operator,
        }
    }

    pub fn rotate(
        &mut self,
        values: BTreeMap<String, String>,
        source_refs: BTreeMap<String, String>,
    ) {
        self.revision = new_provider_credential_revision();
        self.values = values;
        self.source_refs = source_refs;
        self.migrated = false;
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderCredentialSet {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sole: Option<ProviderCredential>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub aliases: BTreeMap<String, ProviderCredential>,
}

impl ProviderCredentialSet {
    pub fn aliasless(credential: ProviderCredential) -> Self {
        Self {
            sole: Some(credential),
            aliases: BTreeMap::new(),
        }
    }

    pub fn promoted(aliases: BTreeMap<String, ProviderCredential>) -> Self {
        Self {
            sole: None,
            aliases,
        }
    }

    pub fn is_promoted(&self) -> bool {
        self.sole.is_none()
    }

    pub fn selected(&self, alias: Option<&str>) -> Option<(&ProviderCredential, Option<&str>)> {
        match (&self.sole, alias) {
            (Some(credential), None) => Some((credential, None)),
            (None, Some(alias)) => self
                .aliases
                .get_key_value(alias)
                .map(|(stored_alias, credential)| (credential, Some(stored_alias.as_str()))),
            _ => None,
        }
    }

    fn validate(&self, provider_id: &str) -> Result<()> {
        match (&self.sole, self.aliases.is_empty()) {
            (Some(_), true) => {}
            (None, false) => {}
            _ => {
                return Err(StackError::SecretStorePlaintextInvalid {
                    reason: format!(
                        "provider credential `{provider_id}` must be aliasless or contain aliases"
                    ),
                });
            }
        }
        for (alias, credential) in &self.aliases {
            if !crate::config::is_valid_secret_ref_name(alias) {
                return Err(StackError::SecretStorePlaintextInvalid {
                    reason: format!(
                        "provider credential `{provider_id}` has invalid alias `{alias}`"
                    ),
                });
            }
            validate_provider_credential(provider_id, credential)?;
        }
        if let Some(credential) = &self.sole {
            validate_provider_credential(provider_id, credential)?;
        }
        Ok(())
    }
}

/// Durable per-namespace record of the last applied managed-state registry.
/// Written atomically together with the credential catalog so the applied
/// revision survives restarts and idempotent replays can be recognized.
/// `provider_id` is `None` after a clear so the revision watermark is retained
/// even with no credential stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedStateRecord {
    pub revision: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

/// The managed-state selection the store applies: env-keyed values (plus
/// optional secret refs) for one provider, exactly the shape
/// [`ProviderCredential`] already models.
#[derive(Clone, PartialEq, Eq)]
pub struct ManagedCredentialSelection {
    pub provider_id: String,
    pub values: BTreeMap<String, String>,
    pub source_refs: BTreeMap<String, String>,
}

impl fmt::Debug for ManagedCredentialSelection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never leak values via Debug; env names are not secret.
        f.debug_struct("ManagedCredentialSelection")
            .field("provider_id", &self.provider_id)
            .field("env_names", &self.values.keys().collect::<Vec<_>>())
            .field("source_refs", &self.source_refs)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedApplyOutcome {
    Applied,
    Cleared,
    Noop,
}

impl ManagedApplyOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::Cleared => "cleared",
            Self::Noop => "noop",
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StorePlaintext {
    #[serde(default)]
    secrets: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    provider_credentials: BTreeMap<String, ProviderCredentialSet>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    managed_state: BTreeMap<String, ManagedStateRecord>,
}

impl StorePlaintext {
    fn validate(&self) -> Result<()> {
        for (provider_id, credentials) in &self.provider_credentials {
            credentials.validate(provider_id)?;
        }
        for (namespace, record) in &self.managed_state {
            if record.revision <= 0 {
                return Err(StackError::SecretStorePlaintextInvalid {
                    reason: format!(
                        "managed-state record `{namespace}` must have a positive revision"
                    ),
                });
            }
        }
        Ok(())
    }
}

impl SecretStore {
    /// Open an existing store, or create an empty one if neither the age key
    /// nor the ciphertext exists yet. Either both files exist or neither
    /// does; an asymmetric state is corruption and is rejected before any
    /// generate/encrypt path runs.
    pub fn open_or_create(home: &Path) -> Result<Self> {
        ensure_dirs(home)?;
        let key_path = age_key_path(home);
        let store_path = secret_store_path(home);
        Self::open_or_create_at_paths(&key_path, &store_path)
    }

    pub fn open_or_create_at_paths(key_path: &Path, store_path: &Path) -> Result<Self> {
        match (key_path.exists(), store_path.exists()) {
            (true, false) => {
                return Err(StackError::AgeKeyParse {
                    path: key_path.to_path_buf(),
                    reason: "age key exists but secret store ciphertext is missing; \
                             run `acps reset --yes` and re-init to recover",
                });
            }
            (false, true) => {
                return Err(StackError::SecretStoreRead {
                    path: store_path.to_path_buf(),
                    source: std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "age key is missing; the encrypted secret store is unreadable. \
                         run `acps reset --yes` and re-init to recover",
                    ),
                });
            }
            _ => {}
        }

        let identity = if key_path.exists() {
            // Repair owner-only mode before reading. The age key decrypts every
            // stored API key; tolerating a world-readable identity from an
            // older binary or sloppy manual edit would expose all of them.
            set_owner_only_file(key_path)?;
            load_identity(key_path)?
        } else {
            generate_identity(key_path)?
        };

        let plaintext = if store_path.exists() {
            set_owner_only_file(store_path)?;
            decrypt_store(&identity, store_path)?
        } else {
            let plaintext = StorePlaintext::default();
            let ciphertext = encrypt_plaintext(&identity.to_public(), &plaintext)?;
            atomic_write_owner_only(store_path, &ciphertext)?;
            plaintext
        };

        Ok(Self {
            identity,
            secrets: plaintext.secrets,
            provider_credentials: plaintext.provider_credentials,
            managed_state: plaintext.managed_state,
            store_path: store_path.to_path_buf(),
        })
    }

    /// Open an existing store. Fails if the age key or the ciphertext is
    /// missing. Use this when you expect a previously-initialized instance.
    pub fn open(home: &Path) -> Result<Self> {
        let key_path = age_key_path(home);
        let store_path = secret_store_path(home);
        Self::open_at_paths(&key_path, &store_path)
    }

    /// Open the existing store without repairing permissions. Native-config
    /// import uses this before restart blockers clear so validation cannot
    /// mutate any live runtime path.
    pub fn open_read_only(home: &Path) -> Result<Self> {
        let key_path = age_key_path(home);
        let store_path = secret_store_path(home);
        validate_owner_only_regular_file(&key_path)?;
        validate_owner_only_regular_file(&store_path)?;
        let identity = load_identity(&key_path)?;
        let plaintext = decrypt_store(&identity, &store_path)?;
        Ok(Self {
            identity,
            secrets: plaintext.secrets,
            provider_credentials: plaintext.provider_credentials,
            managed_state: plaintext.managed_state,
            store_path,
        })
    }

    /// Open an existing store from explicit runtime-managed paths. The daemon
    /// uses this for health checks because tests and embedded runtimes can pass
    /// non-default `RuntimePaths` while still keeping the standard `age.key` /
    /// `secrets.age` filenames beside config and state.
    pub fn open_at_paths(key_path: &Path, store_path: &Path) -> Result<Self> {
        if key_path.exists() {
            set_owner_only_file(key_path)?;
        }
        let identity = load_identity(key_path)?;
        if store_path.exists() {
            set_owner_only_file(store_path)?;
        }
        let plaintext = decrypt_store(&identity, store_path)?;

        Ok(Self {
            identity,
            secrets: plaintext.secrets,
            provider_credentials: plaintext.provider_credentials,
            managed_state: plaintext.managed_state,
            store_path: store_path.to_path_buf(),
        })
    }

    pub fn store_path(&self) -> &Path {
        &self.store_path
    }

    pub fn contains(&self, name: &str) -> bool {
        self.secrets.contains_key(name)
    }

    pub fn get(&self, name: &str) -> Result<&str> {
        self.secrets
            .get(name)
            .map(String::as_str)
            .ok_or_else(|| StackError::SecretNotFound {
                name: name.to_owned(),
            })
    }

    pub fn list_names(&self) -> Vec<&str> {
        self.secrets.keys().map(String::as_str).collect()
    }

    pub fn provider_credentials(&self) -> &BTreeMap<String, ProviderCredentialSet> {
        &self.provider_credentials
    }

    pub fn provider_credential_set(&self, provider_id: &str) -> Option<&ProviderCredentialSet> {
        self.provider_credentials.get(provider_id)
    }

    pub(crate) fn stage_provider_credentials(
        &mut self,
        provider_credentials: BTreeMap<String, ProviderCredentialSet>,
    ) -> Result<()> {
        self.ensure_external_entries_untouched(&provider_credentials)?;
        StorePlaintext {
            secrets: self.secrets.clone(),
            provider_credentials: provider_credentials.clone(),
            managed_state: self.managed_state.clone(),
        }
        .validate()?;
        self.provider_credentials = provider_credentials;
        Ok(())
    }

    pub fn replace_provider_credentials(
        &mut self,
        provider_credentials: BTreeMap<String, ProviderCredentialSet>,
        remove_flat_secrets: &[String],
    ) -> Result<()> {
        self.ensure_external_entries_untouched(&provider_credentials)?;
        let mut secrets = self.secrets.clone();
        for name in remove_flat_secrets {
            secrets.remove(name);
        }
        let plaintext = StorePlaintext {
            secrets: secrets.clone(),
            provider_credentials: provider_credentials.clone(),
            managed_state: self.managed_state.clone(),
        };
        plaintext.validate()?;
        let ciphertext = encrypt_plaintext(&self.identity.to_public(), &plaintext)?;
        atomic_write_owner_only(&self.store_path, &ciphertext)?;
        self.secrets = secrets;
        self.provider_credentials = provider_credentials;
        Ok(())
    }

    /// Operator-path mutations must not clobber externally-owned entries: an
    /// external orchestrator applied them under its own revision watermark,
    /// and a silent operator overwrite would diverge the watermark from the
    /// stored credential. External entries can only change through
    /// [`Self::apply_managed_state_credential`].
    ///
    /// Ownership checks here and in the apply path inspect `sole` only, which
    /// is sound because an external credential is always stored aliasless and
    /// this very guard blocks the one flow (alias promotion) that could
    /// relocate it into `aliases`. Any future path that stores an External
    /// credential under an alias would bypass ownership and must not exist.
    fn ensure_external_entries_untouched(
        &self,
        replacement: &BTreeMap<String, ProviderCredentialSet>,
    ) -> Result<()> {
        for (provider_id, existing) in &self.provider_credentials {
            let Some(CredentialSource::External(namespace)) =
                existing.sole.as_ref().map(|credential| &credential.source)
            else {
                continue;
            };
            if replacement.get(provider_id) != Some(existing) {
                return Err(StackError::ExtensionStateOwnership {
                    namespace: namespace.clone(),
                    provider_id: provider_id.clone(),
                    reason: "the credential is owned by a managed-state extension; apply a new \
                             registry revision through the extension instead"
                        .to_owned(),
                });
            }
        }
        Ok(())
    }

    pub fn managed_state_record(&self, namespace: &str) -> Option<&ManagedStateRecord> {
        self.managed_state.get(namespace)
    }

    pub fn managed_state(&self) -> &BTreeMap<String, ManagedStateRecord> {
        &self.managed_state
    }

    /// Apply one managed-state registry revision for `namespace`: idempotent
    /// replay at the applied revision, stale-revision rejection, and an atomic
    /// catalog-swap + watermark persist. `selection: None` clears the
    /// namespace's credential while retaining the watermark. Ownership is
    /// enforced here: the namespace may only create entries or replace its own.
    pub fn apply_managed_state_credential(
        &mut self,
        namespace: &str,
        kind: &str,
        revision: i64,
        selection: Option<ManagedCredentialSelection>,
    ) -> Result<ManagedApplyOutcome> {
        if revision <= 0 {
            return Err(StackError::InvalidParam {
                field: "revision",
                reason: "revision must be a positive integer".to_owned(),
            });
        }
        let record = self.managed_state.get(namespace);
        match record {
            Some(record) if revision == record.revision => {
                self.ensure_identical_replay(namespace, kind, record, selection.as_ref())?;
                return Ok(ManagedApplyOutcome::Noop);
            }
            Some(record) if revision < record.revision => {
                return Err(StackError::ExtensionRevisionConflict {
                    namespace: namespace.to_owned(),
                    reason: format!(
                        "revision {revision} is stale; revision {} is already applied",
                        record.revision
                    ),
                });
            }
            _ => {}
        }

        let mut catalog = self.provider_credentials.clone();
        if let Some(previous_provider) = record.and_then(|record| record.provider_id.as_deref()) {
            catalog.remove(previous_provider);
        }
        let (new_record, outcome) = match selection {
            None => (
                ManagedStateRecord {
                    revision,
                    provider_id: None,
                    kind: Some(kind.to_owned()),
                },
                ManagedApplyOutcome::Cleared,
            ),
            Some(selection) => {
                if let Some(existing) = catalog.get(&selection.provider_id) {
                    let owned_by_namespace = existing.sole.as_ref().is_some_and(|credential| {
                        credential.source == CredentialSource::External(namespace.to_owned())
                    });
                    if !owned_by_namespace {
                        return Err(StackError::ExtensionStateOwnership {
                            namespace: namespace.to_owned(),
                            provider_id: selection.provider_id.clone(),
                            reason: "the provider already has a credential not owned by this \
                                     namespace; refusing to overwrite it"
                                .to_owned(),
                        });
                    }
                }
                let credential = ProviderCredential {
                    revision: format!("managed:{namespace}:{revision}"),
                    values: selection.values,
                    source_refs: selection.source_refs,
                    migrated: false,
                    source: CredentialSource::External(namespace.to_owned()),
                };
                catalog.insert(
                    selection.provider_id.clone(),
                    ProviderCredentialSet::aliasless(credential),
                );
                (
                    ManagedStateRecord {
                        revision,
                        provider_id: Some(selection.provider_id),
                        kind: Some(kind.to_owned()),
                    },
                    ManagedApplyOutcome::Applied,
                )
            }
        };

        let mut managed_state = self.managed_state.clone();
        managed_state.insert(namespace.to_owned(), new_record);
        let plaintext = StorePlaintext {
            secrets: self.secrets.clone(),
            provider_credentials: catalog.clone(),
            managed_state: managed_state.clone(),
        };
        plaintext.validate()?;
        let ciphertext = encrypt_plaintext(&self.identity.to_public(), &plaintext)?;
        atomic_write_owner_only(&self.store_path, &ciphertext)?;
        self.provider_credentials = catalog;
        self.managed_state = managed_state;
        Ok(outcome)
    }

    /// A replay at the already-applied revision must be an exact no-op: the
    /// orchestrator retries on any failure, so the retried body is expected to
    /// be identical in meaning. Anything else at that revision is a conflict.
    fn ensure_identical_replay(
        &self,
        namespace: &str,
        kind: &str,
        record: &ManagedStateRecord,
        selection: Option<&ManagedCredentialSelection>,
    ) -> Result<()> {
        let conflict = |reason: String| StackError::ExtensionRevisionConflict {
            namespace: namespace.to_owned(),
            reason: format!(
                "revision {} is already applied with different content: {reason}",
                record.revision
            ),
        };
        if record.kind.as_deref() != Some(kind) {
            return Err(conflict("desired kind differs".to_owned()));
        }
        match (selection, record.provider_id.as_deref()) {
            (None, None) => Ok(()),
            (Some(selection), Some(provider_id)) if provider_id == selection.provider_id => {
                let credential = self
                    .provider_credentials
                    .get(provider_id)
                    .and_then(|set| set.sole.as_ref())
                    .ok_or_else(|| {
                        conflict("stored credential for the applied provider is missing".to_owned())
                    })?;
                if credential.source != CredentialSource::External(namespace.to_owned()) {
                    return Err(conflict(
                        "stored credential is not owned by this namespace".to_owned(),
                    ));
                }
                if credential.values != selection.values
                    || credential.source_refs != selection.source_refs
                {
                    return Err(conflict("stored credential fields differ".to_owned()));
                }
                Ok(())
            }
            _ => Err(conflict("selected credential differs".to_owned())),
        }
    }

    pub fn set(&mut self, name: &str, value: &str) -> Result<()> {
        self.secrets.insert(name.to_owned(), value.to_owned());
        self.persist()
    }

    /// Insert several name/value pairs and persist them together as a single
    /// atomic write. Lets the caller avoid leaving the store in a partial
    /// state if a later `set` would have failed.
    pub fn set_many<'a, I>(&mut self, pairs: I) -> Result<()>
    where
        I: IntoIterator<Item = (&'a str, &'a str)>,
    {
        for (name, value) in pairs {
            self.secrets.insert(name.to_owned(), value.to_owned());
        }
        self.persist()
    }

    pub fn delete(&mut self, name: &str) -> Result<()> {
        if self.secrets.remove(name).is_none() {
            return Err(StackError::SecretNotFound {
                name: name.to_owned(),
            });
        }
        self.persist()
    }

    fn persist(&self) -> Result<()> {
        let plaintext = StorePlaintext {
            secrets: self.secrets.clone(),
            provider_credentials: self.provider_credentials.clone(),
            managed_state: self.managed_state.clone(),
        };
        plaintext.validate()?;
        let ciphertext = encrypt_plaintext(&self.identity.to_public(), &plaintext)?;
        atomic_write_owner_only(&self.store_path, &ciphertext)
    }
}

fn generate_identity(path: &Path) -> Result<age::x25519::Identity> {
    if let Some(parent) = path.parent() {
        // Caller is expected to have created the parent dir owner-only, but
        // tests that drive the store directly may not have. Best-effort
        // ensure it exists.
        if !parent.exists() {
            std::fs::create_dir_all(parent).map_err(|source| StackError::DirectoryCreate {
                path: parent.to_path_buf(),
                source,
            })?;
        }
    }
    let identity = age::x25519::Identity::generate();
    let encoded = identity.to_string();
    write_new_file_owner_only(path, encoded.expose_secret().as_bytes())?;
    Ok(identity)
}

fn load_identity(path: &Path) -> Result<age::x25519::Identity> {
    let contents = std::fs::read_to_string(path).map_err(|source| StackError::AgeKeyRead {
        path: path.to_path_buf(),
        source,
    })?;
    let trimmed = contents.trim();
    age::x25519::Identity::from_str(trimmed).map_err(|reason| StackError::AgeKeyParse {
        path: path.to_path_buf(),
        reason,
    })
}

fn decrypt_store(identity: &age::x25519::Identity, path: &Path) -> Result<StorePlaintext> {
    let ciphertext = std::fs::read(path).map_err(|source| StackError::SecretStoreRead {
        path: path.to_path_buf(),
        source,
    })?;
    let plaintext_bytes = age::decrypt(identity, &ciphertext)?;
    let plaintext_str = std::str::from_utf8(&plaintext_bytes)
        .map_err(|source| StackError::SecretStorePlaintextNotUtf8 { source })?;
    let plaintext: StorePlaintext =
        toml::from_str(plaintext_str).map_err(StackError::SecretStorePlaintextParse)?;
    plaintext.validate()?;
    Ok(plaintext)
}

fn validate_provider_credential(provider_id: &str, credential: &ProviderCredential) -> Result<()> {
    if credential.revision.trim().is_empty() || credential.values.is_empty() {
        return Err(StackError::SecretStorePlaintextInvalid {
            reason: format!(
                "provider credential `{provider_id}` must have a revision and at least one value"
            ),
        });
    }
    for name in credential
        .values
        .keys()
        .chain(credential.source_refs.keys())
        .chain(credential.source_refs.values())
    {
        if !crate::config::is_valid_secret_ref_name(name) {
            return Err(StackError::SecretStorePlaintextInvalid {
                reason: format!(
                    "provider credential `{provider_id}` contains invalid env or secret ref `{name}`"
                ),
            });
        }
    }
    if let Some(name) = credential
        .source_refs
        .keys()
        .find(|name| !credential.values.contains_key(*name))
    {
        return Err(StackError::SecretStorePlaintextInvalid {
            reason: format!(
                "provider credential `{provider_id}` has source ref without value field `{name}`"
            ),
        });
    }
    Ok(())
}

fn new_provider_credential_revision() -> String {
    let mut bytes = [0_u8; 16];
    rand::rng().fill(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn encrypt_plaintext(
    recipient: &age::x25519::Recipient,
    plaintext: &StorePlaintext,
) -> Result<Vec<u8>> {
    let toml_text =
        toml::to_string(plaintext).map_err(StackError::SecretStorePlaintextSerialize)?;
    let ciphertext = age::encrypt(recipient, toml_text.as_bytes())?;
    Ok(ciphertext)
}

/// Ensure both the config dir and the state dir exist with owner-only mode
/// before any secret store operation. Convenience helper for callers that
/// only know the home dir.
pub fn ensure_dirs(home: &Path) -> Result<()> {
    use crate::fs_util::create_dir_owner_only;
    let key_parent = parent_dir(&age_key_path(home))?.to_path_buf();
    let store_parent = parent_dir(&secret_store_path(home))?.to_path_buf();
    create_dir_owner_only(&key_parent)?;
    create_dir_owner_only(&store_parent)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fresh_home() -> TempDir {
        TempDir::new().expect("tempdir")
    }

    #[test]
    fn open_or_create_initializes_empty_store() {
        let home = fresh_home();
        let store = SecretStore::open_or_create(home.path()).expect("open or create");
        assert!(store.list_names().is_empty());
        assert!(age_key_path(home.path()).exists());
        assert!(secret_store_path(home.path()).exists());
    }

    #[test]
    fn set_get_delete_roundtrip() {
        let home = fresh_home();
        let mut store = SecretStore::open_or_create(home.path()).expect("open or create");
        store.set("FOO", "bar").expect("set");
        assert_eq!(store.get("FOO").expect("get"), "bar");
        assert!(store.contains("FOO"));
        store.delete("FOO").expect("delete");
        assert!(matches!(
            store.get("FOO"),
            Err(StackError::SecretNotFound { .. })
        ));
    }

    #[test]
    fn reopen_preserves_secrets() {
        let home = fresh_home();
        {
            let mut store = SecretStore::open_or_create(home.path()).expect("open or create");
            store.set("ALPHA", "1").expect("set alpha");
            store.set("BETA", "2").expect("set beta");
        }
        let store = SecretStore::open(home.path()).expect("reopen");
        assert_eq!(store.get("ALPHA").unwrap(), "1");
        assert_eq!(store.get("BETA").unwrap(), "2");
        let names = store.list_names();
        assert_eq!(names, vec!["ALPHA", "BETA"]);
    }

    #[test]
    fn legacy_plaintext_defaults_provider_catalog_to_empty() {
        let plaintext: StorePlaintext =
            toml::from_str("[secrets]\nALPHA = \"1\"\n").expect("legacy plaintext");

        assert_eq!(
            plaintext.secrets.get("ALPHA").map(String::as_str),
            Some("1")
        );
        assert!(plaintext.provider_credentials.is_empty());
    }

    #[test]
    fn provider_credentials_round_trip_without_exposing_values_in_debug() {
        let home = fresh_home();
        let mut store = SecretStore::open_or_create(home.path()).expect("open or create");
        let credential = ProviderCredential::new(
            BTreeMap::from([("OPENCODE_API_KEY".to_owned(), "private-value".to_owned())]),
            BTreeMap::from([("OPENCODE_API_KEY".to_owned(), "SOURCE_KEY".to_owned())]),
        );
        let revision = credential.revision.clone();
        store
            .replace_provider_credentials(
                BTreeMap::from([(
                    "opencode-go".to_owned(),
                    ProviderCredentialSet::aliasless(credential),
                )]),
                &[],
            )
            .expect("persist catalog");

        let reopened = SecretStore::open(home.path()).expect("reopen");
        let credential = reopened
            .provider_credential_set("opencode-go")
            .and_then(|set| set.sole.as_ref())
            .expect("credential");
        assert_eq!(credential.revision, revision);
        assert_eq!(credential.values["OPENCODE_API_KEY"], "private-value");
        let debug = format!("{credential:?}");
        assert!(!debug.contains("private-value"));
        assert!(!debug.contains(&revision));
    }

    #[test]
    fn staged_provider_credentials_are_not_persisted_until_replaced() {
        let home = fresh_home();
        let mut store = SecretStore::open_or_create(home.path()).expect("open or create");
        let persisted = ProviderCredential::new(
            BTreeMap::from([("OPENCODE_API_KEY".to_owned(), "persisted".to_owned())]),
            BTreeMap::new(),
        );
        store
            .replace_provider_credentials(
                BTreeMap::from([(
                    "opencode-go".to_owned(),
                    ProviderCredentialSet::aliasless(persisted),
                )]),
                &[],
            )
            .expect("persist catalog");

        let staged = ProviderCredential::new(
            BTreeMap::from([("OPENCODE_API_KEY".to_owned(), "staged".to_owned())]),
            BTreeMap::new(),
        );
        store
            .stage_provider_credentials(BTreeMap::from([(
                "opencode-go".to_owned(),
                ProviderCredentialSet::aliasless(staged),
            )]))
            .expect("stage catalog");
        assert_eq!(
            store
                .provider_credential_set("opencode-go")
                .and_then(|set| set.sole.as_ref())
                .expect("staged credential")
                .values["OPENCODE_API_KEY"],
            "staged"
        );

        let reopened = SecretStore::open(home.path()).expect("reopen");
        assert_eq!(
            reopened
                .provider_credential_set("opencode-go")
                .and_then(|set| set.sole.as_ref())
                .expect("persisted credential")
                .values["OPENCODE_API_KEY"],
            "persisted"
        );
    }

    #[test]
    fn rotating_provider_credential_changes_revision_and_keeps_alias_mode() {
        let mut credential = ProviderCredential::new(
            BTreeMap::from([("OPENROUTER_API_KEY".to_owned(), "first".to_owned())]),
            BTreeMap::new(),
        );
        let previous_revision = credential.revision.clone();
        credential.rotate(
            BTreeMap::from([("OPENROUTER_API_KEY".to_owned(), "second".to_owned())]),
            BTreeMap::new(),
        );
        let set =
            ProviderCredentialSet::promoted(BTreeMap::from([("backup".to_owned(), credential)]));

        assert!(set.is_promoted());
        let selected = set.selected(Some("backup")).expect("selected alias").0;
        assert_ne!(selected.revision, previous_revision);
        assert_eq!(selected.values["OPENROUTER_API_KEY"], "second");
    }

    #[test]
    fn delete_unknown_secret_errors() {
        let home = fresh_home();
        let mut store = SecretStore::open_or_create(home.path()).expect("open or create");
        let error = store.delete("NOT_THERE").expect_err("must error");
        assert!(matches!(error, StackError::SecretNotFound { .. }));
    }

    #[test]
    fn open_without_init_fails() {
        let home = fresh_home();
        let error = SecretStore::open(home.path()).expect_err("must fail");
        assert!(matches!(error, StackError::AgeKeyRead { .. }));
    }

    #[test]
    fn open_with_corrupt_age_key_errors() {
        let home = fresh_home();
        let key_path = age_key_path(home.path());
        std::fs::create_dir_all(key_path.parent().unwrap()).unwrap();
        std::fs::write(&key_path, "not-an-age-key").unwrap();
        let error = SecretStore::open(home.path()).expect_err("must fail");
        assert!(matches!(error, StackError::AgeKeyParse { .. }));
    }

    fn selection(provider_id: &str, env_name: &str, value: &str) -> ManagedCredentialSelection {
        ManagedCredentialSelection {
            provider_id: provider_id.to_owned(),
            values: BTreeMap::from([(env_name.to_owned(), value.to_owned())]),
            source_refs: BTreeMap::new(),
        }
    }

    #[test]
    fn legacy_plaintext_defaults_source_to_operator_and_managed_state_to_empty() {
        let plaintext: StorePlaintext = toml::from_str(
            "[secrets]\nALPHA = \"1\"\n\
             [provider_credentials.openai.sole]\n\
             revision = \"r1\"\n\
             [provider_credentials.openai.sole.values]\n\
             OPENAI_API_KEY = \"sk\"\n",
        )
        .expect("legacy plaintext");
        let credential = plaintext.provider_credentials["openai"]
            .sole
            .as_ref()
            .expect("sole credential");
        assert_eq!(credential.source, CredentialSource::Operator);
        assert!(plaintext.managed_state.is_empty());
    }

    #[test]
    fn operator_entries_serialize_without_source_field() {
        let plaintext = StorePlaintext {
            secrets: BTreeMap::new(),
            provider_credentials: BTreeMap::from([(
                "openai".to_owned(),
                ProviderCredentialSet::aliasless(ProviderCredential::new(
                    BTreeMap::from([("OPENAI_API_KEY".to_owned(), "sk".to_owned())]),
                    BTreeMap::new(),
                )),
            )]),
            managed_state: BTreeMap::new(),
        };
        let serialized = toml::to_string(&plaintext).expect("serialize");
        assert!(
            !serialized.contains("source"),
            "operator entries must stay byte-identical on disk, got:\n{serialized}"
        );
    }

    #[test]
    fn managed_apply_persists_credential_and_watermark_atomically() {
        let home = fresh_home();
        let mut store = SecretStore::open_or_create(home.path()).expect("open or create");
        let outcome = store
            .apply_managed_state_credential(
                "platform-state",
                "provider-credential",
                7,
                Some(selection("openai", "OPENAI_API_KEY", "sk-managed")),
            )
            .expect("apply");
        assert_eq!(outcome, ManagedApplyOutcome::Applied);

        let reopened = SecretStore::open(home.path()).expect("reopen");
        let record = reopened
            .managed_state_record("platform-state")
            .expect("watermark record");
        assert_eq!(record.revision, 7);
        assert_eq!(record.provider_id.as_deref(), Some("openai"));
        assert_eq!(record.kind.as_deref(), Some("provider-credential"));
        let credential = reopened
            .provider_credential_set("openai")
            .and_then(|set| set.sole.as_ref())
            .expect("stored credential");
        assert_eq!(credential.values["OPENAI_API_KEY"], "sk-managed");
        assert_eq!(
            credential.source,
            CredentialSource::External("platform-state".to_owned())
        );
        let debug = format!("{credential:?}");
        assert!(!debug.contains("sk-managed"));
    }

    #[test]
    fn managed_apply_replay_is_noop_and_divergent_replay_conflicts() {
        let home = fresh_home();
        let mut store = SecretStore::open_or_create(home.path()).expect("open or create");
        store
            .apply_managed_state_credential(
                "platform-state",
                "provider-credential",
                7,
                Some(selection("openai", "OPENAI_API_KEY", "sk-managed")),
            )
            .expect("apply");

        let replay = store
            .apply_managed_state_credential(
                "platform-state",
                "provider-credential",
                7,
                Some(selection("openai", "OPENAI_API_KEY", "sk-managed")),
            )
            .expect("identical replay");
        assert_eq!(replay, ManagedApplyOutcome::Noop);

        let divergent = store
            .apply_managed_state_credential(
                "platform-state",
                "provider-credential",
                7,
                Some(selection("openai", "OPENAI_API_KEY", "sk-other")),
            )
            .expect_err("divergent replay must conflict");
        assert!(matches!(
            divergent,
            StackError::ExtensionRevisionConflict { .. }
        ));

        let stale = store
            .apply_managed_state_credential("platform-state", "provider-credential", 6, None)
            .expect_err("stale revision must conflict");
        assert!(matches!(
            stale,
            StackError::ExtensionRevisionConflict { .. }
        ));
    }

    #[test]
    fn managed_clear_removes_credential_but_retains_watermark() {
        let home = fresh_home();
        let mut store = SecretStore::open_or_create(home.path()).expect("open or create");
        store
            .apply_managed_state_credential(
                "platform-state",
                "provider-credential",
                7,
                Some(selection("openai", "OPENAI_API_KEY", "sk-managed")),
            )
            .expect("apply");
        let outcome = store
            .apply_managed_state_credential("platform-state", "provider-credential", 8, None)
            .expect("clear");
        assert_eq!(outcome, ManagedApplyOutcome::Cleared);

        let reopened = SecretStore::open(home.path()).expect("reopen");
        assert!(reopened.provider_credential_set("openai").is_none());
        let record = reopened
            .managed_state_record("platform-state")
            .expect("watermark survives clear");
        assert_eq!(record.revision, 8);
        assert!(record.provider_id.is_none());
    }

    #[test]
    fn managed_apply_refuses_operator_and_foreign_namespace_entries() {
        let home = fresh_home();
        let mut store = SecretStore::open_or_create(home.path()).expect("open or create");
        store
            .replace_provider_credentials(
                BTreeMap::from([(
                    "openai".to_owned(),
                    ProviderCredentialSet::aliasless(ProviderCredential::new(
                        BTreeMap::from([("OPENAI_API_KEY".to_owned(), "operator".to_owned())]),
                        BTreeMap::new(),
                    )),
                )]),
                &[],
            )
            .expect("seed operator credential");

        let operator_owned = store
            .apply_managed_state_credential(
                "platform-state",
                "provider-credential",
                7,
                Some(selection("openai", "OPENAI_API_KEY", "sk-managed")),
            )
            .expect_err("operator entry must be protected");
        assert!(matches!(
            operator_owned,
            StackError::ExtensionStateOwnership { .. }
        ));

        store
            .apply_managed_state_credential(
                "namespace-a",
                "provider-credential",
                1,
                Some(selection("groq", "GROQ_API_KEY", "gk-managed")),
            )
            .expect("namespace-a takes groq");
        let foreign = store
            .apply_managed_state_credential(
                "namespace-b",
                "provider-credential",
                1,
                Some(selection("groq", "GROQ_API_KEY", "gk-other")),
            )
            .expect_err("foreign namespace entry must be protected");
        assert!(matches!(
            foreign,
            StackError::ExtensionStateOwnership { .. }
        ));
    }

    #[test]
    fn operator_replace_refuses_to_clobber_external_entries() {
        let home = fresh_home();
        let mut store = SecretStore::open_or_create(home.path()).expect("open or create");
        store
            .apply_managed_state_credential(
                "platform-state",
                "provider-credential",
                7,
                Some(selection("openai", "OPENAI_API_KEY", "sk-managed")),
            )
            .expect("apply");

        let clobber = store
            .replace_provider_credentials(BTreeMap::new(), &[])
            .expect_err("operator replace must not drop an external entry");
        assert!(matches!(
            clobber,
            StackError::ExtensionStateOwnership { .. }
        ));

        // An operator replace that carries the external entry through
        // unchanged is fine.
        let carried = store.provider_credentials().clone();
        store
            .replace_provider_credentials(carried, &[])
            .expect("carry-through replace succeeds");
    }
}
