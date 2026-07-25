//! Static policy evaluation for the command gateway.
//!
//! `evaluate_policy` matches submitted command lines and shell command
//! segments against `[permissions].deny`/`review` before any subprocess is
//! spawned. `resolve_cwd_under_workspace` refuses cwds that escape
//! `workspace.root` via symlink/`..`.

mod matching;
mod normalize;
mod substitution;

use crate::config::PermissionsConfig;

use self::matching::glob_match;
use self::normalize::normalize_shell_words;
use self::substitution::analyze_shell_command;

pub(crate) use self::matching::{ResolvedCommandCwd, resolve_cwd_under_workspace};

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
    let mut candidates =
        Vec::with_capacity(analysis.segments.len() + analysis.normalized_segments.len() + 2);
    candidates.push(command.trim());
    let normalized_command = normalize_shell_words(command);
    if normalized_command.text != command.trim() {
        candidates.push(normalized_command.text.as_str());
    }
    if let Some(command_text) = normalized_command.command_text.as_deref()
        && command_text != command.trim()
        && command_text != normalized_command.text
    {
        candidates.push(command_text);
    }
    candidates.extend(analysis.segments.iter().map(String::as_str));
    candidates.extend(analysis.normalized_segments.iter().map(String::as_str));

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
    if normalized_command.command_word_constructed || analysis.command_word_constructed {
        return PolicyDecision::ReviewRequired;
    }
    PolicyDecision::Allow
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
    fn evaluate_policy_matches_deny_on_single_quoted_command_word_construction() {
        let permissions = PermissionsConfig {
            mode: "auto".to_owned(),
            deny: vec!["rm *".to_owned()],
            ..PermissionsConfig::default()
        };
        assert_eq!(
            evaluate_policy("r''m -rf target", &permissions),
            PolicyDecision::Deny
        );
    }

    #[test]
    fn evaluate_policy_matches_deny_on_escaped_command_word_construction() {
        let permissions = PermissionsConfig {
            mode: "auto".to_owned(),
            deny: vec!["rm *".to_owned()],
            ..PermissionsConfig::default()
        };
        assert_eq!(
            evaluate_policy(r"r\m -rf target", &permissions),
            PolicyDecision::Deny
        );
    }

    #[test]
    fn evaluate_policy_matches_deny_on_escaped_newline_command_word_construction() {
        let permissions = PermissionsConfig {
            mode: "auto".to_owned(),
            deny: vec!["rm *".to_owned()],
            ..PermissionsConfig::default()
        };
        assert_eq!(
            evaluate_policy("r\\\nm -rf target", &permissions),
            PolicyDecision::Deny
        );
    }

    #[test]
    fn evaluate_policy_matches_deny_on_ansi_c_quoted_command_word_construction() {
        let permissions = PermissionsConfig {
            mode: "auto".to_owned(),
            deny: vec!["rm *".to_owned()],
            ..PermissionsConfig::default()
        };
        assert_eq!(
            evaluate_policy("$'r'm -rf target", &permissions),
            PolicyDecision::Deny
        );
    }

    #[test]
    fn evaluate_policy_matches_deny_on_ansi_c_octal_command_word_construction() {
        let permissions = PermissionsConfig {
            mode: "auto".to_owned(),
            deny: vec!["rm *".to_owned()],
            ..PermissionsConfig::default()
        };
        assert_eq!(
            evaluate_policy(r"$'\162'm -rf target", &permissions),
            PolicyDecision::Deny
        );
    }

    #[test]
    fn evaluate_policy_matches_deny_on_ansi_c_hex_command_word_construction() {
        let permissions = PermissionsConfig {
            mode: "auto".to_owned(),
            deny: vec!["rm *".to_owned()],
            ..PermissionsConfig::default()
        };
        assert_eq!(
            evaluate_policy(r"$'\x72'm -rf target", &permissions),
            PolicyDecision::Deny
        );
    }

    #[test]
    fn evaluate_policy_matches_deny_on_ansi_c_nul_command_word_construction() {
        let permissions = PermissionsConfig {
            mode: "auto".to_owned(),
            deny: vec!["rm *".to_owned()],
            ..PermissionsConfig::default()
        };
        assert_eq!(
            evaluate_policy(r"$'rm\0' -rf target", &permissions),
            PolicyDecision::Deny
        );
        assert_eq!(
            evaluate_policy(r"$'rm\x00' -rf target", &permissions),
            PolicyDecision::Deny
        );
        assert_eq!(
            evaluate_policy(r"$'rm\0suffix' -rf target", &permissions),
            PolicyDecision::Deny
        );
        assert_eq!(
            evaluate_policy(r"$'rm\x00suffix' -rf target", &permissions),
            PolicyDecision::Deny
        );
    }

    #[test]
    fn evaluate_policy_matches_deny_on_locale_quoted_command_word_construction() {
        let permissions = PermissionsConfig {
            mode: "auto".to_owned(),
            deny: vec!["rm *".to_owned()],
            ..PermissionsConfig::default()
        };
        assert_eq!(
            evaluate_policy("$\"r\"m -rf target", &permissions),
            PolicyDecision::Deny
        );
    }

    #[test]
    fn evaluate_policy_matches_deny_after_assignment_prefix() {
        let permissions = PermissionsConfig {
            mode: "auto".to_owned(),
            deny: vec!["rm *".to_owned()],
            ..PermissionsConfig::default()
        };
        assert_eq!(
            evaluate_policy("X=1 r''m -rf target", &permissions),
            PolicyDecision::Deny
        );
    }

    #[test]
    fn evaluate_policy_matches_deny_after_redirection_prefix() {
        let permissions = PermissionsConfig {
            mode: "auto".to_owned(),
            deny: vec!["rm *".to_owned()],
            ..PermissionsConfig::default()
        };
        assert_eq!(
            evaluate_policy(">/tmp/acp-stack-test.log r''m -rf target", &permissions),
            PolicyDecision::Deny
        );
    }

    #[test]
    fn evaluate_policy_matches_deny_after_separate_redirection_prefix() {
        let permissions = PermissionsConfig {
            mode: "auto".to_owned(),
            deny: vec!["rm *".to_owned()],
            ..PermissionsConfig::default()
        };
        assert_eq!(
            evaluate_policy("> /tmp/acp-stack-test.log r''m -rf target", &permissions),
            PolicyDecision::Deny
        );
    }

    #[test]
    fn evaluate_policy_matches_deny_after_assignment_with_quoted_value() {
        let permissions = PermissionsConfig {
            mode: "auto".to_owned(),
            deny: vec!["rm *".to_owned()],
            ..PermissionsConfig::default()
        };
        assert_eq!(
            evaluate_policy("X='1' r''m -rf target", &permissions),
            PolicyDecision::Deny
        );
    }

    #[test]
    fn evaluate_policy_matches_deny_after_pipeline_negation_prefix() {
        let permissions = PermissionsConfig {
            mode: "auto".to_owned(),
            deny: vec!["rm *".to_owned()],
            ..PermissionsConfig::default()
        };
        assert_eq!(
            evaluate_policy("! r''m -rf target", &permissions),
            PolicyDecision::Deny
        );
    }

    #[test]
    fn evaluate_policy_matches_deny_after_time_prefix() {
        let permissions = PermissionsConfig {
            mode: "auto".to_owned(),
            deny: vec!["rm *".to_owned()],
            ..PermissionsConfig::default()
        };
        assert_eq!(
            evaluate_policy("time r''m -rf target", &permissions),
            PolicyDecision::Deny
        );
        assert_eq!(
            evaluate_policy("time -p r''m -rf target", &permissions),
            PolicyDecision::Deny
        );
    }

    #[test]
    fn evaluate_policy_does_not_treat_escaped_assignment_operator_as_assignment_prefix() {
        let permissions = PermissionsConfig {
            mode: "auto".to_owned(),
            deny: vec!["rm *".to_owned()],
            ..PermissionsConfig::default()
        };
        assert_eq!(
            evaluate_policy(r"X\=1 rm -rf target", &permissions),
            PolicyDecision::ReviewRequired
        );
    }

    #[test]
    fn evaluate_policy_matches_constructed_command_word_in_later_segment() {
        let permissions = PermissionsConfig {
            mode: "auto".to_owned(),
            deny: vec!["rm *".to_owned()],
            ..PermissionsConfig::default()
        };
        assert_eq!(
            evaluate_policy("true && r''m -rf target", &permissions),
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
    fn evaluate_policy_matches_review_on_constructed_command_word() {
        let permissions = PermissionsConfig {
            mode: "auto".to_owned(),
            review: vec!["sudo *".to_owned()],
            ..PermissionsConfig::default()
        };
        assert_eq!(
            evaluate_policy("s''udo true", &permissions),
            PolicyDecision::Review
        );
    }

    #[test]
    fn evaluate_policy_matches_review_after_assignment_prefix() {
        let permissions = PermissionsConfig {
            mode: "auto".to_owned(),
            review: vec!["sudo *".to_owned()],
            ..PermissionsConfig::default()
        };
        assert_eq!(
            evaluate_policy("X=1 s''udo true", &permissions),
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
    fn evaluate_policy_does_not_match_denied_word_inside_quoted_argument() {
        let permissions = PermissionsConfig {
            mode: "auto".to_owned(),
            deny: vec!["rm *".to_owned()],
            ..PermissionsConfig::default()
        };
        assert_eq!(
            evaluate_policy(r#"echo "rm -rf target""#, &permissions),
            PolicyDecision::Allow
        );
    }

    #[test]
    fn evaluate_policy_does_not_match_denied_quoted_argument_after_assignment_prefix() {
        let permissions = PermissionsConfig {
            mode: "auto".to_owned(),
            deny: vec!["rm *".to_owned()],
            ..PermissionsConfig::default()
        };
        assert_eq!(
            evaluate_policy(r#"X=1 echo "rm -rf target""#, &permissions),
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
    fn evaluate_policy_requires_review_for_constructed_command_word() {
        let permissions = PermissionsConfig {
            mode: "auto".to_owned(),
            ..PermissionsConfig::default()
        };
        assert_eq!(
            evaluate_policy("e''cho ok", &permissions),
            PolicyDecision::ReviewRequired
        );
    }

    #[test]
    fn evaluate_policy_requires_review_for_constructed_command_after_assignment_prefix() {
        let permissions = PermissionsConfig {
            mode: "auto".to_owned(),
            ..PermissionsConfig::default()
        };
        assert_eq!(
            evaluate_policy("X=1 e''cho ok", &permissions),
            PolicyDecision::ReviewRequired
        );
    }

    #[test]
    fn evaluate_policy_requires_review_for_constructed_command_after_redirection_prefix() {
        let permissions = PermissionsConfig {
            mode: "auto".to_owned(),
            ..PermissionsConfig::default()
        };
        assert_eq!(
            evaluate_policy(">/tmp/acp-stack-test.log e''cho ok", &permissions),
            PolicyDecision::ReviewRequired
        );
    }

    #[test]
    fn evaluate_policy_requires_review_for_parameter_expanded_command_word() {
        let permissions = PermissionsConfig {
            mode: "auto".to_owned(),
            deny: vec!["rm *".to_owned()],
            ..PermissionsConfig::default()
        };
        assert_eq!(
            evaluate_policy(r"r${X} -rf target", &permissions),
            PolicyDecision::ReviewRequired
        );
    }

    #[test]
    fn evaluate_policy_requires_review_for_parameter_expanded_command_after_prefixes() {
        let permissions = PermissionsConfig {
            mode: "auto".to_owned(),
            deny: vec!["rm *".to_owned()],
            ..PermissionsConfig::default()
        };
        assert_eq!(
            evaluate_policy(
                r"X=1 >/tmp/acp-stack-test.log r${Y} -rf target",
                &permissions
            ),
            PolicyDecision::ReviewRequired
        );
    }

    #[test]
    fn evaluate_policy_requires_review_for_brace_expanded_command_word() {
        let permissions = PermissionsConfig {
            mode: "auto".to_owned(),
            deny: vec!["rm *".to_owned()],
            ..PermissionsConfig::default()
        };
        assert_eq!(
            evaluate_policy("r{m,} -rf target", &permissions),
            PolicyDecision::ReviewRequired
        );
    }

    #[test]
    fn evaluate_policy_requires_review_for_pathname_expanded_command_word() {
        let permissions = PermissionsConfig {
            mode: "auto".to_owned(),
            deny: vec!["rm *".to_owned()],
            ..PermissionsConfig::default()
        };
        assert_eq!(
            evaluate_policy("r? -rf target", &permissions),
            PolicyDecision::ReviewRequired
        );
    }

    #[test]
    fn evaluate_policy_does_not_treat_glob_argument_as_command_construction() {
        let permissions = PermissionsConfig {
            mode: "auto".to_owned(),
            ..PermissionsConfig::default()
        };
        assert_eq!(
            evaluate_policy("echo r?", &permissions),
            PolicyDecision::Allow
        );
    }

    #[test]
    fn evaluate_policy_allows_literal_test_bracket_command() {
        let permissions = PermissionsConfig {
            mode: "auto".to_owned(),
            ..PermissionsConfig::default()
        };
        assert_eq!(
            evaluate_policy("[ -f Cargo.toml ]", &permissions),
            PolicyDecision::Allow
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
