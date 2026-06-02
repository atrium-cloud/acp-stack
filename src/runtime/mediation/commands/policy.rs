//! Static policy evaluation for the command gateway.
//!
//! `evaluate_policy` matches submitted command lines and shell command
//! segments against `[permissions].deny`/`review` before any subprocess is
//! spawned. `resolve_cwd_under_workspace` refuses cwds that escape
//! `workspace.root` via symlink/`..`.

use std::path::Path;

use crate::config::PermissionsConfig;
use crate::error::{Result, StackError};

/// Outcome of evaluating a submitted command against `[permissions]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PolicyDecision {
    Allow,
    Review,
    ReviewRequired,
    Deny,
}

pub(super) fn evaluate_policy(command: &str, permissions: &PermissionsConfig) -> PolicyDecision {
    let analysis = analyze_shell_command(command);
    let mut candidates = Vec::with_capacity(analysis.segments.len() + 1);
    candidates.push(command.trim());
    candidates.extend(analysis.segments.iter().map(String::as_str));

    if permissions.deny.iter().any(|pattern| {
        candidates
            .iter()
            .any(|candidate| glob_match(pattern, candidate))
    }) {
        return PolicyDecision::Deny;
    }
    if permissions.review.iter().any(|pattern| {
        candidates
            .iter()
            .any(|candidate| glob_match(pattern, candidate))
    }) {
        return PolicyDecision::Review;
    }
    if analysis.composed {
        return PolicyDecision::ReviewRequired;
    }
    PolicyDecision::Allow
}

struct ShellCommandAnalysis {
    segments: Vec<String>,
    composed: bool,
}

fn analyze_shell_command(command: &str) -> ShellCommandAnalysis {
    let mut segments = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = command.chars().collect();
    let mut index = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let mut composed = false;

    while index < chars.len() {
        let ch = chars[index];
        if escaped {
            current.push(ch);
            escaped = false;
            index += 1;
            continue;
        }
        if ch == '\\' && !in_single {
            current.push(ch);
            escaped = true;
            index += 1;
            continue;
        }
        if ch == '\'' && !in_double {
            in_single = !in_single;
            current.push(ch);
            index += 1;
            continue;
        }
        if ch == '"' && !in_single {
            in_double = !in_double;
            current.push(ch);
            index += 1;
            continue;
        }
        if !in_single
            && ch == '$'
            && chars.get(index + 1) == Some(&'(')
            && chars.get(index + 2) != Some(&'(')
            && let Some((substitution, end_index)) =
                parse_dollar_command_substitution(&chars, index + 2)
        {
            composed = true;
            push_substitution_segments(&mut segments, &substitution);
            current.extend(chars[index..end_index].iter());
            index = end_index;
            continue;
        }
        if !in_single
            && !in_double
            && matches!(ch, '<' | '>')
            && chars.get(index + 1) == Some(&'(')
            && let Some((substitution, end_index)) = parse_process_substitution(&chars, index + 2)
        {
            composed = true;
            push_substitution_segments(&mut segments, &substitution);
            current.extend(chars[index..end_index].iter());
            index = end_index;
            continue;
        }
        if !in_single
            && ch == '`'
            && let Some((substitution, end_index)) =
                parse_backtick_command_substitution(&chars, index + 1)
        {
            composed = true;
            push_substitution_segments(&mut segments, &substitution);
            current.extend(chars[index..end_index].iter());
            index = end_index;
            continue;
        }
        if !in_single && !in_double && is_shell_separator(ch) {
            composed = true;
            push_segment(&mut segments, &mut current);
            if matches!(ch, '&' | '|') && chars.get(index + 1) == Some(&ch) {
                index += 1;
            }
            index += 1;
            continue;
        }
        current.push(ch);
        index += 1;
    }
    push_segment(&mut segments, &mut current);
    ShellCommandAnalysis { segments, composed }
}

