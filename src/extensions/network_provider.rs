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

use std::collections::{BTreeMap, HashMap};

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
    /// Environment injected into every workload spawned inside the namespace.
    /// Applied by [`apply_workload_env`]; never reaches the provider process.
    pub workload_env: BTreeMap<String, String>,
}

impl NetworkProviderExtension {
    pub fn from_config(name: &str, extension: &ExtensionConfig) -> Self {
        Self {
            name: name.to_owned(),
            provider: extension.provider.clone(),
            provider_timeout: extension.provider_timeout.clone(),
            provider_stderr: extension.provider_stderr,
            workload_env: extension.workload_env.clone(),
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

/// Overlay a network-provider instance's `workload_env` onto a workload's
/// resolved environment. Called at every seam that spawns inside the provider's
/// namespace, *after* the caller's own env, so the declaration wins on conflict
/// — including over runtime-managed rewrites such as the Kimi/Hermes launch
/// keys. That precedence is deliberate: the declaration is infrastructure
/// config from the operator that owns the namespace, and a workload whose
/// egress env is half-overridden reaches no network at all.
///
/// `PATH` and `HOME` are refused at config load, so they cannot appear here;
/// the guard below keeps the invariant local to the seam that relies on it.
pub fn apply_workload_env(
    env: &mut HashMap<String, String>,
    provider: Option<&NetworkProviderExtension>,
) {
    let Some(provider) = provider else {
        return;
    };
    for (name, value) in &provider.workload_env {
        if matches!(name.as_str(), "PATH" | "HOME") {
            tracing::warn!(
                extension = %provider.name,
                name = %name,
                "refusing to inject `{name}` from workload_env: runtime-managed",
            );
            continue;
        }
        env.insert(name.clone(), value.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider_with(entries: &[(&str, &str)]) -> NetworkProviderExtension {
        NetworkProviderExtension {
            name: "egress".to_owned(),
            provider: vec!["/usr/bin/provider".to_owned()],
            provider_timeout: None,
            provider_stderr: SandboxProviderStderr::Daemon,
            workload_env: entries
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
        }
    }

    #[test]
    fn workload_env_wins_over_caller_env() {
        let mut env = HashMap::from([
            ("HTTPS_PROXY".to_owned(), "http://stale:1".to_owned()),
            ("KEEP".to_owned(), "kept".to_owned()),
        ]);

        apply_workload_env(
            &mut env,
            Some(&provider_with(&[
                ("HTTPS_PROXY", "http://127.0.0.1:3128"),
                ("NO_PROXY", "localhost,127.0.0.1"),
            ])),
        );

        assert_eq!(env["HTTPS_PROXY"], "http://127.0.0.1:3128");
        assert_eq!(env["NO_PROXY"], "localhost,127.0.0.1");
        assert_eq!(env["KEEP"], "kept");
    }

    #[test]
    fn absent_provider_leaves_env_untouched() {
        let mut env = HashMap::from([("KEEP".to_owned(), "kept".to_owned())]);
        apply_workload_env(&mut env, None);
        assert_eq!(env, HashMap::from([("KEEP".to_owned(), "kept".to_owned())]));
    }

    #[test]
    fn reserved_names_are_refused_at_the_seam() {
        let mut env = HashMap::new();
        apply_workload_env(
            &mut env,
            Some(&provider_with(&[
                ("PATH", "/attacker/bin"),
                ("HOME", "/attacker"),
                ("HTTP_PROXY", "http://127.0.0.1:3128"),
            ])),
        );

        assert!(!env.contains_key("PATH"));
        assert!(!env.contains_key("HOME"));
        assert_eq!(env["HTTP_PROXY"], "http://127.0.0.1:3128");
    }

    #[test]
    fn workload_env_never_reaches_provider_argv() {
        let fragment = provider_with(&[("HTTPS_PROXY", "http://127.0.0.1:3128")])
            .supervise_argv_fragment()
            .join(" ");
        assert!(!fragment.contains("HTTPS_PROXY"), "{fragment}");
        assert!(!fragment.contains("3128"), "{fragment}");
    }
}
