//! Config / filesystem / import error helpers.
//!
//! Covers every variant whose `error_code()` is in the `config.*`, `import.*`,
//! `io.*`, or `reset.*` namespaces. The `_ => None` arms let the top-level
//! dispatcher fall through to the next domain.

use http::StatusCode;

use super::StackError;

pub(super) fn error_code(err: &StackError) -> Option<&'static str> {
    use StackError::*;
    Some(match err {
        HomeNotSet => "config.home_missing",
        ConfigRead { .. } => "config.read_failed",
        ConfigWrite { .. } => "config.write_failed",
        ConfigInitialize { .. } => "config.initialize_failed",
        ConfigExists { .. } => "config.exists",
        ConfigToml(_) | ConfigSerialize(_) => "config.invalid",
        ImportBase64Decode { .. } => "import.base64_invalid",
        ImportUtf8 { .. } => "import.utf8_invalid",
        NativeAgentConfig { code } => code,
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
        | UrlMustBeHttp { .. }
        | UrlMustBeHttps { .. }
        | InvalidPermissionsMode
        | InvalidDurationField { .. }
        | InvalidEnvName { .. } => "config.invalid",
        ImportTooLarge { .. } => "import.too_large",
        UnsupportedConfigVersion { .. } => "config.unsupported_version",
        SecretRefLooksLikeValue { .. } => "config.invalid",
        _ => return None,
    })
}

pub(super) fn public_message(err: &StackError) -> Option<String> {
    use StackError::*;
    Some(match err {
        HomeNotSet => "HOME is not set".to_owned(),
        ConfigRead { .. } => "failed to read config".to_owned(),
        ConfigWrite { .. } => "failed to write config".to_owned(),
        ConfigInitialize { .. } => "failed to initialize config".to_owned(),
        ConfigExists { .. } => "config already exists".to_owned(),
        ConfigToml(_) => "config TOML is invalid".to_owned(),
        ConfigSerialize(_) => "failed to serialize config".to_owned(),
        ImportBase64Decode { .. } => "import data was not valid base64".to_owned(),
        ImportUtf8 { .. } => "imported config was not valid UTF-8".to_owned(),
        NativeAgentConfig { .. } | NativeAgentConfigOperationFailed { .. } => {
            "native Agent config import failed".to_owned()
        }
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
        UrlMustBeHttp { field } => format!("{field} must start with http:// or https://"),
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
        _ => return None,
    })
}

pub(super) fn http_status(err: &StackError) -> Option<StatusCode> {
    use StackError::*;
    Some(match err {
        NativeAgentConfig {
            code: "native_config_operation_not_found",
        } => StatusCode::NOT_FOUND,
        NativeAgentConfig {
            code:
                "native_config_rollback_conflict"
                | "native_config_rollback_expired"
                | "native_config_base_config_changed"
                | "native_config_operation_in_progress"
                | "native_config_journal_conflict",
        } => StatusCode::CONFLICT,
        NativeAgentConfig {
            code: "native_config_too_large" | "native_config_normalized_too_large",
        } => StatusCode::PAYLOAD_TOO_LARGE,
        NativeAgentConfig {
            code:
                "native_config_journal_invalid"
                | "native_config_journal_too_large"
                | "native_config_journal_too_many"
                | "native_config_rollback_failed"
                | "native_config_claude_state_invalid",
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
        | UrlMustBeHttp { .. }
        | UrlMustBeHttps { .. }
        | InvalidPermissionsMode
        | InvalidDurationField { .. }
        | InvalidEnvName { .. } => StatusCode::BAD_REQUEST,
        ConfigExists { .. } => StatusCode::CONFLICT,
        HomeNotSet
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
        SecretRefLooksLikeValue { .. } => StatusCode::BAD_REQUEST,
        _ => return None,
    })
}
