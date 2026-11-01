//! Secret-store and age-key error helpers (`secrets.*` namespace).

use http::StatusCode;

use super::StackError;

pub(super) fn error_code(err: &StackError) -> Option<&'static str> {
    use StackError::*;
    Some(match err {
        AgeKeyRead { .. } | SecretStoreRead { .. } => "secrets.read_failed",
        AgeKeyWrite { .. } | SecretStoreWrite { .. } => "secrets.write_failed",
        AgeKeyParse { .. } => "secrets.age_key_invalid",
        SecretStoreEncrypt(_) => "secrets.encrypt_failed",
        SecretStoreDecrypt(_) => "secrets.decrypt_failed",
        SecretStorePlaintextParse(_)
        | SecretStorePlaintextSerialize(_)
        | SecretStorePlaintextInvalid { .. }
        | SecretStorePlaintextNotUtf8 { .. } => "secrets.plaintext_invalid",
        SecretNotFound { .. } => "secrets.not_found",
        SecretReferenced { .. } => "secrets.referenced",
        ProviderCredentialRollbackFailed { .. } => "secrets.rollback_failed",
        ProviderSecretNotPushDeliverable { .. } => "secrets.provider_secret_not_push_deliverable",
        InvalidSecretRefName { .. }
        | DuplicateSecretRef { .. }
        | SecretTemplateInvalid { .. }
        | DuplicateEnvVarName { .. } => "config.invalid",
        _ => return None,
    })
}

pub(super) fn public_message(err: &StackError) -> Option<String> {
    use StackError::*;
    Some(match err {
        AgeKeyRead { .. } | SecretStoreRead { .. } => "failed to read secret material".to_owned(),
        AgeKeyWrite { .. } | SecretStoreWrite { .. } => {
            "failed to write secret material".to_owned()
        }
        AgeKeyParse { .. } => "age key is malformed".to_owned(),
        SecretStoreEncrypt(_) => "failed to encrypt secret store".to_owned(),
        SecretStoreDecrypt(_) => "failed to decrypt secret store".to_owned(),
        SecretStorePlaintextParse(_)
        | SecretStorePlaintextSerialize(_)
        | SecretStorePlaintextInvalid { .. }
        | SecretStorePlaintextNotUtf8 { .. } => "secret store plaintext is invalid".to_owned(),
        SecretNotFound { .. } => "secret was not found".to_owned(),
        SecretReferenced {
            name,
            namespace,
            variable,
        } => format!(
            "secret `{name}` sets `{variable}` in managed-state namespace `{namespace}`; apply the namespace without that entry before deleting it"
        ),
        ProviderCredentialRollbackFailed { .. } => {
            "provider credential change failed and could not be rolled back".to_owned()
        }
        ProviderSecretNotPushDeliverable {
            provider_id,
            env_ref,
        } => format!(
            "provider secret `{env_ref}` for provider `{provider_id}` is missing and cannot be delivered by the managed credential push"
        ),
        // A pasted inline credential fails this same check, so the offending
        // name is never echoed; `Display` keeps it for the local CLI only.
        InvalidSecretRefName { .. } => {
            "a secret ref name is invalid; use ASCII letters, digits, and underscores, and do not start with a digit".to_owned()
        }
        DuplicateSecretRef { name } => {
            format!("secret ref `{name}` is declared more than once")
        }
        SecretTemplateInvalid { field, reason } => {
            format!("secret template at `{field}` is invalid: {reason}")
        }
        DuplicateEnvVarName { field, name } => {
            format!("`{field}` declares env var `{name}` more than once")
        }
        _ => return None,
    })
}

pub(super) fn http_status(err: &StackError) -> Option<StatusCode> {
    use StackError::*;
    Some(match err {
        SecretNotFound { .. } => StatusCode::NOT_FOUND,
        SecretReferenced { .. } => StatusCode::CONFLICT,
        AgeKeyRead { .. }
        | AgeKeyWrite { .. }
        | AgeKeyParse { .. }
        | SecretStoreRead { .. }
        | SecretStoreWrite { .. }
        | SecretStoreEncrypt(_)
        | SecretStoreDecrypt(_)
        | SecretStorePlaintextParse(_)
        | SecretStorePlaintextSerialize(_)
        | SecretStorePlaintextInvalid { .. }
        | SecretStorePlaintextNotUtf8 { .. }
        | ProviderCredentialRollbackFailed { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        InvalidSecretRefName { .. }
        | DuplicateSecretRef { .. }
        | SecretTemplateInvalid { .. }
        | DuplicateEnvVarName { .. }
        | ProviderSecretNotPushDeliverable { .. } => StatusCode::BAD_REQUEST,
        _ => return None,
    })
}
