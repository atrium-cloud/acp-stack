//! Per-path posture rules: ownership, mode/symlink, and uninspectable paths.

use crate::security::SecurityCheckInputs;
use crate::security::findings::{SecurityFinding, shell_quote};

pub(in crate::security) fn check_paths(
    inputs: &SecurityCheckInputs<'_>,
    findings: &mut Vec<SecurityFinding>,
) {
    for posture in inputs.path_postures {
        // `shell_quote` so metacharacters in an operator-controlled path cannot produce
        // an unsafe-to-paste remediation command.
        let path_quoted = shell_quote(&posture.path.display().to_string());
        if posture.uid != inputs.process_euid {
            findings.push(
                SecurityFinding::critical(
                    "runtime.path_ownership",
                    &format!(
                        "{label} at {path} is owned by uid {actual}, expected uid {expected}",
                        label = posture.kind.label(),
                        path = posture.path.display(),
                        actual = posture.uid,
                        expected = inputs.process_euid,
                    ),
                )
                .with_details(serde_json::json!({
                    "path": posture.path.display().to_string(),
                    "kind": posture.kind.label(),
                    "actual_uid": posture.uid,
                    "expected_uid": inputs.process_euid,
                }))
                // The hint names the daemon's uid, never `runtime_user_name`, and never
                // suggests relaunching under the owner: the path could be root-owned
                // and `acps serve` refuses root execution.
                .with_remediation(format!(
                    "Run `chown -h {uid} -- {path_quoted}` (as root); uid \
                     {uid} is the running daemon's effective uid. The `-h` \
                     flag keeps `chown` operating on the path itself if it \
                     is a symlink; the gid is left unchanged because the \
                     check validates owner uid only.",
                    uid = inputs.process_euid,
                )),
            );
        }
        if let Some(expected_mode) = posture.kind.expected_mode()
            && posture.mode != expected_mode
        {
            // Linux `chmod` follows symlinks with no `-h` equivalent, so the usual
            // remediation would mutate the wrong target; a symlink here means external
            // tampering and must be removed rather than chmod'd through.
            let remediation = if posture.is_symlink {
                format!(
                    "{label} at {path_quoted} is a symlink; \
                         `chmod` would follow it and mutate the wrong \
                         target. Remove the symlink and recreate the \
                         managed path as an owner-only \
                         file/directory.",
                    label = posture.kind.label(),
                )
            } else {
                format!(
                    "Run `chmod 0{expected_mode:o} -- {path_quoted}` to \
                         restore owner-only permissions."
                )
            };
            findings.push(
                SecurityFinding::critical(
                    "runtime.path_mode_loose",
                    &format!(
                        "{label} at {path} has mode 0o{actual:o}, expected 0o{expected:o}",
                        label = posture.kind.label(),
                        path = posture.path.display(),
                        actual = posture.mode,
                        expected = expected_mode,
                    ),
                )
                .with_details(serde_json::json!({
                    "path": posture.path.display().to_string(),
                    "kind": posture.kind.label(),
                    "actual_mode": format!("0o{:o}", posture.mode),
                    "expected_mode": format!("0o{:o}", expected_mode),
                    "is_symlink": posture.is_symlink,
                }))
                .with_remediation(remediation),
            );
        }
    }

    for issue in inputs.path_issues {
        let path = issue.path.display().to_string();
        let path_quoted = shell_quote(&path);
        findings.push(
            SecurityFinding::critical(
                "runtime.path_uninspectable",
                &format!(
                    "{label} at {path} could not be inspected: {error}",
                    label = issue.kind.label(),
                    error = issue.error,
                ),
            )
            .with_details(serde_json::json!({
                "path": issue.path.display().to_string(),
                "kind": issue.kind.label(),
                "error": issue.error,
            }))
            .with_remediation(format!(
                "Restore {label} at {path_quoted} so the daemon uid {uid} can stat it. \
                 If the file was deleted, restore it from backup or run `acps init` to \
                 recreate missing runtime-managed files, then repair owner-only \
                 permissions with the matching `chmod` hint from `acps security check`.",
                label = issue.kind.label(),
                uid = inputs.process_euid,
            )),
        );
    }
}
