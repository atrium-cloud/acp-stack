//! Sandbox-availability rules: `runtime.sandbox_unavailable` and
//! `runtime.sandbox_available`, including the "off but capable" nudge.

use crate::config::SandboxMode;
use crate::security::SecurityCheckInputs;
use crate::security::findings::SecurityFinding;

pub(in crate::security) fn check_sandbox(
    inputs: &SecurityCheckInputs<'_>,
    findings: &mut Vec<SecurityFinding>,
) {
    if let Some(reason) = inputs.sandbox_unavailable_reason.as_deref() {
        // Under `off` only a workload identity has prerequisites to miss.
        let remediation = if inputs.sandbox_mode == SandboxMode::Off {
            "Grant the runtime what `[workspace.sandbox].workload_user` requires, or remove \
             `workload_user`."
        } else {
            "Install the backend's prerequisites, switch `[workspace.sandbox].mode`, \
             or set it to `off`."
        };
        findings.push(
            SecurityFinding::critical(
                "runtime.sandbox_unavailable",
                &format!(
                    "configured sandbox mode `{}` cannot run on this host: {reason}",
                    inputs.sandbox_mode.as_str(),
                ),
            )
            .with_remediation(remediation.to_owned()),
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
