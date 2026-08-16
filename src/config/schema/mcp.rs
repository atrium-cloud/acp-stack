//! MCP server declarations and their HTTP header secret plumbing.

use super::*;

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpConfig {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub servers: Vec<McpServerConfig>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum McpServerConfig {
    Stdio(McpStdioServer),
    Http(McpHttpServer),
}

impl McpServerConfig {
    pub fn name(&self) -> &str {
        match self {
            McpServerConfig::Stdio(s) => &s.name,
            McpServerConfig::Http(s) => &s.name,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpStdioServer {
    pub name: String,
    pub command: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpHttpServer {
    pub name: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<HttpHeaderRef>,
}

/// One MCP HTTP header, valued either by a whole-value secret ref
/// (`value_ref`) or by a `${NAME}`-interpolated template (`value`).
/// Exactly-one is enforced in validation rather than serde so TOML errors
/// stay readable; `source()` is the runtime accessor that upholds it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpHeaderRef {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderValueSource<'a> {
    Ref(&'a str),
    Template(&'a str),
}

impl HttpHeaderRef {
    pub fn from_ref(name: impl Into<String>, value_ref: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value_ref: Some(value_ref.into()),
            value: None,
        }
    }

    pub fn from_template(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value_ref: None,
            value: Some(value.into()),
        }
    }

    /// Secret ref names this header depends on, ignoring syntax errors.
    /// For report-only callers (health, init prompt collection) that must
    /// never fail on a config that bypassed validation.
    pub fn ref_names_lossy(&self) -> Vec<String> {
        match (self.value_ref.as_deref(), self.value.as_deref()) {
            (Some(value_ref), None) => vec![value_ref.to_owned()],
            (None, Some(template)) => crate::config::secret_template::ref_names_lossy(template),
            _ => Vec::new(),
        }
    }

    pub fn source(&self) -> Result<HeaderValueSource<'_>, StackError> {
        match (self.value_ref.as_deref(), self.value.as_deref()) {
            (Some(value_ref), None) => Ok(HeaderValueSource::Ref(value_ref)),
            (None, Some(template)) => Ok(HeaderValueSource::Template(template)),
            _ => Err(StackError::InvalidHeaderValueSource {
                header: self.name.clone(),
            }),
        }
    }
}
