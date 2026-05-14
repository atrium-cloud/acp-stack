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
}

pub type Result<T> = std::result::Result<T, StackError>;
