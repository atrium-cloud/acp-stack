//! Config / filesystem / import error helpers, covering the `config.*`,
//! `import.*`, `io.*`, and `reset.*` namespaces.

use http::StatusCode;
use serde_json::{Map, Value};

use super::StackError;
use crate::redaction::{bounded, redact_text, strip_serde_value_literals};

// CONSTANTS

const CONFIG_TOML_REASON_MAX_BYTES: usize = 1024;
/// `toml::de::Error`'s Display opens with this header when it knows where the error is.
const TOML_POSITION_PREFIX: &str = "TOML parse error at line ";
const TOML_POSITION_COLUMN_SEPARATOR: &str = ", column ";

/// Position and parser message for an invalid config TOML. The body can carry secrets, so the
/// source line the Display quotes stays out and value literals in the message are replaced.
pub(super) fn public_details(err: &StackError) -> Option<Map<String, Value>> {
    let StackError::ConfigToml(error) = err else {
        return None;
    };
    let mut details = Map::new();
    if let Some((line, column)) = toml_error_position(error) {
        details.insert("line".to_owned(), Value::from(line));
        details.insert("column".to_owned(), Value::from(column));
    }
    let reason = strip_serde_value_literals(error.message());
    details.insert(
        "reason".to_owned(),
        Value::String(bounded(&redact_text(&reason), CONFIG_TOML_REASON_MAX_BYTES).into_owned()),
    );
    Some(details)
}

/// `toml::de::Error` exposes the span but not the input it indexes, so line and column are read
/// back from the Display header.
fn toml_error_position(error: &toml::de::Error) -> Option<(u64, u64)> {
    let rendered = error.to_string();
    let header = rendered.lines().next()?;
    let (line, column) = header
        .strip_prefix(TOML_POSITION_PREFIX)?
        .split_once(TOML_POSITION_COLUMN_SEPARATOR)?;
    Some((line.parse().ok()?, column.parse().ok()?))
}

pub(super) fn error_code(err: &StackError) -> Option<&'static str> {
    use StackError::*;
    Some(match err {
        HomeNotSet => "config.home_missing",
        HomeNotIsolated { .. } => "config.home_not_isolated",
        FixtureEgressRefused { .. } => "config.fixture_egress_refused",
        ConfigRead { .. } => "config.read_failed",
        ConfigWrite { .. } => "config.write_failed",
        ConfigInitialize { .. } => "config.initialize_failed",
        ConfigExists { .. } => "config.exists",
        ConfigToml(_) | ConfigSerialize(_) => "config.invalid",
        ImportBase64Decode { .. } => "import.base64_invalid",
        ImportUtf8 { .. } => "import.utf8_invalid",
        NativeAgentConfig { code } => code,
        NativeAgentConfigDetailed { code, .. } => code,
        DirectoryCreate { .. } => "io.directory_create_failed",
        FileCreate { .. } => "io.file_create_failed",
        FileRemove { .. } => "io.file_remove_failed",
        PermissionSet { .. } => "io.permission_set_failed",
        MissingParentDir { .. } => "io.missing_parent_dir",
        ResetNotConfirmed => "reset.not_confirmed",
        StdinRead { .. } => "io.stdin_read_failed",
        MissingSection { .. }
        | MissingField { .. }
        | InvalidConfigFieldForType { .. }
        | InvalidSocketAddress { .. }
        | NonZeroRequired { .. }
        | PathMustBeAbsolute { .. }
        | PathContainsParentDir { .. }
        | InvalidAgentRestart
        | InvalidExpectedSha256
        | InvalidAgentInstallType
        | UrlMustBeHttps { .. }
        | InvalidPermissionsMode
        | InvalidDurationField { .. }
        | InvalidEnvName { .. } => "config.invalid",
        ImportTooLarge { .. } => "import.too_large",
        UnsupportedConfigVersion { .. } => "config.unsupported_version",
        SecretRefLooksLikeValue { .. } | InvalidHeaderValueSource { .. } => "config.invalid",
        _ => return None,
    })
}

