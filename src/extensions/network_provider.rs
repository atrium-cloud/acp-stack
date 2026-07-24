//! The network-provider extension contract.
//!
//! Declaring a `network-provider` instance switches every sandboxed spawn
//! (agent harness and each mediated command alike) to a fresh, per-spawn
//! network namespace. With an empty provider argv the namespace is deny-all:
//! acp-stack configures nothing, not even loopback. All network policy — veth
//! devices, routes, DNS, gateways, proxies — belongs to the external provider
//! executable; acp-stack never configures interfaces, resolves DNS, or
//! inspects traffic.
//!
//! The provider wire contract (setup/teardown verbs, `ACPS_SANDBOX_NETWORK_*`
//! env vars, protocol version, timeouts, fail-closed exits) is implemented by
//! the supervisor mechanism in `crate::runtime::sandbox::supervise`; this
//! module owns the resolved policy the sandbox seam consumes.

use crate::config::{DEFAULT_NETWORK_PROVIDER_TIMEOUT, ExtensionConfig, SandboxProviderStderr};

/// A resolved `type = "network-provider"` extension instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkProviderExtension {
    /// The operator-chosen `[extensions.<name>]` key, used for diagnostics.
    pub name: String,
    /// Lifecycle provider argv. Empty means no provider: deny-all networking.
    pub provider: Vec<String>,
    /// Raw duration string; `None` means [`DEFAULT_NETWORK_PROVIDER_TIMEOUT`].
    pub provider_timeout: Option<String>,
    /// Where provider stderr goes. Stdout is always discarded.
    pub provider_stderr: SandboxProviderStderr,
}

impl NetworkProviderExtension {
    pub fn from_config(name: &str, extension: &ExtensionConfig) -> Self {
        Self {
            name: name.to_owned(),
            provider: extension.provider.clone(),
            provider_timeout: extension.provider_timeout.clone(),
            provider_stderr: extension.provider_stderr,
        }
    }

    pub fn provider_timeout_raw(&self) -> &str {
        self.provider_timeout
            .as_deref()
            .unwrap_or(DEFAULT_NETWORK_PROVIDER_TIMEOUT)
    }

    /// The `__sandbox-supervise` argv fragment carrying this instance's
    /// provider policy: timeout, stderr routing, and one `--provider-arg` per
    /// argv token. The sandbox wrapper appends the workload chain after it.
    pub fn supervise_argv_fragment(&self) -> Vec<String> {
        let mut out = vec![
            "--provider-timeout".to_owned(),
            self.provider_timeout_raw().to_owned(),
            "--provider-stderr".to_owned(),
            self.provider_stderr.as_str().to_owned(),
        ];
        for provider_argument in &self.provider {
            out.push("--provider-arg".to_owned());
            out.push(provider_argument.clone());
        }
        out
    }
}
