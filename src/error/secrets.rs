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
        | SecretStorePlaintextNotUtf8 { .. } => "secrets.plaintext_invalid",
        SecretNotFound { .. } => "secrets.not_found",
        InvalidSecretRefName { .. } | DuplicateSecretRef { .. } => "config.invalid",
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
        | SecretStorePlaintextNotUtf8 { .. } => "secret store plaintext is invalid".to_owned(),
        SecretNotFound { .. } => "secret was not found".to_owned(),
        InvalidSecretRefName { name } => format!("secret ref name `{name}` is invalid"),
        DuplicateSecretRef { name } => {
            format!("secret ref `{name}` is declared more than once")
        }
        _ => return None,
    })
}

pub(super) fn http_status(err: &StackError) -> Option<StatusCode> {
    use StackError::*;
    Some(match err {
        SecretNotFound { .. } => StatusCode::NOT_FOUND,
        AgeKeyRead { .. }
        | AgeKeyWrite { .. }
        | AgeKeyParse { .. }
        | SecretStoreRead { .. }
        | SecretStoreWrite { .. }
        | SecretStoreEncrypt(_)
        | SecretStoreDecrypt(_)
        | SecretStorePlaintextParse(_)
        | SecretStorePlaintextSerialize(_)
        | SecretStorePlaintextNotUtf8 { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        InvalidSecretRefName { .. } | DuplicateSecretRef { .. } => StatusCode::BAD_REQUEST,
        _ => return None,
    })
}
