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

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ApiSuccess<T> {
    /// Always `true` on a success envelope; the discriminator against
    /// [`ApiErrorEnvelope`], whose `ok` is always `false`.
    #[schemars(extend("const" = true))]
    pub ok: bool,
    pub data: T,
}

impl<T> ApiSuccess<T> {
    pub fn new(data: T) -> Self {
        Self { ok: true, data }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ApiErrorEnvelope {
    /// Always `false` on an error envelope; the discriminator against
    /// [`ApiSuccess`], whose `ok` is always `true`.
    #[schemars(extend("const" = false))]
    pub ok: bool,
    pub error: ApiError,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ApiError {
    /// Stable machine-readable error identifier. An open, versioned set of
    /// dotted `<domain>.<condition>` strings (e.g. `config.invalid`,
    /// `request.invalid_param`, `agent.inference_5xx`). Not a closed enum —
    /// clients match known values and treat unknown codes as generic
    /// failures. See docs/specs/api/api.md and docs/specs/state-logging.md.
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

    /// Code + message for a bare HTTP status, used by the envelope-rewrapping
    /// middlewares that turn framework-generated responses (axum's 404/405
    /// fallbacks, body-limit 413s) into `{ok:false, ...}` bodies. Both the
    /// main API and the `acps init --serve` bootstrap server rewrap the same
    /// statuses, so the tables live here — on the type that owns the wire
    /// shape — rather than being duplicated per middleware where they can
    /// drift apart.
    pub fn for_status(status: StatusCode) -> Self {
        Self::new(error_code_for_status(status), message_for_status(status))
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

fn error_code_for_status(status: StatusCode) -> &'static str {
    match status {
        StatusCode::BAD_REQUEST => "request.invalid",
        StatusCode::UNAUTHORIZED => "auth.invalid",
        StatusCode::FORBIDDEN => "auth.forbidden",
        StatusCode::NOT_FOUND => "request.not_found",
        StatusCode::METHOD_NOT_ALLOWED => "request.method_not_allowed",
        StatusCode::PAYLOAD_TOO_LARGE => "request.too_large",
        StatusCode::UNSUPPORTED_MEDIA_TYPE => "request.unsupported_media_type",
        _ if status.is_server_error() => "server.internal_error",
        _ => "request.rejected",
    }
}

fn message_for_status(status: StatusCode) -> &'static str {
    match status {
        StatusCode::BAD_REQUEST => "bad request",
        StatusCode::UNAUTHORIZED => "authentication required",
        StatusCode::FORBIDDEN => "forbidden",
        StatusCode::NOT_FOUND => "not found",
        StatusCode::METHOD_NOT_ALLOWED => "method not allowed",
        StatusCode::PAYLOAD_TOO_LARGE => "request body exceeds configured size limit",
        StatusCode::UNSUPPORTED_MEDIA_TYPE => "unsupported media type",
        _ if status.is_server_error() => "internal server error",
        _ => "request rejected",
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

    #[test]
    fn for_status_emits_domain_prefixed_codes() {
        // Every published code carries a `<domain>.` prefix; the rewrapping
        // middlewares must not reintroduce bare identifiers for the statuses
        // axum generates on its own.
        let code = |status| ApiError::for_status(status).code;
        assert_eq!(code(StatusCode::BAD_REQUEST), "request.invalid");
        assert_eq!(code(StatusCode::UNAUTHORIZED), "auth.invalid");
        assert_eq!(code(StatusCode::FORBIDDEN), "auth.forbidden");
        assert_eq!(code(StatusCode::NOT_FOUND), "request.not_found");
        assert_eq!(
            code(StatusCode::METHOD_NOT_ALLOWED),
            "request.method_not_allowed"
        );
        assert_eq!(code(StatusCode::PAYLOAD_TOO_LARGE), "request.too_large");
        assert_eq!(
            code(StatusCode::UNSUPPORTED_MEDIA_TYPE),
            "request.unsupported_media_type"
        );
        assert_eq!(
            code(StatusCode::INTERNAL_SERVER_ERROR),
            "server.internal_error"
        );
        assert_eq!(code(StatusCode::BAD_GATEWAY), "server.internal_error");
        assert_eq!(code(StatusCode::CONFLICT), "request.rejected");
    }

    #[test]
    fn for_status_pairs_each_code_with_a_message() {
        let error = ApiError::for_status(StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(error.code, "request.method_not_allowed");
        assert_eq!(error.message, "method not allowed");
        assert!(error.details.is_empty());
    }
}