pub(super) fn public_message(err: &StackError) -> Option<String> {
    use StackError::*;
    Some(match err {
        HomeNotSet => "HOME is not set".to_owned(),
        HomeNotIsolated { .. } => "fixture build refuses a HOME outside the temp dir".to_owned(),
        FixtureEgressRefused { .. } => "fixture build refuses a non-loopback URL".to_owned(),
        ConfigRead { .. } => "failed to read config".to_owned(),
        ConfigWrite { .. } => "failed to write config".to_owned(),
        ConfigInitialize { .. } => "failed to initialize config".to_owned(),
        ConfigExists { .. } => "config already exists".to_owned(),
        ConfigToml(_) => "config TOML is invalid".to_owned(),
        ConfigSerialize(_) => "failed to serialize config".to_owned(),
        ImportBase64Decode { .. } => "import data was not valid base64".to_owned(),
        ImportUtf8 { .. } => "imported config was not valid UTF-8".to_owned(),
        NativeAgentConfig { .. }
        | NativeAgentConfigDetailed { .. }
        | NativeAgentConfigOperationFailed { .. } => "native Agent config import failed".to_owned(),
        DirectoryCreate { .. } => "failed to create directory".to_owned(),
        FileCreate { .. } => "failed to create file".to_owned(),
        FileRemove { .. } => "failed to remove file".to_owned(),
        PermissionSet { .. } => "failed to set owner-only permissions".to_owned(),
        MissingParentDir { .. } => "path has no parent directory".to_owned(),
        ResetNotConfirmed => "reset requires --yes".to_owned(),
        StdinRead { .. } => "failed to read stdin".to_owned(),
        MissingSection { section } => format!("missing required section `{section}`"),
        MissingField { field } => format!("{field} is required"),
        InvalidConfigFieldForType {
            field,
            type_field,
            type_value,
        } => format!("{field} is not valid when {type_field} is {type_value}"),
        InvalidSocketAddress { field } => format!("{field} must be a socket address"),
        NonZeroRequired { field } => format!("{field} must be greater than zero"),
        PathMustBeAbsolute { field } => format!("{field} must be absolute"),
        PathContainsParentDir { field } => format!("{field} must not contain `..` segments"),
        InvalidAgentRestart => "agent.restart must be one of never, on-crash".to_owned(),
        InvalidExpectedSha256 => {
            "agent.expected_sha256 must be exactly 64 lowercase hex characters".to_owned()
        }
        InvalidAgentInstallType => {
            "agent.install.type must be `shell` (the only operator-facing install type)".to_owned()
        }
        UrlMustBeHttps { field } => format!("{field} must start with https://"),
        InvalidPermissionsMode => {
            "permissions.mode must be one of auto, supervised, locked".to_owned()
        }
        InvalidDurationField { field } => {
            format!("{field} must be a duration like \"10m\", \"5s\", \"1d\", \"4w\", or \"100ms\"")
        }
        InvalidEnvName { name } => {
            format!("env variable name `{name}` is not a valid POSIX identifier")
        }
        ImportTooLarge { limit, .. } => {
            format!("config import exceeds the {limit}-byte size limit")
        }
        UnsupportedConfigVersion { version } => {
            format!("unsupported config version {version}; this binary only supports version 1")
        }
        SecretRefLooksLikeValue { field, .. } => format!(
            "secret ref at `{field}` looks like an inline secret value rather than a reference name"
        ),
        InvalidHeaderValueSource { header } => {
            format!("mcp header `{header}` must set exactly one of `value_ref` or `value`")
        }
        _ => return None,
    })
}

pub(super) fn http_status(err: &StackError) -> Option<StatusCode> {
    use StackError::*;
    Some(match err {
        NativeAgentConfigDetailed { code, .. } => {
            return http_status(&NativeAgentConfig { code });
        }
        NativeAgentConfig {
            code: "agent.native_config_operation_not_found",
        } => StatusCode::NOT_FOUND,
        NativeAgentConfig {
            code:
                "agent.native_config_rollback_conflict"
                | "agent.native_config_rollback_expired"
                | "agent.native_config_base_config_changed"
                | "agent.native_config_operation_in_progress"
                | "agent.native_config_journal_conflict",
        } => StatusCode::CONFLICT,
        NativeAgentConfig {
            code: "agent.native_config_too_large" | "agent.native_config_normalized_too_large",
        } => StatusCode::PAYLOAD_TOO_LARGE,
        NativeAgentConfig {
            code:
                "agent.native_config_journal_invalid"
                | "agent.native_config_journal_too_large"
                | "agent.native_config_journal_too_many"
                | "agent.native_config_rollback_failed"
                | "agent.native_config_claude_state_invalid",
        } => StatusCode::INTERNAL_SERVER_ERROR,
        ConfigToml(_)
        | ConfigSerialize(_)
        | ImportBase64Decode { .. }
        | ImportUtf8 { .. }
        | NativeAgentConfig { .. }
        | NativeAgentConfigOperationFailed { .. }
        | ResetNotConfirmed
        | MissingSection { .. }
        | MissingField { .. }
        | InvalidConfigFieldForType { .. }
        | InvalidSocketAddress { .. }
        | NonZeroRequired { .. }
        | PathMustBeAbsolute { .. }
        | PathContainsParentDir { .. }
        | InvalidAgentRestart
        | InvalidExpectedSha256
        | InvalidAgentInstallType
        | UrlMustBeHttps { .. }
        | InvalidPermissionsMode
        | InvalidDurationField { .. }
        | InvalidEnvName { .. } => StatusCode::BAD_REQUEST,
        ConfigExists { .. } => StatusCode::CONFLICT,
        HomeNotSet
        | HomeNotIsolated { .. }
        | FixtureEgressRefused { .. }
        | ConfigRead { .. }
        | ConfigWrite { .. }
        | ConfigInitialize { .. }
        | DirectoryCreate { .. }
        | FileCreate { .. }
        | FileRemove { .. }
        | PermissionSet { .. }
        | MissingParentDir { .. }
        | StdinRead { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        ImportTooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
        UnsupportedConfigVersion { .. } => StatusCode::BAD_REQUEST,
        SecretRefLooksLikeValue { .. } | InvalidHeaderValueSource { .. } => StatusCode::BAD_REQUEST,
        _ => return None,
    })
}
