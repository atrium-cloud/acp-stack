//! Cloudflare Tunnel posture checks. When `[edge.cloudflare]` declares
//! `exposure = "tunnel"`, the daemon expects to live behind a local
//! cloudflared and accept traffic only via loopback with localhost trusted
//! proxies. This module emits findings for each posture mismatch and for
//! recent traffic that bypassed the expected path.

use crate::security::SecurityCheckInputs;
use crate::security::findings::{SecurityFinding, bind_is_public};

pub(in crate::security) fn check_cloudflare(
    inputs: &SecurityCheckInputs<'_>,
    findings: &mut Vec<SecurityFinding>,
) {
    let Some(cloudflare) = inputs.cloudflare else {
        return;
    };
    if !cloudflare.enabled || cloudflare.exposure != "tunnel" {
        return;
    }

    let bind_is_public = bind_is_public(inputs.effective_bind);
    if bind_is_public {
        findings.push(
            SecurityFinding::critical(
                "edge.cloudflare.public_bind_tunnel",
                "Cloudflare Tunnel mode is configured but the API bind is public",
            )
            .with_remediation(
                "Set `[api].bind = \"127.0.0.1:7700\"` so only local \
                 cloudflared can reach the daemon.",
            ),
        );
    }
    if inputs.http.allowed_origins.is_empty()
        || inputs
            .http
            .allowed_origins
            .iter()
            .any(|origin| origin == "*")
    {
        findings.push(
            SecurityFinding::critical(
                "edge.cloudflare.unsafe_origins",
                "Cloudflare Tunnel mode requires explicit non-wildcard allowed origins",
            )
            .with_remediation(
                "Set `[security.http].allowed_origins` to the exact \
                 `https://<hostname>` origin served by Cloudflare.",
            ),
        );
    }
    let has_localhost_proxy = inputs
        .http
        .trusted_proxies
        .iter()
        .any(|proxy| proxy == "127.0.0.1")
        && inputs
            .http
            .trusted_proxies
            .iter()
            .any(|proxy| proxy == "::1");
    if !inputs.http.trust_proxy_headers || !has_localhost_proxy {
        findings.push(
            SecurityFinding::critical(
                "edge.cloudflare.missing_local_trusted_proxies",
                "Cloudflare Tunnel mode requires localhost trusted proxies",
            )
            .with_remediation(
                "Set `[security.http].trust_proxy_headers = true` and \
                 `[security.http].trusted_proxies = [\"127.0.0.1\", \"::1\"]`.",
            ),
        );
    }
    if cloudflare.cloudflared_deployment == "host" && !inputs.cloudflared_available {
        findings.push(
            SecurityFinding::warning(
                "edge.cloudflare.cloudflared_missing",
                "Cloudflare Tunnel host deployment is configured but cloudflared is unavailable",
            )
            .with_remediation(
                "Install `cloudflared` on PATH, or set \
                 `cloudflared_deployment = \"docker\"` / \"external\" if it runs outside \
                 the daemon host.",
            ),
        );
    }
    if inputs.recent_missing_cloudflare_header_requests > 0 {
        findings.push(
            SecurityFinding::warning(
                "edge.cloudflare.headers_missing",
                "recent trusted-proxy requests were missing Cloudflare headers",
            )
            .with_remediation(
                "Verify the public hostname routes through Cloudflare Tunnel and that \
                 Cloudflare visitor IP/location headers have not been stripped.",
            ),
        );
    }
    if inputs.recent_direct_cloudflare_mode_requests > 0 {
        findings.push(
            SecurityFinding::critical(
                "edge.cloudflare.direct_public_requests",
                "recent requests reached the daemon without the trusted Cloudflare proxy path",
            )
            .with_remediation(
                "Keep the daemon bound to loopback and ensure firewall or container \
                 networking prevents direct public access.",
            ),
        );
    }
}
