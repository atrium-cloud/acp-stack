//! API key generation, verifier hashing, constant-time comparison, and structured auth-failure
//! logging. An `auth_failures` row records only the expected kind and the reason, never the
//! attempted key value.

use base64::Engine;
use rand::RngExt;
use serde::Serialize;
use sha2::Digest;
use std::path::Path;
use subtle::ConstantTimeEq;

use crate::config::LegacyAuthConfig;
use crate::error::Result;
use crate::secrets::SecretStore;
use crate::state::{AuthFailure, StateStore};

pub const API_KEY_PREFIX: &str = "acps_";
pub const API_KEY_ENTROPY_BYTES: usize = 32;
pub const AUTH_VERIFIER_ALGORITHM: &str = "sha256-v1";
pub const AUTH_VERIFIER_SALT_BYTES: usize = 32;
pub const AUTH_VERIFIER_DIGEST_BYTES: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyKind {
    Session,
    Admin,
    /// Stamped by the internal Unix-domain-socket listener and not derivable from any bearer;
    /// `enforce_tier` never accepts Local on the TCP router, so a leaked Local tag is a 401.
    Local,
    Unknown,
}

impl KeyKind {
    pub fn as_wire_str(self) -> &'static str {
        match self {
            KeyKind::Session => "session",
            KeyKind::Admin => "admin",
            KeyKind::Local => "local",
            KeyKind::Unknown => "unknown",
        }
    }
}

/// The auth-tier vocabulary of the acp-stack API, described per tier by the
/// root `x-auth-tiers` annotation of the published schema.
// Distinct from `KeyKind`, the runtime request tag: `KeyKind::Unknown`
// classifies failed authentication and is not a tier, and the `init` tier has
// no `KeyKind` because the init-serve token never passes through the TCP
// router's key verifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AuthTier {
    Init,
    Session,
    Admin,
    Local,
}

impl AuthTier {
    pub const ALL: [AuthTier; 4] = [
        AuthTier::Init,
        AuthTier::Session,
        AuthTier::Admin,
        AuthTier::Local,
    ];

