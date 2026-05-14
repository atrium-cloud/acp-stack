//! HTTP response envelope per `docs/specs/api/api.md:20-42`.
//!
//! These types are not wired into any HTTP handler yet; they exist so that
//! the upcoming axum layer has a single, stable serialization shape to build
//! against. `ApiError::from_stack_error` is the bridge between the domain
//! error enum and the wire envelope.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::StackError;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiSuccess<T> {
    pub ok: bool,
    pub data: T,
}

impl<T> ApiSuccess<T> {
    pub fn new(data: T) -> Self {
        Self { ok: true, data }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiErrorEnvelope {
    pub ok: bool,
    pub error: ApiError,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    pub code: String,
    pub message: String,
    // Always serialized, even when empty. The API spec example at
    // `docs/specs/api/api.md:31-41` shows `"details": {}` as present in error
    // responses; clients and tests can rely on the key existing without
    // having to distinguish "missing" from "empty".
    #[serde(default)]
    pub details: Map<String, Value>,
}

impl ApiError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            details: Map::new(),
        }
    }

    pub fn with_detail(mut self, key: impl Into<String>, value: Value) -> Self {
        self.details.insert(key.into(), value);
        self
    }

    /// Build a wire-ready error from a `StackError`. The dotted code comes
    /// from `StackError::error_code`; the message comes from
    /// `StackError::public_message` so API clients do not receive local paths
    /// or secret-store internals from the CLI/internal `Display` text.
    pub fn from_stack_error(err: &StackError) -> Self {
        Self::new(err.error_code(), err.public_message())
    }

    pub fn into_envelope(self) -> ApiErrorEnvelope {
        ApiErrorEnvelope {
            ok: false,
            error: self,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_envelope_serializes_with_ok_and_data() {
        #[derive(Serialize)]
        struct Body {
            value: u32,
        }
        let env = ApiSuccess::new(Body { value: 42 });
        let json = serde_json::to_value(&env).expect("serialize");
        assert_eq!(json["ok"], Value::Bool(true));
        assert_eq!(json["data"]["value"], Value::Number(42.into()));
    }

    #[test]
    fn error_envelope_always_includes_details_object() {
        let env = ApiError::new("config.invalid", "bad").into_envelope();
        let json = serde_json::to_value(&env).expect("serialize");
        assert_eq!(json["ok"], Value::Bool(false));
        assert_eq!(
            json["error"]["code"],
            Value::String("config.invalid".into())
        );
        assert_eq!(json["error"]["message"], Value::String("bad".into()));
        // The API spec example shows `"details": {}` as a present key even
        // when empty; serialization must include it.
        assert_eq!(json["error"]["details"], serde_json::json!({}));
    }

    #[test]
    fn error_envelope_includes_details_when_present() {
        let env = ApiError::new("workspace.path", "bad path")
            .with_detail("field", Value::String("workspace.root".into()))
            .into_envelope();
        let json = serde_json::to_value(&env).expect("serialize");
        assert_eq!(
            json["error"]["details"]["field"],
            Value::String("workspace.root".into())
        );
    }

    #[test]
    fn from_stack_error_pulls_code_and_message() {
        let err = StackError::MissingField { field: "api.bind" };
        let env = ApiError::from_stack_error(&err);
        assert_eq!(env.code, "config.invalid");
        assert!(env.message.contains("api.bind"));
    }

    #[test]
    fn from_stack_error_sanitizes_local_config_paths() {
        let err = StackError::ConfigRead {
            path: "/home/alice/.config/acp-stack/acp-stack.toml".into(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "missing"),
        };
        let env = ApiError::from_stack_error(&err);
        assert_eq!(env.code, "config.read_failed");
        assert_eq!(env.message, "failed to read config");
        assert!(!env.message.contains("/home/alice"));
    }

    #[test]
    fn from_stack_error_sanitizes_secret_store_paths() {
        let err = StackError::AgeKeyRead {
            path: "/home/alice/.config/acp-stack/age.key".into(),
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
        };
        let env = ApiError::from_stack_error(&err);
        assert_eq!(env.code, "secrets.read_failed");
        assert_eq!(env.message, "failed to read secret material");
        assert!(!env.message.contains("age.key"));
        assert!(!env.message.contains("/home/alice"));
    }

    #[test]
    fn from_stack_error_sanitizes_secret_names() {
        let err = StackError::SecretNotFound {
            name: "PRIVATE_TOKEN".into(),
        };
        let env = ApiError::from_stack_error(&err);
        assert_eq!(env.code, "secrets.not_found");
        assert_eq!(env.message, "secret was not found");
        assert!(!env.message.contains("PRIVATE_TOKEN"));
    }

    #[test]
    fn from_stack_error_sanitizes_auth_ref_import_names() {
        let err = StackError::ImportChangesAuthRef {
            field: "admin_key_ref",
            current: "ACP_STACK_ADMIN_KEY".into(),
            incoming: "OTHER_ADMIN_KEY".into(),
        };
        let env = ApiError::from_stack_error(&err);
        assert_eq!(env.code, "config.import_changes_auth_ref");
        assert_eq!(
            env.message,
            "config import would change auth key references"
        );
        assert!(!env.message.contains("ACP_STACK_ADMIN_KEY"));
        assert!(!env.message.contains("OTHER_ADMIN_KEY"));
    }
}
