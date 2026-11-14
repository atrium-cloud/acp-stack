//! Age-encrypted secret store (`age.key` + `secrets.age`), rewritten in full on every mutation.

use std::collections::{BTreeMap, BTreeSet};
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

/// No-op guard at mutation call sites: auth keys live as state verifiers, so no secret-store name is reserved for auth.
pub fn reject_auth_ref_mutation(_name: &str) -> Result<()> {
    Ok(())
}

/// Runtime config directory: `~/.config/acp-stack/`. Owner-only (0700).
pub fn config_dir(home: &Path) -> PathBuf {
    home.join(".config").join("acp-stack")
}

/// Runtime state directory: `~/.local/share/acp-stack/`. Owner-only (0700).
pub fn state_dir(home: &Path) -> PathBuf {
    home.join(".local").join("share").join("acp-stack")
}

pub fn age_key_path(home: &Path) -> PathBuf {
    config_dir(home).join("age.key")
}

pub fn secret_store_path(home: &Path) -> PathBuf {
    state_dir(home).join("secrets.age")
}

/// The managed-state workload environment for `home`, resolved for one spawn; a store that
/// does not exist yet has none.
pub fn managed_environment_for_home(home: &Path) -> Result<BTreeMap<String, String>> {
    if !age_key_path(home).exists() || !secret_store_path(home).exists() {
        return Ok(BTreeMap::new());
    }
    SecretStore::open_read_only(home)?.managed_environment()
}

/// The one writer-visible handle a long-running process shares across threads.
/// The store rewrites its whole ciphertext on every mutation, so a second
/// decrypted snapshot's later persist would silently clobber the first
/// handle's writes; every writer must go through this handle instead.
pub type SharedSecretStore = std::sync::Arc<std::sync::Mutex<SecretStore>>;

pub fn new_shared_secret_store(store: SecretStore) -> SharedSecretStore {
    std::sync::Arc::new(std::sync::Mutex::new(store))
}

/// Lock the shared handle, recovering from poisoning. Recovery is deliberate: a panicking writer
/// is rare, and forcing every later writer to fail would be worse than proceeding from the
/// recovered state. The single-op mutators (`set`, `set_many`, `delete`) mutate memory before
/// persisting, so a persist error can leave memory one step ahead of disk; the deposit transaction
/// restores its snapshot on both error AND panic (see `deposit_and_apply_managed_credential`), so a
/// failed deposit cannot leave an orphaned secret behind a recovered lock.
pub fn lock_shared_secret_store(
    handle: &SharedSecretStore,
) -> std::sync::MutexGuard<'_, SecretStore> {
    handle
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The outcome of staging a managed-state credential apply without writing it: either the applied
/// revision matches what is already installed (`Noop`) or it produces a new candidate catalog and
/// namespace record to persist. Separating staging from the write lets a deposit fold the flat
/// secret write and the catalog swap into one atomic transaction.
enum StagedManagedCredential {
    Noop,
    Changed {
        provider_credentials: BTreeMap<String, ProviderCredentialSet>,
        managed_state: BTreeMap<String, ManagedStateRecord>,
        outcome: ManagedApplyOutcome,
    },
}

/// Loaded, decrypted view of the secret store; every mutation writes through to disk atomically.
pub struct SecretStore {
    identity: age::x25519::Identity,
    secrets: BTreeMap<String, String>,
    provider_credentials: BTreeMap<String, ProviderCredentialSet>,
    managed_state: BTreeMap<String, ManagedStateRecord>,
    store_path: PathBuf,
}

impl fmt::Debug for SecretStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never leak the identity or secret values via Debug; names alone are already public.
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

/// Provenance of a stored provider credential; overwrite protection across the two owners is enforced by the store itself, not by any one endpoint.
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

/// Unknown fields load and drop on the next write, so a store holding the retired `base_url` opens.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCredential {
    pub revision: String,
    pub values: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub source_refs: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub migrated: bool,
    /// Absent on disk for operator entries, so pre-provenance stores load unchanged.
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

