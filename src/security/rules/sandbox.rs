//! Sandbox-availability rules: `runtime.sandbox_unavailable` and
//! `runtime.sandbox_available`.
//!
//! `acps serve` already refuses to start when a configured (non-`off`) sandbox
//! cannot run on the host, so a live daemon should not normally reach
//! `sandbox_unavailable`. The self-check still reports it for the keyless local
//! diagnostic, and emits the "off but capable" nudge when the operator left the
//! agent workload sharing the daemon's secrets on a host that could isolate it.

use crate::config::SandboxMode;
use crate::security::SecurityCheckInputs;
use crate::security::findings::SecurityFinding;

fn mode_label(mode: SandboxMode) -> &'static str {
    match mode {
        SandboxMode::Off => "off",
        SandboxMode::Unshare => "unshare",
        SandboxMode::Bwrap => "bwrap",
        SandboxMode::Custom => "custom",
    }
}

pub(in crate::security) fn check_sandbox(
    inputs: &SecurityCheckInputs<'_>,
    findings: &mut Vec<SecurityFinding>,
) {
    if let Some(reason) = inputs.sandbox_unavailable_reason.as_deref() {
        findings.push(
            SecurityFinding::critical(
                "runtime.sandbox_unavailable",
                &format!(
                    "configured sandbox mode `{}` cannot run on this host: {reason}",
                    mode_label(inputs.sandbox_mode),
                ),
            )
            .with_remediation(
                "Install the backend's prerequisites, switch `[workspace.sandbox].mode`, \
                 or set it to `off`."
                    .to_owned(),
            ),
        );
    } else if inputs.sandbox_off_but_capable {
        findings.push(
            SecurityFinding::warning(
                "runtime.sandbox_available",
                "agent sandbox is off but this host can run the `unshare` backend; the agent \
                 workload shares the daemon's access to secrets and config",
            )
            .with_remediation(
                "Set `[workspace.sandbox].mode = \"unshare\"` to isolate the agent harness \
                 and mediated shells."
                    .to_owned(),
            ),
        );
    }
}