fn parse_dollar_command_substitution(chars: &[char], start: usize) -> Option<(String, usize)> {
    let mut content = String::new();
    let mut index = start;
    let mut depth = 1;
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;

    while index < chars.len() {
        let ch = chars[index];
        if escaped {
            content.push(ch);
            escaped = false;
            index += 1;
            continue;
        }
        if ch == '\\' && !in_single {
            content.push(ch);
            escaped = true;
            index += 1;
            continue;
        }
        if ch == '\'' && !in_double {
            in_single = !in_single;
            content.push(ch);
            index += 1;
            continue;
        }
        if ch == '"' && !in_single {
            in_double = !in_double;
            content.push(ch);
            index += 1;
            continue;
        }
        if !in_single && !in_double && ch == '$' && chars.get(index + 1) == Some(&'(') {
            depth += 1;
            content.push(ch);
            content.push('(');
            index += 2;
            continue;
        }
        if !in_single && !in_double && ch == ')' {
            depth -= 1;
            if depth == 0 {
                return Some((content, index + 1));
            }
            content.push(ch);
            index += 1;
            continue;
        }
        content.push(ch);
        index += 1;
    }
    None
}

fn parse_process_substitution(chars: &[char], start: usize) -> Option<(String, usize)> {
    parse_parenthesized_shell_content(chars, start)
}

fn parse_backtick_command_substitution(chars: &[char], start: usize) -> Option<(String, usize)> {
    let mut content = String::new();
    let mut index = start;
    let mut escaped = false;

    while index < chars.len() {
        let ch = chars[index];
        if escaped {
            content.push(ch);
            escaped = false;
            index += 1;
            continue;
        }
        if ch == '\\' {
            content.push(ch);
            escaped = true;
            index += 1;
            continue;
        }
        if ch == '`' {
            return Some((content, index + 1));
        }
        content.push(ch);
        index += 1;
    }
    None
}

fn parse_parenthesized_shell_content(chars: &[char], start: usize) -> Option<(String, usize)> {
    let mut content = String::new();
    let mut index = start;
    let mut depth = 1;
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;

    while index < chars.len() {
        let ch = chars[index];
        if escaped {
            content.push(ch);
            escaped = false;
            index += 1;
            continue;
        }
        if ch == '\\' && !in_single {
            content.push(ch);
            escaped = true;
            index += 1;
            continue;
        }
        if ch == '\'' && !in_double {
            in_single = !in_single;
            content.push(ch);
            index += 1;
            continue;
        }
        if ch == '"' && !in_single {
            in_double = !in_double;
            content.push(ch);
            index += 1;
            continue;
        }
        if !in_single && !in_double && ch == '(' {
            depth += 1;
            content.push(ch);
            index += 1;
            continue;
        }
        if !in_single && !in_double && ch == ')' {
            depth -= 1;
            if depth == 0 {
                return Some((content, index + 1));
            }
            content.push(ch);
            index += 1;
            continue;
        }
        content.push(ch);
        index += 1;
    }
    None
}

fn push_substitution_segments(segments: &mut Vec<String>, substitution: &str) {
    let analysis = analyze_shell_command(substitution);
    if analysis.segments.is_empty() {
        let trimmed = substitution.trim();
        if !trimmed.is_empty() {
            segments.push(trimmed.to_owned());
        }
    } else {
        segments.extend(analysis.segments);
    }
}

fn is_shell_separator(ch: char) -> bool {
    matches!(ch, ';' | '&' | '|' | '\n')
}

