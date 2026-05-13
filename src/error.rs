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

    #[error("config TOML is invalid: {0}")]
    ConfigToml(#[from] toml::de::Error),

    #[error("failed to serialize canonical config TOML: {0}")]
    ConfigSerialize(#[from] toml::ser::Error),

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
}

pub type Result<T> = std::result::Result<T, StackError>;