/// Durable per-namespace record of the last applied managed-state registry, written atomically with the credential catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedStateRecord {
    pub revision: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    /// A `provider-credential-set` record's providers, in active-set order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// A `secret-environment` record's workload env names, each mapped to its secret ref.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub environment: BTreeMap<String, String>,
}

impl ManagedStateRecord {
    /// Every provider whose catalog entry the namespace owns, whichever credential kind wrote it.
    pub fn owned_provider_ids(&self) -> impl Iterator<Item = &str> {
        self.provider_id
            .iter()
            .chain(&self.provider_ids)
            .map(String::as_str)
    }
}

/// The managed-state selection the store applies: env-keyed values plus optional secret refs for one provider.
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
    /// Open an existing store, or create an empty one. Either both the age key and the ciphertext exist or neither does; an asymmetric state is corruption and is rejected before any generate/encrypt path runs.
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
            // Repair owner-only mode before reading: this key decrypts every stored API key.
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

        let store = Self {
            identity,
            secrets: plaintext.secrets,
            provider_credentials: plaintext.provider_credentials,
            managed_state: plaintext.managed_state,
            store_path: store_path.to_path_buf(),
        };
        store.register_known_values();
        Ok(store)
    }

    /// Open an existing store, failing if the age key or the ciphertext is missing.
    pub fn open(home: &Path) -> Result<Self> {
        let key_path = age_key_path(home);
        let store_path = secret_store_path(home);
        Self::open_at_paths(&key_path, &store_path)
    }

    /// Open the existing store without repairing permissions, so validation cannot mutate any live runtime path.
    pub fn open_read_only(home: &Path) -> Result<Self> {
        let key_path = age_key_path(home);
        let store_path = secret_store_path(home);
        validate_owner_only_regular_file(&key_path)?;
        validate_owner_only_regular_file(&store_path)?;
        let identity = load_identity(&key_path)?;
        let plaintext = decrypt_store(&identity, &store_path)?;
        let store = Self {
            identity,
            secrets: plaintext.secrets,
            provider_credentials: plaintext.provider_credentials,
            managed_state: plaintext.managed_state,
            store_path,
        };
        store.register_known_values();
        Ok(store)
    }

    /// Open an existing store from explicit runtime-managed paths.
    pub fn open_at_paths(key_path: &Path, store_path: &Path) -> Result<Self> {
        if key_path.exists() {
            set_owner_only_file(key_path)?;
        }
        let identity = load_identity(key_path)?;
        if store_path.exists() {
            set_owner_only_file(store_path)?;
        }
        let plaintext = decrypt_store(&identity, store_path)?;

        let store = Self {
            identity,
            secrets: plaintext.secrets,
            provider_credentials: plaintext.provider_credentials,
            managed_state: plaintext.managed_state,
            store_path: store_path.to_path_buf(),
        };
        store.register_known_values();
        Ok(store)
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
        self.register_known_values();
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
        self.register_known_values();
        Ok(())
    }

    /// Operator-path mutations must not clobber externally-owned entries; those change only through [`Self::apply_managed_state_credential`].
    /// Ownership checks inspect `sole` only, which holds because an external credential is always stored aliasless and this guard blocks the one flow (alias promotion) that could relocate it into `aliases`.
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

    pub fn managed_provider_ids(&self, namespace: &str) -> BTreeSet<String> {
        self.managed_state
            .get(namespace)
            .into_iter()
            .flat_map(|record| record.owned_provider_ids().map(str::to_owned))
            .collect()
    }

    /// Apply one managed-state registry revision for `namespace`; the namespace may only create entries or replace its own.
    pub fn apply_managed_state_credential(
        &mut self,
        namespace: &str,
        kind: &str,
        revision: i64,
        selection: Option<ManagedCredentialSelection>,
    ) -> Result<ManagedApplyOutcome> {
        let selections = selection.into_iter().collect();
        self.commit_staged(
            self.stage_managed_state_credential(namespace, kind, revision, selections, false)?,
        )
    }

    /// Apply one `provider-credential-set` revision: the namespace owns exactly these providers.
    pub fn apply_managed_state_credential_set(
        &mut self,
        namespace: &str,
        kind: &str,
        revision: i64,
        selections: Vec<ManagedCredentialSelection>,
    ) -> Result<ManagedApplyOutcome> {
        self.commit_staged(
            self.stage_managed_state_credential(namespace, kind, revision, selections, true)?,
        )
    }

    fn commit_staged(&mut self, staged: StagedManagedCredential) -> Result<ManagedApplyOutcome> {
        match staged {
            StagedManagedCredential::Noop => Ok(ManagedApplyOutcome::Noop),
            StagedManagedCredential::Changed {
                provider_credentials,
                managed_state,
                outcome,
            } => {
                self.commit_managed_state(provider_credentials, managed_state)?;
                Ok(outcome)
            }
        }
    }

    /// Apply one `secret-environment` revision for `namespace`. The record keeps refs, not
    /// values, so a ref's value may rotate under the same revision and the next spawn reads
    /// it; replay identity compares the refs alone.
    pub fn apply_managed_state_environment(
        &mut self,
        namespace: &str,
        kind: &str,
        revision: i64,
        environment: BTreeMap<String, String>,
    ) -> Result<ManagedApplyOutcome> {
        let record = self.managed_state.get(namespace);
        if let Some(record) = self.replayed_record(namespace, revision)? {
            if record.kind.as_deref() != Some(kind) || record.environment != environment {
                return Err(StackError::ExtensionRevisionConflict {
                    namespace: namespace.to_owned(),
                    reason: format!(
                        "revision {revision} is already applied with different content"
                    ),
                });
            }
            return Ok(ManagedApplyOutcome::Noop);
        }
        let mut catalog = self.provider_credentials.clone();
        for previous_provider in record
            .into_iter()
            .flat_map(ManagedStateRecord::owned_provider_ids)
        {
            catalog.remove(previous_provider);
        }
        let outcome = if environment.is_empty() {
            ManagedApplyOutcome::Cleared
        } else {
            ManagedApplyOutcome::Applied
        };
        let mut managed_state = self.managed_state.clone();
        managed_state.insert(
            namespace.to_owned(),
            ManagedStateRecord {
                revision,
                provider_id: None,
                provider_ids: Vec::new(),
                kind: Some(kind.to_owned()),
                environment,
            },
        );
        self.commit_managed_state(catalog, managed_state)?;
        Ok(outcome)
    }

    /// Every managed-state environment entry with its ref resolved, for one workload spawn.
    pub fn managed_environment(&self) -> Result<BTreeMap<String, String>> {
        let mut environment = BTreeMap::new();
        for (namespace, record) in &self.managed_state {
            for (name, secret_ref) in &record.environment {
                let value = self.get(secret_ref).map_err(|error| match error {
                    StackError::SecretNotFound { .. } => {
                        StackError::ExtensionEnvironmentUnresolved {
                            namespace: namespace.clone(),
                            name: name.clone(),
                            secret_ref: secret_ref.clone(),
                        }
                    }
                    other => other,
                })?;
                environment.insert(name.clone(), value.to_owned());
            }
        }
        Ok(environment)
    }

    /// The revision rule every managed-state kind shares: positive and never below the
    /// applied one. `Some` is a replay at the applied revision, whose content must match.
    fn replayed_record(
        &self,
        namespace: &str,
        revision: i64,
    ) -> Result<Option<&ManagedStateRecord>> {
        if revision <= 0 {
            return Err(StackError::InvalidParam {
                field: "revision",
                reason: "revision must be a positive integer".to_owned(),
            });
        }
        match self.managed_state.get(namespace) {
            Some(record) if revision == record.revision => Ok(Some(record)),
            Some(record) if revision < record.revision => {
                Err(StackError::ExtensionRevisionConflict {
                    namespace: namespace.to_owned(),
                    reason: format!(
                        "revision {revision} is stale; revision {} is already applied",
                        record.revision
                    ),
                })
            }
            _ => Ok(None),
        }
    }

    fn commit_managed_state(
        &mut self,
        provider_credentials: BTreeMap<String, ProviderCredentialSet>,
        managed_state: BTreeMap<String, ManagedStateRecord>,
    ) -> Result<()> {
        let plaintext = StorePlaintext {
            secrets: self.secrets.clone(),
            provider_credentials: provider_credentials.clone(),
            managed_state: managed_state.clone(),
        };
        plaintext.validate()?;
        let ciphertext = encrypt_plaintext(&self.identity.to_public(), &plaintext)?;
        atomic_write_owner_only(&self.store_path, &ciphertext)?;
        self.provider_credentials = provider_credentials;
        self.managed_state = managed_state;
        self.register_known_values();
        Ok(())
    }

    /// Deposit flat secrets and apply a managed-state credential selection as ONE transaction. The
    /// deposited secrets are made visible to `resolve` first, so a `source_refs` entry may name a
    /// secret this same call deposits, but nothing is written to disk or committed in memory
    /// unless the whole operation succeeds. A stale revision, ownership conflict, or invalid
    /// selection restores the store exactly as it was, on disk and in memory, rather than leaving
    /// the deposited secrets behind (which `set_many` followed by a failing apply would do).
    ///
    /// Persists once even when the managed apply is a no-op, as long as there are secrets to write;
    /// an empty deposit whose apply no-ops writes nothing, matching a bare `apply`.
    pub fn deposit_and_apply_managed_credential<'a, I, F>(
        &mut self,
        secrets: I,
        namespace: &str,
        kind: &str,
        revision: i64,
        resolve: F,
    ) -> Result<ManagedApplyOutcome>
    where
        I: IntoIterator<Item = (&'a str, &'a str)>,
        F: FnOnce(&Self) -> Result<Option<ManagedCredentialSelection>>,
    {
        let secrets_snapshot = self.secrets.clone();
        // `catch_unwind` so a panic inside the transaction (in `resolve`, or the staging it drives)
        // restores the snapshot too, not only an `Err`. Without it a panic would leave the deposit
        // in memory, poison the mutex, and, because the lock is recovered rather than refused,
        // let the next successful write persist the orphaned secret. `AssertUnwindSafe` is required
        // only because `&mut self` is not `UnwindSafe`; the snapshot restore below re-establishes a
        // consistent state before the unwind resumes.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut deposited_any = false;
            for (name, value) in secrets {
                self.secrets.insert(name.to_owned(), value.to_owned());
                deposited_any = true;
            }
            let selections = resolve(self)?.into_iter().collect();
            let staged =
                self.stage_managed_state_credential(namespace, kind, revision, selections, false)?;
            let (provider_credentials, managed_state, outcome) = match staged {
                StagedManagedCredential::Noop => {
                    if !deposited_any {
                        // Nothing to write: the deposit carried no secrets and the apply no-ops.
                        return Ok(ManagedApplyOutcome::Noop);
                    }
                    (
                        self.provider_credentials.clone(),
                        self.managed_state.clone(),
                        ManagedApplyOutcome::Noop,
                    )
                }
                StagedManagedCredential::Changed {
                    provider_credentials,
                    managed_state,
                    outcome,
                } => (provider_credentials, managed_state, outcome),
            };
            let plaintext = StorePlaintext {
                secrets: self.secrets.clone(),
                provider_credentials: provider_credentials.clone(),
                managed_state: managed_state.clone(),
            };
            plaintext.validate()?;
            let ciphertext = encrypt_plaintext(&self.identity.to_public(), &plaintext)?;
            atomic_write_owner_only(&self.store_path, &ciphertext)?;
            self.provider_credentials = provider_credentials;
            self.managed_state = managed_state;
            Ok(outcome)
        }));
        match result {
            Ok(Ok(outcome)) => {
                self.register_known_values();
                Ok(outcome)
            }
            Ok(Err(error)) => {
                // Roll back the in-memory deposit so a failed apply cannot leave the store's memory
                // holding secrets that never reached disk.
                self.secrets = secrets_snapshot;
                Err(error)
            }
            Err(panic) => {
                self.secrets = secrets_snapshot;
                std::panic::resume_unwind(panic);
            }
        }
    }

    /// Pure validation + candidate construction for a managed-state credential apply: no mutation,
    /// no disk write. `apply_managed_state_credential` persists and commits the result;
    /// `deposit_and_apply_managed_credential` folds it into a larger transaction. `as_set` records
    /// the providers as a set rather than the one `provider-credential` selection.
    fn stage_managed_state_credential(
        &self,
        namespace: &str,
        kind: &str,
        revision: i64,
        selections: Vec<ManagedCredentialSelection>,
        as_set: bool,
    ) -> Result<StagedManagedCredential> {
        if let Some(record) = self.replayed_record(namespace, revision)? {
            self.ensure_identical_replay(namespace, kind, record, &selections)?;
            return Ok(StagedManagedCredential::Noop);
        }
        let record = self.managed_state.get(namespace);

        let mut catalog = self.provider_credentials.clone();
        for previous_provider in record
            .into_iter()
            .flat_map(ManagedStateRecord::owned_provider_ids)
        {
            catalog.remove(previous_provider);
        }
        let mut provider_ids = Vec::with_capacity(selections.len());
        for selection in selections {
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
            // An unchanged entry keeps its revision, so a running Agent's loaded snapshot still matches it.
            let credential = match self.owned_credential(namespace, &selection.provider_id) {
                Some(stored)
                    if stored.values == selection.values
                        && stored.source_refs == selection.source_refs =>
                {
                    stored.clone()
                }
                _ => ProviderCredential {
                    revision: format!("managed:{namespace}:{revision}"),
                    values: selection.values,
                    source_refs: selection.source_refs,
                    migrated: false,
                    source: CredentialSource::External(namespace.to_owned()),
                },
            };
            catalog.insert(
                selection.provider_id.clone(),
                ProviderCredentialSet::aliasless(credential),
            );
            provider_ids.push(selection.provider_id);
        }
        let outcome = if provider_ids.is_empty() {
            ManagedApplyOutcome::Cleared
        } else {
            ManagedApplyOutcome::Applied
        };
        let (provider_id, provider_ids) = if as_set {
            (None, provider_ids)
        } else {
            (provider_ids.pop(), Vec::new())
        };
        let mut managed_state = self.managed_state.clone();
        managed_state.insert(
            namespace.to_owned(),
            ManagedStateRecord {
                revision,
                provider_id,
                provider_ids,
                kind: Some(kind.to_owned()),
                environment: BTreeMap::new(),
            },
        );
        Ok(StagedManagedCredential::Changed {
            provider_credentials: catalog,
            managed_state,
            outcome,
        })
    }

    fn owned_credential(&self, namespace: &str, provider_id: &str) -> Option<&ProviderCredential> {
        self.provider_credentials
            .get(provider_id)
            .and_then(|set| set.sole.as_ref())
            .filter(|credential| {
                credential.source == CredentialSource::External(namespace.to_owned())
            })
    }

    /// A replay at the already-applied revision must be an exact no-op; anything else at that revision is a conflict.
    fn ensure_identical_replay(
        &self,
        namespace: &str,
        kind: &str,
        record: &ManagedStateRecord,
        selections: &[ManagedCredentialSelection],
    ) -> Result<()> {
        let conflict = |reason: &str| StackError::ExtensionRevisionConflict {
            namespace: namespace.to_owned(),
            reason: format!(
                "revision {} is already applied with different content: {reason}",
                record.revision
            ),
        };
        if record.kind.as_deref() != Some(kind) {
            return Err(conflict("desired kind differs"));
        }
        let selected = selections
            .iter()
            .map(|selection| selection.provider_id.as_str());
        if !record.owned_provider_ids().eq(selected) {
            return Err(conflict("selected credential differs"));
        }
        for selection in selections {
            let credential = self
                .owned_credential(namespace, &selection.provider_id)
                .ok_or_else(|| conflict("the namespace no longer holds an applied credential"))?;
            if credential.values != selection.values
                || credential.source_refs != selection.source_refs
            {
                return Err(conflict("stored credential fields differ"));
            }
        }
        Ok(())
    }

    pub fn set(&mut self, name: &str, value: &str) -> Result<()> {
        crate::redaction::register_secret_values([value]);
        self.secrets.insert(name.to_owned(), value.to_owned());
        self.persist()
    }

    /// Insert several name/value pairs and persist them together as a single atomic write.
    pub fn set_many<'a, I>(&mut self, pairs: I) -> Result<()>
    where
        I: IntoIterator<Item = (&'a str, &'a str)>,
    {
        for (name, value) in pairs {
            crate::redaction::register_secret_values([value]);
            self.secrets.insert(name.to_owned(), value.to_owned());
        }
        self.persist()
    }

    /// A secret a managed environment entry still names is refused, so every spawn
    /// keeps resolving each ref the namespace holds.
    pub fn delete(&mut self, name: &str) -> Result<()> {
        for (namespace, record) in &self.managed_state {
            if let Some((variable, _)) = record
                .environment
                .iter()
                .find(|(_, secret_ref)| secret_ref.as_str() == name)
            {
                return Err(StackError::SecretReferenced {
                    name: name.to_owned(),
                    namespace: namespace.clone(),
                    variable: variable.clone(),
                });
            }
        }
        if self.secrets.remove(name).is_none() {
            return Err(StackError::SecretNotFound {
                name: name.to_owned(),
            });
        }
        self.persist()
    }

    /// Feed every decrypted value to the redaction registry; this store is the one place every
    /// secret-backed value (agent env, MCP headers, provider keys) passes through in plaintext.
    /// Provider companions such as `CLOUD_ML_REGION=global` are skipped: redacting them would
    /// mangle unrelated text (`kimi-coding-global`) without hiding any key material.
    fn register_known_values(&self) {
        let mut values: Vec<&str> = self.secrets.values().map(String::as_str).collect();
        for (provider_id, set) in &self.provider_credentials {
            let non_credential_refs =
                crate::runtime::agent::provider_keys::non_credential_env_refs_for_provider_id(
                    provider_id,
                );
            values.extend(
                set.sole
                    .iter()
                    .chain(set.aliases.values())
                    .flat_map(|credential| credential.values.iter())
                    .filter(|(name, _)| !non_credential_refs.contains(&name.as_str()))
                    .map(|(_, value)| value.as_str()),
            );
        }
        crate::redaction::register_secret_values(values);
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
        // Best-effort: tests that drive the store directly may not have created the parent dir.
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

/// Ensure both the config dir and the state dir exist with owner-only mode before any secret store operation.
pub fn ensure_dirs(home: &Path) -> Result<()> {
    use crate::fs_util::create_dir_owner_only;
    let key_parent = parent_dir(&age_key_path(home))?.to_path_buf();
    let store_parent = parent_dir(&secret_store_path(home))?.to_path_buf();
    create_dir_owner_only(&key_parent)?;
    create_dir_owner_only(&store_parent)?;
    Ok(())
}

#[cfg(test)]
mod tests;
