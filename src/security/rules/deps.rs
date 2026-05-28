//! Dependency posture self-check.

use serde_json::json;

use crate::security::{
    MAX_DEPENDENCY_FINDING_DETAILS, SecurityCheckInputs, findings::SecurityFinding,
};

pub(in crate::security) fn check_deps(
    inputs: &SecurityCheckInputs<'_>,
    findings: &mut Vec<SecurityFinding>,
) {
    if inputs.dependency_failures.is_empty() {
        return;
    }

    let shown: Vec<_> = inputs
        .dependency_failures
        .iter()
        .take(MAX_DEPENDENCY_FINDING_DETAILS)
        .map(|dependency| {
            json!({
                "name": dependency.name.as_str(),
                "kind": dependency.kind.as_str(),
                "feature": dependency.feature.as_deref(),
                "reason": dependency.reason.as_deref(),
            })
        })
        .collect();
    let total = inputs.dependency_failures.len();
    let suffix = if total == 1 { "y" } else { "ies" };
    findings.push(
        SecurityFinding::warning(
            "deps.required_unavailable",
            &format!("{total} required dependenc{suffix} unavailable"),
        )
        .with_details(json!({
            "total": total,
            "truncated": total > MAX_DEPENDENCY_FINDING_DETAILS,
            "dependencies": shown,
        }))
        .with_remediation(
            "Run `acps deps check` for the full dependency report. For command \
             dependencies with declared install actions, run `acps deps apply --yes`; \
             otherwise install or configure the missing dependency manually.",
        ),
    );
}
