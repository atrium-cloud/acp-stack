//! Static policy evaluation for the command gateway.
//!
//! `evaluate_policy` matches the submitted command line against the
//! `[permissions].deny`/`review` glob lists from config — the synchronous
//! gate before any subprocess is spawned. `resolve_cwd_under_workspace`
//! refuses cwds that escape `workspace.root` via symlink/`..`.

use std::path::Path;

use crate::config::PermissionsConfig;
use crate::error::{Result, StackError};

/// Outcome of evaluating a submitted command against `[permissions]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PolicyDecision {
    Allow,
    Review,
    Deny,
}

pub(super) fn evaluate_policy(command: &str, permissions: &PermissionsConfig) -> PolicyDecision {
    if permissions
        .deny
        .iter()
        .any(|pattern| glob_match(pattern, command))
    {
        return PolicyDecision::Deny;
    }
    if permissions
        .review
        .iter()
        .any(|pattern| glob_match(pattern, command))
    {
        return PolicyDecision::Review;
    }
    PolicyDecision::Allow
}

/// Minimal shell-style glob matcher. Supports `*` (greedy, any chars including
/// none) and `?` (exactly one char). Everything else matches literally. This
/// is sufficient for the `deny = ["rm *", "shutdown"]`-style patterns the
/// spec calls out; it is NOT a full POSIX-glob implementation.
fn glob_match(pattern: &str, input: &str) -> bool {
    let pattern_bytes = pattern.as_bytes();
    let input_bytes = input.as_bytes();
    glob_match_inner(pattern_bytes, input_bytes)
}

fn glob_match_inner(pattern: &[u8], input: &[u8]) -> bool {
    let mut p = 0;
    let mut i = 0;
    let mut star_p: Option<usize> = None;
    let mut star_i = 0;
    while i < input.len() {
        if p < pattern.len() && (pattern[p] == input[i] || pattern[p] == b'?') {
            p += 1;
            i += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star_p = Some(p);
            star_i = i;
            p += 1;
        } else if let Some(sp) = star_p {
            p = sp + 1;
            star_i += 1;
            i = star_i;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

pub(super) fn resolve_cwd_under_workspace(
    root: &Path,
    requested: &str,
) -> Result<std::path::PathBuf> {
    if requested.contains('\0') {
        return Err(StackError::CommandCwdOutsideWorkspace {
            requested: requested.to_owned(),
        });
    }
    let candidate = if Path::new(requested).is_absolute() {
        std::path::PathBuf::from(requested)
    } else {
        root.join(requested)
    };
    let canonical_root =
        root.canonicalize()
            .map_err(|_| StackError::CommandCwdOutsideWorkspace {
                requested: requested.to_owned(),
            })?;
    let canonical_candidate =
        candidate
            .canonicalize()
            .map_err(|_| StackError::CommandCwdOutsideWorkspace {
                requested: requested.to_owned(),
            })?;
    if !canonical_candidate.starts_with(&canonical_root) {
        return Err(StackError::CommandCwdOutsideWorkspace {
            requested: requested.to_owned(),
        });
    }
    Ok(canonical_candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_match_matches_literal_and_wildcards() {
        assert!(glob_match("rm *", "rm -rf foo"));
        assert!(glob_match("shutdown", "shutdown"));
        assert!(!glob_match("shutdown", "shutdown now"));
        assert!(glob_match("shutdown*", "shutdown now"));
        assert!(glob_match("ls", "ls"));
        assert!(!glob_match("ls", "lsof"));
        assert!(glob_match("git ?ush", "git push"));
        assert!(!glob_match("git ?ush", "git status"));
        assert!(glob_match("*", ""));
        assert!(glob_match("*", "anything goes"));
    }

    #[test]
    fn evaluate_policy_prefers_deny_over_review() {
        let permissions = PermissionsConfig {
            mode: "auto".to_owned(),
            review: vec!["rm *".to_owned()],
            deny: vec!["rm *".to_owned()],
            ..PermissionsConfig::default()
        };
        assert_eq!(
            evaluate_policy("rm -rf /", &permissions),
            PolicyDecision::Deny
        );
    }

    #[test]
    fn evaluate_policy_returns_allow_for_unmatched() {
        let permissions = PermissionsConfig {
            mode: "auto".to_owned(),
            review: vec!["sudo *".to_owned()],
            deny: vec!["shutdown".to_owned()],
            ..PermissionsConfig::default()
        };
        assert_eq!(
            evaluate_policy("ls -la", &permissions),
            PolicyDecision::Allow
        );
    }
}