    pub fn as_wire_str(self) -> &'static str {
        match self {
            AuthTier::Init => "init",
            AuthTier::Session => "session",
            AuthTier::Admin => "admin",
            AuthTier::Local => "local",
        }
    }

    /// One-line tier summary, published as the `x-auth-tiers` values.
    pub fn description(self) -> &'static str {
        match self {
            AuthTier::Init => {
                "one-off bearer token for `acps init serve`, valid only while hosted setup \
                 runs before session/admin keys exist; supplied via process input"
            }
            AuthTier::Session => {
                "everyday key for sessions, workspace files, mediated commands, logs, status, \
                 and pending permissions"
            }
            AuthTier::Admin => {
                "instance-control key for secrets, config import, agent process control, and \
                 security-sensitive operations"
            }
            AuthTier::Local => {
                "keyless same-user access over the internal Unix socket for local `acps` \
                 routes; never accepted on the TCP router"
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthFailureReason {
    Missing,
    Invalid,
    WrongKind,
    MalformedHeader,
}

impl AuthFailureReason {
    pub fn as_wire_str(self) -> &'static str {
        match self {
            AuthFailureReason::Missing => "missing",
            AuthFailureReason::Invalid => "invalid",
            AuthFailureReason::WrongKind => "wrong_kind",
            AuthFailureReason::MalformedHeader => "malformed_header",
        }
    }
}

/// Generate a fresh `acps_`-prefixed API key from the thread-local CSPRNG.
pub fn generate_api_key() -> String {
    let mut bytes = [0u8; API_KEY_ENTROPY_BYTES];
    rand::rng().fill(&mut bytes);
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    format!("{API_KEY_PREFIX}{encoded}")
}

/// Constant-time equality for equal-length slices. The length check itself is NOT constant-time,
/// which is safe only because the API key length is fixed and public.
pub fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.ct_eq(right).into()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthVerifier {
    key_kind: KeyKind,
    algorithm: String,
    salt: Vec<u8>,
    digest: Vec<u8>,
}

impl AuthVerifier {
    pub fn create(key_kind: KeyKind, plaintext: &str) -> Self {
        let mut salt = [0u8; AUTH_VERIFIER_SALT_BYTES];
        rand::rng().fill(&mut salt);
        let digest = digest_key(&salt, plaintext).to_vec();
        Self {
            key_kind,
            algorithm: AUTH_VERIFIER_ALGORITHM.to_owned(),
            salt: salt.to_vec(),
            digest,
        }
    }

    pub fn from_encoded(
        key_kind: KeyKind,
        algorithm: String,
        salt: String,
        digest: String,
    ) -> Result<Self> {
        if algorithm != AUTH_VERIFIER_ALGORITHM {
            return Err(crate::error::StackError::InvalidParam {
                field: "auth_keys.algorithm",
                reason: format!("unsupported auth verifier algorithm `{algorithm}`"),
            });
        }
        let salt = decode_verifier_part("auth_keys.salt", &salt, AUTH_VERIFIER_SALT_BYTES)?;
        let digest = decode_verifier_part("auth_keys.digest", &digest, AUTH_VERIFIER_DIGEST_BYTES)?;
        Ok(Self {
            key_kind,
            algorithm,
            salt,
            digest,
        })
    }

    pub fn verify(&self, plaintext: &str) -> bool {
        let candidate = digest_key(&self.salt, plaintext);
        constant_time_eq(&candidate, &self.digest)
    }

    pub fn key_kind(&self) -> KeyKind {
        self.key_kind
    }

    pub fn algorithm(&self) -> &str {
        &self.algorithm
    }

    pub fn encoded_salt(&self) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&self.salt)
    }

    pub fn encoded_digest(&self) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&self.digest)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthVerifierSet {
    pub session: AuthVerifier,
    pub admin: AuthVerifier,
}

impl AuthVerifierSet {
    pub fn create(session_key: &str, admin_key: &str) -> Self {
        Self {
            session: AuthVerifier::create(KeyKind::Session, session_key),
            admin: AuthVerifier::create(KeyKind::Admin, admin_key),
        }
    }

