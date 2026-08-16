//! Sandbox isolation and the data-declared extension seam.

use super::*;

/// Selects how the agent harness and mediated shells are isolated from the
/// daemon. The daemon always derives the set of its own sensitive paths to mask
/// (config dir, state dir) from its path helpers, so an operator cannot forget
/// to protect them; the fields below only add to or parameterize that.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    #[serde(default)]
    pub mode: SandboxMode,
    /// Wrapper argv for `mode = "custom"`: prepended to the harness command
    /// (e.g. `["systemd-run", "--scope", "-p", "..."]`). Required for `custom`,
    /// ignored otherwise.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub wrapper: Vec<String>,
    /// Extra absolute paths to mask (read-deny) on top of the daemon's own.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mask_paths: Vec<String>,
    /// Extra absolute paths the workload may read+write (e.g. bwrap binds)
    /// beyond the workspace root.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_paths: Vec<String>,
}

impl SandboxConfig {
    pub fn is_off(&self) -> bool {
        *self == SandboxConfig::default()
    }
}

/// Isolation mechanism. `unshare` requires the daemon to hold `CAP_SYS_ADMIN`
/// (privileged container); `bwrap` requires unprivileged user namespaces;
/// `custom` delegates to an operator-supplied [`SandboxConfig::wrapper`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SandboxMode {
    #[default]
    Off,
    Unshare,
    Bwrap,
    Custom,
}

pub const DEFAULT_NETWORK_PROVIDER_TIMEOUT: &str = "30s";

/// A typed, data-declared extension seam. An `[extensions.<name>]` table
/// declares one instance of an acp-stack-defined extension type; acp-stack
/// supervises or serves the type's generic contract and never learns the
/// extension's semantics. The struct is flat across all types; the extensions
/// validator rejects fields that do not belong to the declared `type`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionConfig {
    #[serde(rename = "type")]
    pub extension_type: ExtensionType,
    /// `network-provider` only. Lifecycle provider argv, invoked as
    /// `<exe> setup|teardown <args...>` around each network-isolated spawn.
    /// Empty means no provider: the namespace stays deny-all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider: Vec<String>,
    /// `network-provider` only. Duration string applied independently to
    /// provider setup and teardown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_timeout: Option<String>,
    /// `network-provider` only. Where provider stderr goes: the daemon's
    /// stderr diagnostic channel, or discarded. Stdout is always discarded.
    #[serde(default, skip_serializing_if = "SandboxProviderStderr::is_default")]
    pub provider_stderr: SandboxProviderStderr,
    /// `managed-state` only. The state contract the namespace applies;
    /// `provider-credential` is the only capability today.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability: Option<String>,
}

/// The extension types acp-stack defines. Declaring a `network-provider`
/// instance switches every sandboxed spawn to an isolated network namespace
/// whose policy belongs to the external provider executable; declaring a
/// `managed-state` instance grants an external orchestrator ownership of a
/// named state namespace via the admin apply endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExtensionType {
    NetworkProvider,
    ManagedState,
}

impl ExtensionType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ExtensionType::NetworkProvider => "network-provider",
            ExtensionType::ManagedState => "managed-state",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SandboxProviderStderr {
    #[default]
    Daemon,
    Null,
}

impl SandboxProviderStderr {
    fn is_default(&self) -> bool {
        *self == SandboxProviderStderr::default()
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            SandboxProviderStderr::Daemon => "daemon",
            SandboxProviderStderr::Null => "null",
        }
    }
}
