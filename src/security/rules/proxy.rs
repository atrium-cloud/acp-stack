//! `http.trust_proxy_without_trusted_proxies` rule. Trusting client-supplied
//! proxy headers without an allowlist lets any caller spoof the remote
//! address, so a daemon configured for proxy trust must also enumerate which
//! proxies it trusts.

use crate::security::SecurityCheckInputs;
use crate::security::findings::SecurityFinding;

pub(in crate::security) fn check_proxy(
    inputs: &SecurityCheckInputs<'_>,
    findings: &mut Vec<SecurityFinding>,
) {
    if inputs.http.trust_proxy_headers && inputs.http.trusted_proxies.is_empty() {
        findings.push(
            SecurityFinding::critical(
                "http.trust_proxy_without_trusted_proxies",
                "proxy headers are trusted but no trusted proxy allowlist is configured",
            )
            .with_remediation(
                "Populate `[security.http].trusted_proxies` with the addresses of the \
                 reverse proxies in front of the daemon, or set \
                 `trust_proxy_headers = false`.",
            ),
        );
    }
}