    pub fn verify(&self, bearer: &str) -> Option<KeyKind> {
        if self.session.verify(bearer) {
            Some(KeyKind::Session)
        } else if self.admin.verify(bearer) {
            Some(KeyKind::Admin)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthVerifierEnsureOutcome {
    Preserved,
    BackfilledLegacySecrets,
    Missing,
}

pub(crate) fn ensure_auth_verifier_pair(
    store: &StateStore,
    legacy_auth: Option<&LegacyAuthConfig>,
    home: &Path,
) -> Result<AuthVerifierEnsureOutcome> {
    let session_present = store.get_auth_key(KeyKind::Session)?.is_some();
    let admin_present = store.get_auth_key(KeyKind::Admin)?.is_some();
    match (session_present, admin_present) {
        (true, true) => {
            store.load_auth_verifier_pair()?;
            Ok(AuthVerifierEnsureOutcome::Preserved)
        }
        (true, false) => Err(crate::error::StackError::MissingField {
            field: "auth_keys.admin",
        }),
        (false, true) => Err(crate::error::StackError::MissingField {
            field: "auth_keys.session",
        }),
        (false, false) => {
            let Some(legacy_auth) = legacy_auth else {
                return Ok(AuthVerifierEnsureOutcome::Missing);
            };
            let secret_store = SecretStore::open(home)?;
            let session_key = secret_store.get(&legacy_auth.session_key_ref)?.to_owned();
            let admin_key = secret_store.get(&legacy_auth.admin_key_ref)?.to_owned();
            let verifiers = AuthVerifierSet::create(&session_key, &admin_key);
            store.insert_auth_key_pair(&verifiers)?;
            Ok(AuthVerifierEnsureOutcome::BackfilledLegacySecrets)
        }
    }
}

fn digest_key(salt: &[u8], plaintext: &str) -> [u8; AUTH_VERIFIER_DIGEST_BYTES] {
    let mut hasher = sha2::Sha256::new();
    hasher.update(salt);
    hasher.update(plaintext.as_bytes());
    hasher.finalize().into()
}

fn decode_verifier_part(
    field: &'static str,
    encoded: &str,
    expected_len: usize,
) -> Result<Vec<u8>> {
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|source| crate::error::StackError::InvalidParam {
            field,
            reason: format!("base64url decode failed: {source}"),
        })?;
    if decoded.len() != expected_len {
        return Err(crate::error::StackError::InvalidParam {
            field,
            reason: format!("expected {expected_len} bytes, got {}", decoded.len()),
        });
    }
    Ok(decoded)
}

/// Persist an auth-failure record. The attempted key value must never be written here.
pub fn record_auth_failure(
    state: &StateStore,
    kind: KeyKind,
    reason: AuthFailureReason,
    client_ip: Option<&str>,
    route: Option<&str>,
) -> Result<AuthFailure> {
    record_auth_failure_with_origin(state, kind, reason, client_ip, route, None)
}

pub fn record_auth_failure_with_origin(
    state: &StateStore,
    kind: KeyKind,
    reason: AuthFailureReason,
    client_ip: Option<&str>,
    route: Option<&str>,
    origin: Option<&crate::http_hardening::RequestOrigin>,
) -> Result<AuthFailure> {
    let mut payload = serde_json::json!({
        "key_kind": kind.as_wire_str(),
        "reason": reason.as_wire_str(),
        "client_ip": client_ip,
        "route": route,
    });
    if let (Some(origin), Some(map)) = (origin, payload.as_object_mut()) {
        map.insert("origin".to_owned(), origin.as_json());
    }
    state.append_auth_failure(
        kind.as_wire_str(),
        reason.as_wire_str(),
        client_ip,
        route,
        &payload.to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_keys_have_acps_prefix_and_43_char_body() {
        let key = generate_api_key();
        assert!(key.starts_with(API_KEY_PREFIX));
        let body = &key[API_KEY_PREFIX.len()..];
        assert_eq!(body.len(), 43);
        for c in body.chars() {
            assert!(
                c.is_ascii_alphanumeric() || c == '-' || c == '_',
                "non-base64url char: {c}"
            );
        }
    }

    #[test]
    fn generated_keys_differ() {
        let a = generate_api_key();
        let b = generate_api_key();
        assert_ne!(a, b);
    }

    #[test]
    fn constant_time_eq_matches_equal_slices() {
        assert!(constant_time_eq(b"hello", b"hello"));
    }

    #[test]
    fn constant_time_eq_rejects_different_slices() {
        assert!(!constant_time_eq(b"hello", b"world"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"", b"a"));
    }

    #[test]
    fn auth_verifier_accepts_original_key_and_rejects_other_values() {
        let verifier = AuthVerifier::create(KeyKind::Session, "acps_original");

        assert!(verifier.verify("acps_original"));
        assert!(!verifier.verify("acps_other"));
    }

    #[test]
    fn auth_verifier_round_trips_encoded_parts() {
        let verifier = AuthVerifier::create(KeyKind::Admin, "acps_admin");
        let restored = AuthVerifier::from_encoded(
            KeyKind::Admin,
            verifier.algorithm().to_owned(),
            verifier.encoded_salt(),
            verifier.encoded_digest(),
        )
        .expect("restore verifier");

        assert_eq!(restored.key_kind(), KeyKind::Admin);
        assert!(restored.verify("acps_admin"));
        assert!(!restored.verify("acps_session"));
    }

    #[test]
    fn ensure_auth_verifier_pair_backfills_legacy_secret_refs() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let state_path = tempdir.path().join("state.sqlite");
        let store = StateStore::open(&state_path).expect("state");
        store.migrate().expect("migrate");
        let mut secret_store =
            crate::secrets::SecretStore::open_or_create(tempdir.path()).expect("secret store");
        secret_store
            .set_many([
                ("ACP_STACK_SESSION_KEY", "acps_legacy_session"),
                ("ACP_STACK_ADMIN_KEY", "acps_legacy_admin"),
            ])
            .expect("seed legacy keys");
        let legacy_auth = crate::config::LegacyAuthConfig {
            session_key_ref: "ACP_STACK_SESSION_KEY".to_owned(),
            admin_key_ref: "ACP_STACK_ADMIN_KEY".to_owned(),
        };

        let outcome = ensure_auth_verifier_pair(&store, Some(&legacy_auth), tempdir.path())
            .expect("backfill legacy verifiers");

        assert_eq!(outcome, AuthVerifierEnsureOutcome::BackfilledLegacySecrets);
        let verifiers = store.load_auth_verifier_pair().expect("verifiers");
        assert_eq!(
            verifiers.verify("acps_legacy_session"),
            Some(KeyKind::Session)
        );
        assert_eq!(verifiers.verify("acps_legacy_admin"), Some(KeyKind::Admin));
    }

    #[test]
    fn ensure_auth_verifier_pair_rejects_partial_state() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let state_path = tempdir.path().join("state.sqlite");
        let store = StateStore::open(&state_path).expect("state");
        store.migrate().expect("migrate");
        let verifiers = AuthVerifierSet::create("acps_session", "acps_admin");
        store
            .upsert_auth_key(KeyKind::Session, &verifiers.session)
            .expect("seed session verifier");

        let error = ensure_auth_verifier_pair(&store, None, tempdir.path())
            .expect_err("partial auth key rows must fail");

        assert!(error.to_string().contains("auth_keys.admin"), "{error}");
    }

    #[test]
    fn key_kind_wire_strings_are_stable() {
        assert_eq!(KeyKind::Session.as_wire_str(), "session");
        assert_eq!(KeyKind::Admin.as_wire_str(), "admin");
        assert_eq!(KeyKind::Local.as_wire_str(), "local");
        assert_eq!(KeyKind::Unknown.as_wire_str(), "unknown");
    }

    #[test]
    fn auth_tier_wire_strings_are_stable() {
        assert_eq!(AuthTier::Init.as_wire_str(), "init");
        assert_eq!(AuthTier::Session.as_wire_str(), "session");
        assert_eq!(AuthTier::Admin.as_wire_str(), "admin");
        assert_eq!(AuthTier::Local.as_wire_str(), "local");
    }

    #[test]
    fn auth_tier_strings_match_key_kind_for_shared_tiers() {
        assert_eq!(
            AuthTier::Session.as_wire_str(),
            KeyKind::Session.as_wire_str()
        );
        assert_eq!(AuthTier::Admin.as_wire_str(), KeyKind::Admin.as_wire_str());
        assert_eq!(AuthTier::Local.as_wire_str(), KeyKind::Local.as_wire_str());
    }

    #[test]
    fn auth_tier_all_covers_every_variant() {
        assert_eq!(AuthTier::ALL.len(), 4);
        for tier in AuthTier::ALL {
            assert!(!tier.as_wire_str().is_empty());
            assert!(!tier.description().is_empty());
        }
    }

    #[test]
    fn auth_failure_reason_wire_strings_are_stable() {
        assert_eq!(AuthFailureReason::Missing.as_wire_str(), "missing");
        assert_eq!(AuthFailureReason::Invalid.as_wire_str(), "invalid");
        assert_eq!(AuthFailureReason::WrongKind.as_wire_str(), "wrong_kind");
        assert_eq!(
            AuthFailureReason::MalformedHeader.as_wire_str(),
            "malformed_header"
        );
    }
}
