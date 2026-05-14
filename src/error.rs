use http::StatusCode;
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum StackError {
    #[error("HOME is not set; cannot resolve default config path")]
    HomeNotSet,

    #[error("failed to read config at {path}: {source}")]
    ConfigRead {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("failed to write config export at {path}: {source}")]
    ConfigWrite {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("failed to create directory {path}: {source}")]
    DirectoryCreate {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("failed to create file {path}: {source}")]
    FileCreate {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("failed to set owner-only permissions on {path}: {source}")]
    PermissionSet {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("failed to initialize config at {path}: {source}")]
    ConfigInitialize {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("config already exists at {path}; pass --force to replace it")]
    ConfigExists { path: PathBuf },

    #[error("import data was not valid base64: {source}")]
    ImportBase64Decode {
        #[source]
        source: base64::DecodeError,
    },

    #[error("imported config was not valid UTF-8: {source}")]
    ImportUtf8 {
        #[source]
        source: std::string::FromUtf8Error,
    },

    #[error("failed to remove {path}: {source}")]
    FileRemove {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("acps reset would delete the listed files; re-run with --yes to confirm")]
    ResetNotConfirmed,

    #[error("path {path} has no parent directory")]
    MissingParentDir { path: PathBuf },

    #[error("config TOML is invalid: {0}")]
    ConfigToml(#[from] toml::de::Error),

    #[error("failed to serialize canonical config TOML: {0}")]
    ConfigSerialize(#[from] toml::ser::Error),

    #[error("state database error: {0}")]
    State(#[from] rusqlite::Error),

    #[error("state schema version {found} is newer than supported version {supported}")]
    IncompatibleStateSchema { found: i64, supported: i64 },

    #[error("existing state table `{table}` is not managed by a recorded migration")]
    UnmanagedStateTable { table: &'static str },

    #[error("migration manifest is invalid: {0}")]
    MigrationManifestParse(toml::de::Error),

    #[error("migration manifest references id {id} but the binary has no SQL embedded for it")]
    UnknownMigrationId { id: i64 },

    #[error(
        "migration manifest ids must be strictly increasing positive integers; saw {id} after {previous}"
    )]
    InvalidManifestOrder { id: i64, previous: i64 },

    #[error(
        "state database is missing the required `{table}` table after migrations; the file may be corrupted"
    )]
    MissingMigratedTable { table: &'static str },

    #[error("event payload must be valid JSON text")]
    InvalidEventPayload,

    #[error("auth failure payload must be valid JSON text")]
    InvalidAuthFailurePayload,

    #[error("failed to read age key at {path}: {source}")]
    AgeKeyRead {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("failed to write age key at {path}: {source}")]
    AgeKeyWrite {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("age key at {path} is malformed: {reason}")]
    AgeKeyParse { path: PathBuf, reason: &'static str },

    #[error("failed to read secret store at {path}: {source}")]
    SecretStoreRead {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("failed to write secret store at {path}: {source}")]
    SecretStoreWrite {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("failed to encrypt secret store: {0}")]
    SecretStoreEncrypt(#[from] age::EncryptError),

    #[error("failed to decrypt secret store: {0}")]
    SecretStoreDecrypt(#[from] age::DecryptError),

    #[error("decrypted secret store could not be parsed as TOML: {0}")]
    SecretStorePlaintextParse(toml::de::Error),

    #[error("decrypted secret store plaintext was not valid UTF-8: {source}")]
    SecretStorePlaintextNotUtf8 {
        #[source]
        source: std::str::Utf8Error,
    },

    #[error("failed to serialize secret store plaintext: {0}")]
    SecretStorePlaintextSerialize(toml::ser::Error),

    #[error("secret `{name}` was not found in the secret store")]
    SecretNotFound { name: String },

    #[error(
        "secret store is non-empty but does not contain the session key reference `{name}`; reset and re-init"
    )]
    MissingSessionKey { name: String },

    #[error(
        "secret store is non-empty but does not contain the admin key reference `{name}`; reset and re-init"
    )]
    MissingAdminKey { name: String },

    #[error("failed to read stdin: {source}")]
    StdinRead { source: std::io::Error },

    #[error("missing required section `{section}`")]
    MissingSection { section: &'static str },

    #[error("{field} is required")]
    MissingField { field: &'static str },

    #[error("{field} is not valid when workspace.source.type is {source_type}")]
    InvalidWorkspaceSourceField {
        field: &'static str,
        source_type: &'static str,
    },

    #[error("{field} must be a socket address")]
    InvalidSocketAddress { field: &'static str },

    #[error("{field} must be greater than zero")]
    NonZeroRequired { field: &'static str },

    #[error("{field} must be absolute")]
    PathMustBeAbsolute { field: &'static str },

    #[error("workspace.source.type must be one of none, git, s3")]
    InvalidWorkspaceSourceType,

    #[error("agent.restart must be one of never, on-crash")]
    InvalidAgentRestart,

    #[error("agent.expected_sha256 must be exactly 64 lowercase hex characters")]
    InvalidExpectedSha256,

    #[error(
        "auth.session_key_ref and auth.admin_key_ref must be different names; aliasing them would collapse the session/admin boundary"
    )]
    AuthRefsNotDistinct,

    #[error(
        "secret `{name}` is the configured {kind} key reference; use `acps auth regenerate-session-key` or `acps reset --yes` instead of `acps secrets`"
    )]
    SecretReservedForAuth { name: String, kind: &'static str },

    #[error(
        "config import would change `[auth].{field}` from `{current}` to `{incoming}`; run `acps reset --yes` and re-init to rotate auth references"
    )]
    ImportChangesAuthRef {
        field: &'static str,
        current: String,
        incoming: String,
    },

    #[error("failed to bind {bind}: {source}")]
    ServeBind {
        bind: String,
        source: std::io::Error,
    },

    #[error("HTTP server error: {source}")]
    ServeIo {
        #[source]
        source: std::io::Error,
    },
}

