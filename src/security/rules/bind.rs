//! `api.public_bind` rule. Flags binds on unspecified IPs (`0.0.0.0`/`[::]`)
//! unless the daemon is running on Railway's managed runtime, where the
//! platform terminates traffic at its edge.

use crate::security::SecurityCheckInputs;
use crate::security::findings::{SecurityFinding, bind_is_public};

pub(in crate::security) fn check_bind(
    inputs: &SecurityCheckInputs<'_>,
    findings: &mut Vec<SecurityFinding>,
) {
    let bind_is_public = bind_is_public(inputs.effective_bind);
    if bind_is_public && !inputs.railway_platform {
        findings.push(
            SecurityFinding::warning(
                "api.public_bind",
                "API bind address listens on all interfaces",
            )
            .with_remediation(
                "Bind to a loopback or private interface, or front the daemon with a \
                 reverse proxy that terminates TLS and enforces auth before traffic \
                 reaches `acps`.",
            ),
        );
    }
}
