//! HTTP response envelope per `docs/specs/api/api.md:20-42`.
//!
//! `ApiError::from_stack_error` bridges the domain error enum to the wire
//! envelope. `ApiResult<T>` is the standard handler return type: handlers can
//! `?` on `StackError` and the wrapper handles status code + envelope on the
//! way back out.

use axum::Json;
use axum::response::{IntoResponse, Response};
use http::StatusCode;
use http::header::CONTENT_TYPE;
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

    /// Render this error as an HTTP response with the given status code.
    /// Use this when an error is produced outside the `StackError` flow —
    /// notably the auth middleware, which constructs envelopes directly.
    pub fn into_response_with(self, status: StatusCode) -> Response {
        (status, Json(self.into_envelope())).into_response()
    }
}

impl<T> IntoResponse for ApiSuccess<T>
where
    T: Serialize,
{
    fn into_response(self) -> Response {
        (
            StatusCode::OK,
            [(CONTENT_TYPE, "application/json")],
            Json(self),
        )
            .into_response()
    }
}

impl IntoResponse for ApiErrorEnvelope {
    fn into_response(self) -> Response {
        // ApiErrorEnvelope on its own carries no status code. Callers that
        // construct envelopes directly should use `ApiError::into_response_with`
        // to attach an explicit status. Falling through here means the caller
        // forgot; surface that as 500 rather than silently 200.
        (StatusCode::INTERNAL_SERVER_ERROR, Json(self)).into_response()
    }
}

/// Standard handler return type. `Ok` becomes a 200 + `ApiSuccess` envelope;
/// `Err(StackError)` becomes `err.http_status()` + `ApiError` envelope built
/// via `ApiError::from_stack_error`, which uses `public_message()` to avoid
/// leaking local paths or secret-store internals.
pub struct ApiResult<T>(pub Result<T, StackError>);

impl<T> From<Result<T, StackError>> for ApiResult<T> {
    fn from(value: Result<T, StackError>) -> Self {
        Self(value)
    }
}

impl<T> IntoResponse for ApiResult<T>
where
    T: Serialize,
{
    fn into_response(self) -> Response {
        match self.0 {
            Ok(data) => ApiSuccess::new(data).into_response(),
            Err(err) => err.into_response(),
        }
    }
}

impl IntoResponse for StackError {
    fn into_response(self) -> Response {
        let status = self.http_status();
        ApiError::from_stack_error(&self).into_response_with(status)
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
            path: "/home/alice/.config/acp-stack/acps-config.toml".into(),
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

    #[test]
    fn workspace_path_invalid_maps_to_400() {
        let err = StackError::WorkspacePathInvalid {
            reason: "contains ..".into(),
            requested: "../etc/passwd".into(),
        };
        assert_eq!(err.error_code(), "workspace.path_invalid");
        assert_eq!(err.http_status(), StatusCode::BAD_REQUEST);
        let env = ApiError::from_stack_error(&err);
        assert!(env.message.contains("contains .."), "got {}", env.message);
    }

    #[test]
    fn workspace_symlink_escape_maps_to_400() {
        let err = StackError::WorkspaceSymlinkEscape {
            requested: "outside".into(),
        };
        assert_eq!(err.error_code(), "workspace.symlink_escape");
        assert_eq!(err.http_status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn workspace_not_found_maps_to_404() {
        let err = StackError::WorkspaceNotFound {
            requested: "missing".into(),
        };
        assert_eq!(err.error_code(), "workspace.not_found");
        assert_eq!(err.http_status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn workspace_too_large_maps_to_413_with_limit() {
        let err = StackError::WorkspaceTooLarge { limit: 8_388_608 };
        assert_eq!(err.error_code(), "workspace.too_large");
        assert_eq!(err.http_status(), StatusCode::PAYLOAD_TOO_LARGE);
        let env = ApiError::from_stack_error(&err);
        assert!(env.message.contains("8388608"), "got {}", env.message);
    }

    #[test]
    fn workspace_upload_invalid_maps_to_400() {
        let err = StackError::WorkspaceUploadInvalid {
            reason: "missing path field",
        };
        assert_eq!(err.error_code(), "workspace.upload_invalid");
        assert_eq!(err.http_status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn workspace_io_failed_sanitizes_local_paths() {
        let err = StackError::WorkspaceIo {
            requested: "notes.md".into(),
            source: std::io::Error::other("/home/alice/secret"),
        };
        assert_eq!(err.error_code(), "workspace.io_failed");
        assert_eq!(err.http_status(), StatusCode::INTERNAL_SERVER_ERROR);
        let env = ApiError::from_stack_error(&err);
        assert!(
            !env.message.contains("/home/alice"),
            "leaked: {}",
            env.message
        );
        assert!(!env.message.contains("secret"), "leaked: {}", env.message);
    }

    #[test]
    fn workspace_encoding_invalid_maps_to_400() {
        let err = StackError::WorkspaceEncodingInvalid {
            reason: "encoding must be utf8 or base64",
        };
        assert_eq!(err.error_code(), "workspace.encoding_invalid");
        assert_eq!(err.http_status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn api_result_ok_renders_200_success_envelope() {
        let result: ApiResult<&str> = ApiResult(Ok("hello"));
        let response = result.into_response();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn api_result_err_renders_status_from_stack_error() {
        let err = StackError::MissingField { field: "api.bind" };
        let result: ApiResult<()> = ApiResult(Err(err));
        let response = result.into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn api_result_err_renders_500_for_server_faults() {
        let err = StackError::AgeKeyParse {
            path: "/tmp/age.key".into(),
            reason: "garbage",
        };
        let result: ApiResult<()> = ApiResult(Err(err));
        let response = result.into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn api_error_into_response_with_uses_provided_status() {
        let response = ApiError::new("auth.missing", "Missing Authorization header")
            .into_response_with(StatusCode::UNAUTHORIZED);
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