impl StackError {
    /// Dotted-namespace code suitable for the HTTP error envelope at
    /// `docs/specs/api/api.md:20-42`. The set is intentionally coarse: it
    /// identifies the failed subsystem and broad failure mode, not every
    /// variant.
    pub fn error_code(&self) -> &'static str {
        use StackError::*;
        match self {
            HomeNotSet => "config.home_missing",
            ConfigRead { .. } => "config.read_failed",
            ConfigWrite { .. } => "config.write_failed",
            ConfigInitialize { .. } => "config.initialize_failed",
            ConfigExists { .. } => "config.exists",
            ConfigToml(_) | ConfigSerialize(_) => "config.invalid",
            ImportBase64Decode { .. } => "import.base64_invalid",
            ImportUtf8 { .. } => "import.utf8_invalid",
            DirectoryCreate { .. } => "io.directory_create_failed",
            FileCreate { .. } => "io.file_create_failed",
            FileRemove { .. } => "io.file_remove_failed",
            PermissionSet { .. } => "io.permission_set_failed",
            MissingParentDir { .. } => "io.missing_parent_dir",
            ResetNotConfirmed => "reset.not_confirmed",
            State(_) => "state.error",
            IncompatibleStateSchema { .. } => "state.incompatible_schema",
            UnmanagedStateTable { .. } => "state.unmanaged_table",
            MigrationManifestParse(_) => "state.migration_manifest_invalid",
            UnknownMigrationId { .. } => "state.unknown_migration_id",
            InvalidManifestOrder { .. } => "state.invalid_manifest_order",
            MissingMigratedTable { .. } => "state.missing_migrated_table",
            InvalidEventPayload => "state.invalid_event_payload",
            InvalidAuthFailurePayload => "state.invalid_auth_failure_payload",
            AgeKeyRead { .. } | SecretStoreRead { .. } => "secrets.read_failed",
            AgeKeyWrite { .. } | SecretStoreWrite { .. } => "secrets.write_failed",
            AgeKeyParse { .. } => "secrets.age_key_invalid",
            SecretStoreEncrypt(_) => "secrets.encrypt_failed",
            SecretStoreDecrypt(_) => "secrets.decrypt_failed",
            SecretStorePlaintextParse(_)
            | SecretStorePlaintextSerialize(_)
            | SecretStorePlaintextNotUtf8 { .. } => "secrets.plaintext_invalid",
            SecretNotFound { .. } => "secrets.not_found",
            MissingSessionKey { .. } => "auth.missing_session_key",
            MissingAdminKey { .. } => "auth.missing_admin_key",
            StdinRead { .. } => "io.stdin_read_failed",
            MissingSection { .. }
            | MissingField { .. }
            | InvalidWorkspaceSourceField { .. }
            | InvalidSocketAddress { .. }
            | NonZeroRequired { .. }
            | PathMustBeAbsolute { .. }
            | InvalidWorkspaceSourceType
            | InvalidAgentRestart
            | InvalidExpectedSha256
            | AuthRefsNotDistinct => "config.invalid",
            SecretReservedForAuth { .. } => "secrets.reserved_for_auth",
            ImportChangesAuthRef { .. } => "config.import_changes_auth_ref",
            ServeBind { .. } => "serve.bind_failed",
            ServeIo { .. } => "serve.io_error",
        }
    }

    /// Human-readable message safe to expose through the public HTTP API.
    /// `Display` remains intentionally detailed for CLI diagnostics and local
    /// logs; this method avoids leaking local filesystem paths, OS errors, or
    /// secret-store metadata to remote clients.
    pub fn public_message(&self) -> String {
        use StackError::*;
        match self {
            HomeNotSet => "HOME is not set".to_owned(),
            ConfigRead { .. } => "failed to read config".to_owned(),
            ConfigWrite { .. } => "failed to write config".to_owned(),
            ConfigInitialize { .. } => "failed to initialize config".to_owned(),
            ConfigExists { .. } => "config already exists".to_owned(),
            ConfigToml(_) => "config TOML is invalid".to_owned(),
            ConfigSerialize(_) => "failed to serialize config".to_owned(),
            ImportBase64Decode { .. } => "import data was not valid base64".to_owned(),
            ImportUtf8 { .. } => "imported config was not valid UTF-8".to_owned(),
            DirectoryCreate { .. } => "failed to create directory".to_owned(),
            FileCreate { .. } => "failed to create file".to_owned(),
            FileRemove { .. } => "failed to remove file".to_owned(),
            PermissionSet { .. } => "failed to set owner-only permissions".to_owned(),
            MissingParentDir { .. } => "path has no parent directory".to_owned(),
            ResetNotConfirmed => "reset requires --yes".to_owned(),
            State(_) => "state database error".to_owned(),
            IncompatibleStateSchema { .. } => {
                "state schema is newer than this binary supports".to_owned()
            }
            UnmanagedStateTable { table } => {
                format!("existing state table `{table}` is not managed by a recorded migration")
            }
            MigrationManifestParse(_) => "migration manifest is invalid".to_owned(),
            UnknownMigrationId { id } => {
                format!("migration manifest references unknown id {id}")
            }
            InvalidManifestOrder { .. } => {
                "migration manifest ids must be strictly increasing positive integers".to_owned()
            }
            MissingMigratedTable { table } => {
                format!("state database is missing required table `{table}`")
            }
            InvalidEventPayload => "event payload must be valid JSON text".to_owned(),
            InvalidAuthFailurePayload => "auth failure payload must be valid JSON text".to_owned(),
            AgeKeyRead { .. } | SecretStoreRead { .. } => {
                "failed to read secret material".to_owned()
            }
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
            MissingSessionKey { .. } => "secret store is missing session key reference".to_owned(),
            MissingAdminKey { .. } => "secret store is missing admin key reference".to_owned(),
            StdinRead { .. } => "failed to read stdin".to_owned(),
            MissingSection { section } => format!("missing required section `{section}`"),
            MissingField { field } => format!("{field} is required"),
            InvalidWorkspaceSourceField { field, source_type } => {
                format!("{field} is not valid when workspace.source.type is {source_type}")
            }
            InvalidSocketAddress { field } => format!("{field} must be a socket address"),
            NonZeroRequired { field } => format!("{field} must be greater than zero"),
            PathMustBeAbsolute { field } => format!("{field} must be absolute"),
            InvalidWorkspaceSourceType => {
                "workspace.source.type must be one of none, git, s3".to_owned()
            }
            InvalidAgentRestart => "agent.restart must be one of never, on-crash".to_owned(),
            InvalidExpectedSha256 => {
                "agent.expected_sha256 must be exactly 64 lowercase hex characters".to_owned()
            }
            AuthRefsNotDistinct => {
                "auth.session_key_ref and auth.admin_key_ref must be different names".to_owned()
            }
            SecretReservedForAuth { .. } => "secret is reserved for auth".to_owned(),
            ImportChangesAuthRef { .. } => {
                "config import would change auth key references".to_owned()
            }
            ServeBind { .. } => "failed to bind HTTP listener".to_owned(),
            ServeIo { .. } => "HTTP server error".to_owned(),
        }
    }

    /// HTTP status code for this error when rendered through the API envelope.
    /// Coarse mapping: client-provided invalid input is 4xx; failures the
    /// server hits internally (filesystem, sqlite, age decrypt) are 5xx.
    pub fn http_status(&self) -> StatusCode {
        use StackError::*;
        match self {
            // Client-supplied bad input
            ConfigToml(_)
            | ConfigSerialize(_)
            | ImportBase64Decode { .. }
            | ImportUtf8 { .. }
            | ResetNotConfirmed
            | InvalidEventPayload
            | InvalidAuthFailurePayload
            | MissingSection { .. }
            | MissingField { .. }
            | InvalidWorkspaceSourceField { .. }
            | InvalidSocketAddress { .. }
            | NonZeroRequired { .. }
            | PathMustBeAbsolute { .. }
            | InvalidWorkspaceSourceType
            | InvalidAgentRestart
            | InvalidExpectedSha256
            | AuthRefsNotDistinct
            | SecretReservedForAuth { .. }
            | ImportChangesAuthRef { .. } => StatusCode::BAD_REQUEST,
            // Not found / conflict
            SecretNotFound { .. } => StatusCode::NOT_FOUND,
            ConfigExists { .. } => StatusCode::CONFLICT,
            // Everything else is a server-side fault. Includes startup-time
            // issues (missing keys, unreadable secret store) that can surface
            // if a handler ever rebuilds state mid-flight.
            HomeNotSet
            | ConfigRead { .. }
            | ConfigWrite { .. }
            | ConfigInitialize { .. }
            | DirectoryCreate { .. }
            | FileCreate { .. }
            | FileRemove { .. }
            | PermissionSet { .. }
            | MissingParentDir { .. }
            | State(_)
            | IncompatibleStateSchema { .. }
            | UnmanagedStateTable { .. }
            | MigrationManifestParse(_)
            | UnknownMigrationId { .. }
            | InvalidManifestOrder { .. }
            | MissingMigratedTable { .. }
            | AgeKeyRead { .. }
            | AgeKeyWrite { .. }
            | AgeKeyParse { .. }
            | SecretStoreRead { .. }
            | SecretStoreWrite { .. }
            | SecretStoreEncrypt(_)
            | SecretStoreDecrypt(_)
            | SecretStorePlaintextParse(_)
            | SecretStorePlaintextSerialize(_)
            | SecretStorePlaintextNotUtf8 { .. }
            | MissingSessionKey { .. }
            | MissingAdminKey { .. }
            | StdinRead { .. }
            | ServeBind { .. }
            | ServeIo { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

pub type Result<T> = std::result::Result<T, StackError>;
