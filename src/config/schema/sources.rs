//! Workspace layout and the code/data sources init seeds into it.

use super::*;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    pub root: String,
    pub uploads: String,
    pub default_shell: String,
    pub runtime_user: String,
    pub max_file_bytes: u64,
    /// Isolation backend that the agent harness and mediated shells run inside.
    /// Default `off` preserves single-process behavior; other modes wrap each
    /// spawn so the workload cannot read the daemon's secrets/state or reach its
    /// control socket. See [`SandboxConfig`].
    #[serde(default, skip_serializing_if = "SandboxConfig::is_off")]
    pub sandbox: SandboxConfig,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub code_sources: Vec<CodeSourceConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub data_sources: Vec<DataSourceConfig>,
}

/// Source for code that init should seed under `<workspace.root>/usr/code/`.
///
/// The only `type` value today is `git`. The schema is shaped as an enum so
/// that additional code-source kinds can be added without invalidating
/// existing configs, but loaders reject unknown values fail-fast.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeSourceConfig {
    #[serde(rename = "type")]
    pub source_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_ref: Option<String>,
    /// Override the derived `<repo-name>` directory under
    /// `<workspace.root>/usr/code/`. Defaults to the trailing path segment of
    /// the repository URL with any `.git` suffix stripped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Source for arbitrary data that init should seed under
/// `<workspace.root>/usr/data/`. `type` is one of `local`, `https`, or `s3`;
/// the other fields are required-or-rejected based on the selected type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DataSourceConfig {
    #[serde(rename = "type")]
    pub source_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    // local
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,

    // https
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_download_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_extracted_bytes: Option<u64>,

    // s3
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bucket: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_key_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_key_ref: Option<String>,
}