fn push_segment(segments: &mut Vec<String>, current: &mut String) {
    let segment = current.trim();
    if !segment.is_empty() {
        segments.push(segment.to_owned());
    }
    current.clear();
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
    fn evaluate_policy_matches_deny_on_shell_segment() {
        let permissions = PermissionsConfig {
            mode: "auto".to_owned(),
            deny: vec!["rm *".to_owned()],
            ..PermissionsConfig::default()
        };
        assert_eq!(
            evaluate_policy("true && rm -rf target", &permissions),
            PolicyDecision::Deny
        );
    }

    #[test]
    fn evaluate_policy_matches_deny_inside_dollar_command_substitution() {
        let permissions = PermissionsConfig {
            mode: "auto".to_owned(),
            deny: vec!["rm *".to_owned()],
            ..PermissionsConfig::default()
        };
        assert_eq!(
            evaluate_policy(r#"echo "$(rm -rf target)""#, &permissions),
            PolicyDecision::Deny
        );
    }

    #[test]
    fn evaluate_policy_matches_deny_inside_process_substitution() {
        let permissions = PermissionsConfig {
            mode: "auto".to_owned(),
            deny: vec!["rm *".to_owned()],
            ..PermissionsConfig::default()
        };
        assert_eq!(
            evaluate_policy("cat <(rm -rf target)", &permissions),
            PolicyDecision::Deny
        );
    }

    #[test]
    fn evaluate_policy_matches_review_on_shell_segment() {
        let permissions = PermissionsConfig {
            mode: "auto".to_owned(),
            review: vec!["sudo *".to_owned()],
            ..PermissionsConfig::default()
        };
        assert_eq!(
            evaluate_policy("echo ok; sudo apt update", &permissions),
            PolicyDecision::Review
        );
    }

    #[test]
    fn evaluate_policy_matches_review_inside_process_substitution() {
        let permissions = PermissionsConfig {
            mode: "auto".to_owned(),
            review: vec!["sudo *".to_owned()],
            ..PermissionsConfig::default()
        };
        assert_eq!(
            evaluate_policy("diff <(sudo cat /etc/shadow) /dev/null", &permissions),
            PolicyDecision::Review
        );
    }

    #[test]
    fn evaluate_policy_matches_review_inside_backtick_command_substitution() {
        let permissions = PermissionsConfig {
            mode: "auto".to_owned(),
            review: vec!["sudo *".to_owned()],
            ..PermissionsConfig::default()
        };
        assert_eq!(
            evaluate_policy("echo `sudo apt update`", &permissions),
            PolicyDecision::Review
        );
    }

    #[test]
    fn evaluate_policy_does_not_split_quoted_operators() {
        let permissions = PermissionsConfig {
            mode: "auto".to_owned(),
            ..PermissionsConfig::default()
        };
        assert_eq!(
            evaluate_policy(r#"echo "a && b""#, &permissions),
            PolicyDecision::Allow
        );
    }

    #[test]
    fn evaluate_policy_does_not_treat_quoted_process_substitution_as_composition() {
        let permissions = PermissionsConfig {
            mode: "auto".to_owned(),
            deny: vec!["rm *".to_owned()],
            ..PermissionsConfig::default()
        };
        assert_eq!(
            evaluate_policy("echo '<(rm -rf target)'", &permissions),
            PolicyDecision::Allow
        );
    }

    #[test]
    fn evaluate_policy_does_not_treat_double_quoted_process_substitution_as_composition() {
        let permissions = PermissionsConfig {
            mode: "auto".to_owned(),
            deny: vec!["rm *".to_owned()],
            ..PermissionsConfig::default()
        };
        assert_eq!(
            evaluate_policy(r#"echo "<(rm -rf target)""#, &permissions),
            PolicyDecision::Allow
        );
    }

    #[test]
    fn evaluate_policy_does_not_treat_single_quoted_substitution_as_composition() {
        let permissions = PermissionsConfig {
            mode: "auto".to_owned(),
            deny: vec!["rm *".to_owned()],
            ..PermissionsConfig::default()
        };
        assert_eq!(
            evaluate_policy(r#"echo '$(rm -rf target)'"#, &permissions),
            PolicyDecision::Allow
        );
    }

    #[test]
    fn evaluate_policy_requires_review_for_composition() {
        let permissions = PermissionsConfig {
            mode: "auto".to_owned(),
            ..PermissionsConfig::default()
        };
        assert_eq!(
            evaluate_policy("echo one && echo two", &permissions),
            PolicyDecision::ReviewRequired
        );
    }

    #[test]
    fn evaluate_policy_requires_review_for_process_substitution() {
        let permissions = PermissionsConfig {
            mode: "auto".to_owned(),
            ..PermissionsConfig::default()
        };
        assert_eq!(
            evaluate_policy("cat <(date)", &permissions),
            PolicyDecision::ReviewRequired
        );
    }

    #[test]
    fn evaluate_policy_requires_review_for_command_substitution() {
        let permissions = PermissionsConfig {
            mode: "auto".to_owned(),
            ..PermissionsConfig::default()
        };
        assert_eq!(
            evaluate_policy("echo $(date)", &permissions),
            PolicyDecision::ReviewRequired
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
