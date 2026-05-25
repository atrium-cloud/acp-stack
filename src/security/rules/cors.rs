//! `http.wildcard_origin_public_bind` rule. A wildcard CORS origin on a public
//! bind is a critical finding — anyone can hit the API from any origin.

use crate::security::SecurityCheckInputs;
use crate::security::findings::{SecurityFinding, bind_is_public};

pub(in crate::security) fn check_cors(
    inputs: &SecurityCheckInputs<'_>,
    findings: &mut Vec<SecurityFinding>,
) {
    let bind_is_public = bind_is_public(inputs.effective_bind);
    if bind_is_public
        && inputs
            .http
            .allowed_origins
            .iter()
            .any(|origin| origin == "*")
    {
        findings.push(
            SecurityFinding::critical(
                "http.wildcard_origin_public_bind",
                "wildcard CORS origin is configured on a public bind address",
            )
            .with_remediation(
                "Set `[security.http].allowed_origins` to an explicit allow-list of \
                 origins before exposing the bind publicly.",
            ),
        );
    }
}
