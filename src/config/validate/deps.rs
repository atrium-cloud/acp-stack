//! Dependencies validation: per-category dedup + Phase 4 [install] action
//! constraints on `dependencies.commands`.

use std::collections::HashSet;

use crate::config::schema::{DependenciesConfig, DependencyEntry};
use crate::error::{Result, StackError};

pub(crate) fn validate_dependencies(deps: &DependenciesConfig) -> Result<()> {
    fn check(category: &'static str, list: &[DependencyEntry]) -> Result<()> {
        let mut seen = HashSet::new();
        for entry in list {
            if entry.name.trim().is_empty() {
                return Err(StackError::DependencyMissingName { category });
            }
            if !seen.insert(entry.name.clone()) {
                return Err(StackError::DuplicateDependency {
                    category,
                    name: entry.name.clone(),
                });
            }
        }
        Ok(())
    }
    check("commands", &deps.commands)?;
    check("packages", &deps.packages)?;
    check("runtimes", &deps.runtimes)?;
    check("mcp", &deps.mcp)?;
    // The `install` block is only meaningful for command deps —
    // `acps deps apply` runs install actions exclusively against
    // `dependencies.commands`. Reject install metadata on the other
    // categories so the operator doesn't declare it expecting it to
    // do something and silently get nothing (the "narrow supported
    // actions" contract from Phase 4 spec L62/L67).
    for (category, list) in [
        ("packages", &deps.packages),
        ("runtimes", &deps.runtimes),
        ("mcp", &deps.mcp),
    ] {
        for entry in list.iter() {
            if entry.install.is_some() {
                return Err(StackError::InvalidParam {
                    field: "dependencies",
                    reason: format!(
                        "dependency `{name}` under `{category}` declares an [install] block, \
                         but install actions are only supported on `commands` (Phase 4 deps apply)",
                        name = entry.name,
                    ),
                });
            }
        }
    }
    for entry in &deps.commands {
        let Some(install) = entry.install.as_ref() else {
            continue;
        };
        // Catch operator typos at config-load. An empty shell snippet
        // would no-op the install; a blank `creates` would produce an
        // impossible postcheck; `timeout_secs = 0` would surface as
        // an instant timeout on every run.
        if install.shell.trim().is_empty() {
            return Err(StackError::InvalidParam {
                field: "dependencies",
                reason: format!(
                    "dependency `{name}` has [install] with empty `shell`",
                    name = entry.name,
                ),
            });
        }
        if let Some(creates) = install.creates.as_deref()
            && creates.trim().is_empty()
        {
            return Err(StackError::InvalidParam {
                field: "dependencies",
                reason: format!(
                    "dependency `{name}` has [install] with empty `creates`",
                    name = entry.name,
                ),
            });
        }
        if matches!(install.timeout_secs, Some(0)) {
            return Err(StackError::InvalidParam {
                field: "dependencies",
                reason: format!(
                    "dependency `{name}` has [install].timeout_secs = 0; \
                     omit the field to use the 10m default, or set a positive value",
                    name = entry.name,
                ),
            });
        }
        // Same cap as the agent registry: an unbounded value would panic the
        // deadline arithmetic in `run_captured` when the apply runs.
        if let Some(timeout_secs) = install.timeout_secs
            && timeout_secs > crate::runtime::process_runner::MAX_INSTALL_TIMEOUT_SECS
        {
            return Err(StackError::InvalidParam {
                field: "dependencies",
                reason: format!(
                    "dependency `{name}` has [install].timeout_secs = {timeout_secs}, \
                     which exceeds the {MAX_INSTALL_TIMEOUT_SECS}-second (24 hour) cap",
                    name = entry.name,
                    MAX_INSTALL_TIMEOUT_SECS =
                        crate::runtime::process_runner::MAX_INSTALL_TIMEOUT_SECS,
                ),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::DependencyInstallAction;
    use crate::runtime::process_runner::MAX_INSTALL_TIMEOUT_SECS;

    fn deps_with_timeout(timeout_secs: Option<u64>) -> DependenciesConfig {
        DependenciesConfig {
            commands: vec![DependencyEntry {
                name: "tool".to_owned(),
                required: true,
                feature: None,
                install: Some(DependencyInstallAction {
                    shell: "true".to_owned(),
                    creates: None,
                    scope: Default::default(),
                    timeout_secs,
                }),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn rejects_install_timeout_past_the_cap() {
        let result = validate_dependencies(&deps_with_timeout(Some(MAX_INSTALL_TIMEOUT_SECS + 1)));
        assert!(result.is_err());
    }

    #[test]
    fn accepts_install_timeout_at_the_cap() {
        validate_dependencies(&deps_with_timeout(Some(MAX_INSTALL_TIMEOUT_SECS)))
            .expect("at-cap timeout is valid");
    }
}
