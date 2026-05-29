//! Runtime-identity rules: `runtime.user_mismatch` and
//! `runtime.workspace_not_writable`.
//!
//! Both findings turn on the daemon's effective uid versus the configured
//! workspace setup. The user-mismatch check compares `process_euid` against
//! the uid resolved from `workspace.runtime_user`; the workspace-writable
//! check probes whether that running uid can actually create files in the
//! configured workspace root.

use crate::security::SecurityCheckInputs;
use crate::security::findings::{SecurityFinding, shell_quote};

pub(in crate::security) fn check_runtime_user(
    inputs: &SecurityCheckInputs<'_>,
    findings: &mut Vec<SecurityFinding>,
) {
    if let Some(uid) = inputs.runtime_user_uid {
        if uid != inputs.process_euid {
            // Root execution is dev-only, so remediation never tells an
            // operator to relaunch the production daemon as root.
            let remediation = if uid == 0 {
                format!(
                    "Update `[workspace].runtime_user` to an unprivileged \
                     user that matches the launching uid {euid}; root \
                     execution is reserved for development.",
                    euid = inputs.process_euid,
                )
            } else {
                format!(
                    "Relaunch the daemon as '{name}' (check the systemd \
                     `User=` directive or the container `USER` instruction), \
                     or update `[workspace].runtime_user` so it matches the \
                     launching uid {euid}.",
                    name = inputs.runtime_user_name,
                    euid = inputs.process_euid,
                )
            };
            findings.push(
                SecurityFinding::warning(
                    "runtime.user_mismatch",
                    &format!(
                        "daemon euid {euid} does not match configured runtime_user \
                         '{name}' (uid {uid})",
                        euid = inputs.process_euid,
                        name = inputs.runtime_user_name,
                    ),
                )
                .with_remediation(remediation),
            );
        }
    } else {
        findings.push(
            SecurityFinding::warning(
                "runtime.user_unresolved",
                &format!(
                    "configured runtime_user '{}' does not resolve to a local uid",
                    inputs.runtime_user_name,
                ),
            )
            .with_remediation(format!(
                "Create the configured user '{name}', or update `[workspace].runtime_user` \
                 to the user that launches the daemon.",
                name = inputs.runtime_user_name,
            )),
        );
    }

    if !inputs.workspace_writable {
        findings.push(
            SecurityFinding::critical(
                "runtime.workspace_not_writable",
                &format!(
                    "workspace root {root} is not writable by the running daemon \
                     (uid {euid})",
                    root = inputs.workspace_root,
                    euid = inputs.process_euid,
                ),
            )
            // The probe runs as the daemon's effective uid (see
            // `ownership::workspace_writable`), so the hint must reference
            // that uid — not `runtime_user_name`, which can resolve to a
            // different uid (`runtime.user_mismatch` would fire separately
            // and the operator picks which side to fix).
            .with_remediation(format!(
                "Ensure {root} exists and is writable by uid {euid} (the \
                 daemon's effective uid); check parent directory ownership \
                 and any read-only mount options.",
                root = shell_quote(inputs.workspace_root),
                euid = inputs.process_euid,
            )),
        );
    }
}
