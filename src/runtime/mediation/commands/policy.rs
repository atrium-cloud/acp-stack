//! Static policy evaluation for the command gateway.
//!
//! `evaluate_policy` matches submitted command lines and shell command
//! segments against `[permissions].deny`/`review` before any subprocess is
//! spawned. `resolve_cwd_under_workspace` refuses cwds that escape
//! `workspace.root` via symlink/`..`.

use std::path::{Path, PathBuf};

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

struct ShellCommandAnalysis {
    segments: Vec<String>,
    normalized_segments: Vec<String>,
    composed: bool,
    command_word_constructed: bool,
}

fn analyze_shell_command(command: &str) -> ShellCommandAnalysis {
    let mut segments = Vec::new();
    let mut normalized_segments = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = command.chars().collect();
    let mut index = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let mut composed = false;
    let mut command_word_constructed = false;

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
            push_substitution_segments_with_normalized(
                &mut segments,
                &mut normalized_segments,
                &substitution,
            );
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
            push_substitution_segments_with_normalized(
                &mut segments,
                &mut normalized_segments,
                &substitution,
            );
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
            push_substitution_segments_with_normalized(
                &mut segments,
                &mut normalized_segments,
                &substitution,
            );
            current.extend(chars[index..end_index].iter());
            index = end_index;
            continue;
        }
        if !in_single && !in_double && is_shell_separator(ch) {
            composed = true;
            command_word_constructed |=
                push_segment(&mut segments, &mut normalized_segments, &mut current);
            if matches!(ch, '&' | '|') && chars.get(index + 1) == Some(&ch) {
                index += 1;
            }
            index += 1;
            continue;
        }
        current.push(ch);
        index += 1;
    }
    command_word_constructed |= push_segment(&mut segments, &mut normalized_segments, &mut current);
    ShellCommandAnalysis {
        segments,
        normalized_segments,
        composed,
        command_word_constructed,
    }
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

fn push_substitution_segments_with_normalized(
    segments: &mut Vec<String>,
    normalized_segments: &mut Vec<String>,
    substitution: &str,
) {
    let analysis = analyze_shell_command(substitution);
    if analysis.segments.is_empty() {
        let trimmed = substitution.trim();
        if !trimmed.is_empty() {
            segments.push(trimmed.to_owned());
            push_normalized_candidates(normalized_segments, trimmed);
        }
    } else {
        segments.extend(analysis.segments);
        normalized_segments.extend(analysis.normalized_segments);
    }
}

fn is_shell_separator(ch: char) -> bool {
    matches!(ch, ';' | '&' | '|' | '\n')
}

fn push_segment(
    segments: &mut Vec<String>,
    normalized_segments: &mut Vec<String>,
    current: &mut String,
) -> bool {
    let segment = current.trim();
    let mut command_word_constructed = false;
    if !segment.is_empty() {
        segments.push(segment.to_owned());
        let normalized = normalize_shell_words(segment);
        command_word_constructed = normalized.command_word_constructed;
        push_normalized_shell_words(normalized_segments, segment, normalized);
    }
    current.clear();
    command_word_constructed
}

struct NormalizedShellWords {
    text: String,
    command_text: Option<String>,
    command_word_constructed: bool,
}

struct NormalizedShellWord {
    text: String,
    constructed: bool,
    assignment_operator_index: Option<usize>,
    assignment_name_constructed: bool,
    redirection_operator_end: Option<usize>,
}

fn normalize_shell_words(input: &str) -> NormalizedShellWords {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut chars = input.trim().chars().peekable();
    let mut in_single = false;
    let mut in_double = false;
    let mut word_constructed = false;
    let mut assignment_operator_index = None;
    let mut assignment_name_constructed = false;
    let mut redirection_operator_prefix = false;

    while let Some(ch) = chars.next() {
        if ch == '\\' && !in_single {
            word_constructed = true;
            if assignment_operator_index.is_none() {
                assignment_name_constructed = true;
            }
            if let Some(next) = chars.next() {
                if next != '\n' {
                    current.push(next);
                }
            } else {
                current.push(ch);
            }
            continue;
        }
        if ch == '\'' && !in_double {
            word_constructed = true;
            if assignment_operator_index.is_none() {
                assignment_name_constructed = true;
            }
            in_single = !in_single;
            continue;
        }
        if ch == '"' && !in_single {
            word_constructed = true;
            if assignment_operator_index.is_none() {
                assignment_name_constructed = true;
            }
            in_double = !in_double;
            continue;
        }
        if ch == '$'
            && !in_single
            && !in_double
            && let Some(next) = chars.peek().copied()
            && matches!(next, '\'' | '"')
        {
            word_constructed = true;
            if assignment_operator_index.is_none() {
                assignment_name_constructed = true;
            }
            let quote = chars.next().expect("peeked quote");
            if quote == '\'' {
                current.push_str(&consume_ansi_c_quoted(&mut chars));
            } else if quote == '"' {
                in_double = !in_double;
            }
            continue;
        }
        if ch == '$' && !in_single {
            word_constructed = true;
            if assignment_operator_index.is_none() {
                assignment_name_constructed = true;
            }
        }
        if !in_single
            && !in_double
            && (matches!(ch, '*' | '?' | '{' | '}')
                || (matches!(ch, '[' | ']') && !current.is_empty()))
        {
            word_constructed = true;
            if assignment_operator_index.is_none() {
                assignment_name_constructed = true;
            }
        }
        if ch.is_whitespace() && !in_single && !in_double {
            push_normalized_word(
                &mut words,
                &mut current,
                &mut word_constructed,
                &mut assignment_operator_index,
                &mut assignment_name_constructed,
                &mut redirection_operator_prefix,
            );
            continue;
        }
        if !in_single
            && !in_double
            && matches!(ch, '<' | '>')
            && assignment_operator_index.is_none()
            && !assignment_name_constructed
            && (current.is_empty() || current.chars().all(|existing| existing.is_ascii_digit()))
        {
            redirection_operator_prefix = true;
        }
        if ch == '=' && !in_single && !in_double && assignment_operator_index.is_none() {
            assignment_operator_index = Some(current.len());
        }
        current.push(ch);
    }
    push_normalized_word(
        &mut words,
        &mut current,
        &mut word_constructed,
        &mut assignment_operator_index,
        &mut assignment_name_constructed,
        &mut redirection_operator_prefix,
    );

    let command_index = command_word_index(&words);
    let command_text = command_index
        .filter(|index| *index > 0)
        .map(|index| join_words(&words[index..]));
    let command_word_constructed = command_index
        .and_then(|index| words.get(index))
        .is_some_and(|word| word.constructed);

    NormalizedShellWords {
        text: join_words(&words),
        command_text,
        command_word_constructed,
    }
}

fn push_normalized_word(
    words: &mut Vec<NormalizedShellWord>,
    current: &mut String,
    word_constructed: &mut bool,
    assignment_operator_index: &mut Option<usize>,
    assignment_name_constructed: &mut bool,
    redirection_operator_prefix: &mut bool,
) {
    if !current.is_empty() || *word_constructed {
        let text = std::mem::take(current);
        let redirection_operator_end =
            redirection_operator_end(&text, *redirection_operator_prefix);
        words.push(NormalizedShellWord {
            text,
            constructed: *word_constructed,
            assignment_operator_index: *assignment_operator_index,
            assignment_name_constructed: *assignment_name_constructed,
            redirection_operator_end,
        });
    }
    *word_constructed = false;
    *assignment_operator_index = None;
    *assignment_name_constructed = false;
    *redirection_operator_prefix = false;
}

fn consume_ansi_c_quoted(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
    let mut output = String::new();
    while let Some(ch) = chars.next() {
        if ch == '\'' {
            break;
        }
        if ch == '\\' {
            let Some(decoded) = decode_ansi_c_escape(chars) else {
                drain_ansi_c_quoted(chars);
                break;
            };
            output.push_str(&decoded);
        } else if ch == '\0' {
            drain_ansi_c_quoted(chars);
            break;
        } else {
            output.push(ch);
        }
    }
    output
}

fn drain_ansi_c_quoted(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    for ch in chars.by_ref() {
        if ch == '\'' {
            break;
        }
    }
}

fn decode_ansi_c_escape(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<String> {
    let Some(ch) = chars.next() else {
        return Some("\\".to_owned());
    };
    let decoded = match ch {
        'a' => Some("\u{7}".to_owned()),
        'b' => Some("\u{8}".to_owned()),
        'e' | 'E' => Some("\u{1b}".to_owned()),
        'f' => Some("\u{c}".to_owned()),
        'n' => Some("\n".to_owned()),
        'r' => Some("\r".to_owned()),
        't' => Some("\t".to_owned()),
        'v' => Some("\u{b}".to_owned()),
        '\\' => Some("\\".to_owned()),
        '\'' => Some("'".to_owned()),
        '"' => Some("\"".to_owned()),
        '?' => Some("?".to_owned()),
        'x' => decode_hex_escape(chars, 2),
        'u' => decode_hex_escape(chars, 4),
        'U' => decode_hex_escape(chars, 8),
        '0'..='7' => decode_octal_escape(chars, ch),
        other => Some(other.to_string()),
    }?;
    Some(decoded)
}

fn decode_octal_escape(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    first: char,
) -> Option<String> {
    let mut digits = String::from(first);
    while digits.len() < 3 {
        let Some(next) = chars.peek().copied() else {
            break;
        };
        if !matches!(next, '0'..='7') {
            break;
        }
        digits.push(chars.next().expect("peeked octal digit"));
    }
    let Some(value) = u32::from_str_radix(&digits, 8)
        .ok()
        .and_then(char::from_u32)
    else {
        return Some(String::new());
    };
    if value == '\0' {
        return None;
    }
    Some(value.to_string())
}

fn decode_hex_escape(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    max: usize,
) -> Option<String> {
    let mut digits = String::new();
    while digits.len() < max {
        let Some(next) = chars.peek().copied() else {
            break;
        };
        if !next.is_ascii_hexdigit() {
            break;
        }
        digits.push(chars.next().expect("peeked hex digit"));
    }
    if digits.is_empty() {
        return Some(String::new());
    }
    let Some(value) = u32::from_str_radix(&digits, 16)
        .ok()
        .and_then(char::from_u32)
    else {
        return Some(String::new());
    };
    if value == '\0' {
        return None;
    }
    Some(value.to_string())
}

fn push_normalized_candidates(candidates: &mut Vec<String>, input: &str) {
    let normalized = normalize_shell_words(input);
    push_normalized_shell_words(candidates, input, normalized);
}

fn push_normalized_shell_words(
    candidates: &mut Vec<String>,
    input: &str,
    normalized: NormalizedShellWords,
) {
    if normalized.text != input {
        candidates.push(normalized.text.clone());
    }
    if let Some(command_text) = normalized.command_text
        && command_text != input
        && command_text != normalized.text
    {
        candidates.push(command_text);
    }
}

fn join_words(words: &[NormalizedShellWord]) -> String {
    words
        .iter()
        .map(|word| word.text.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

fn command_word_index(words: &[NormalizedShellWord]) -> Option<usize> {
    let mut index = 0;
    while index < words.len() {
        if is_shell_assignment_word(&words[index]) {
            index += 1;
            continue;
        }
        if let Some(width) = redirection_prefix_width(words, index) {
            index += width;
            continue;
        }
        if let Some(width) = shell_pipeline_prefix_width(words, index) {
            index += width;
            continue;
        }
        return Some(index);
    }
    None
}

fn is_shell_assignment_word(word: &NormalizedShellWord) -> bool {
    if word.assignment_name_constructed {
        return false;
    }
    let Some(operator_index) = word.assignment_operator_index else {
        return false;
    };
    let name = &word.text[..operator_index];
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn redirection_prefix_width(words: &[NormalizedShellWord], index: usize) -> Option<usize> {
    let word = words.get(index)?;
    let operator_end = word.redirection_operator_end?;
    if word.text.len() == operator_end && words.get(index + 1).is_some() {
        Some(2)
    } else {
        Some(1)
    }
}

fn shell_pipeline_prefix_width(words: &[NormalizedShellWord], index: usize) -> Option<usize> {
    let word = words.get(index)?;
    if word.text == "!" {
        return Some(1);
    }
    if word.text != "time" {
        return None;
    }
    let mut width = 1;
    while words
        .get(index + width)
        .is_some_and(|candidate| candidate.text == "-p")
    {
        width += 1;
    }
    Some(width)
}

fn redirection_operator_end(word: &str, operator_prefix: bool) -> Option<usize> {
    if !operator_prefix {
        return None;
    }
    let start = word
        .char_indices()
        .find_map(|(index, ch)| matches!(ch, '<' | '>').then_some(index))?;
    let operator = &word[start..];
    let operator_len = if operator.starts_with("<<-") || operator.starts_with("<<<") {
        3
    } else if operator.starts_with(">>")
        || operator.starts_with("<>")
        || operator.starts_with("<&")
        || operator.starts_with(">&")
        || operator.starts_with(">|")
        || operator.starts_with("<<")
    {
        2
    } else {
        1
    };
    Some(start + operator_len)
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

#[derive(Debug, Clone)]
pub(super) struct ResolvedCommandCwd {
    path: PathBuf,
    #[cfg(unix)]
    identity: FileIdentity,
}

impl ResolvedCommandCwd {
    #[cfg(not(unix))]
    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn display_path(&self) -> String {
        self.path.to_string_lossy().into_owned()
    }

    #[cfg(unix)]
    pub(super) fn open_verified(&self) -> std::io::Result<std::fs::File> {
        use std::os::unix::fs::OpenOptionsExt;

        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(&self.path)?;
        let identity = FileIdentity::from_metadata(&file.metadata()?);
        if identity != self.identity {
            return Err(std::io::Error::other(
                "command cwd changed after validation",
            ));
        }
        Ok(file)
    }
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    dev: u64,
    ino: u64,
}

#[cfg(unix)]
impl FileIdentity {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
        }
    }
}

pub(super) fn resolve_cwd_under_workspace(
    root: &Path,
    requested: &str,
) -> Result<ResolvedCommandCwd> {
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
    let metadata =
        canonical_candidate
            .metadata()
            .map_err(|_| StackError::CommandCwdOutsideWorkspace {
                requested: requested.to_owned(),
            })?;
    if !metadata.is_dir() {
        return Err(StackError::CommandCwdOutsideWorkspace {
            requested: requested.to_owned(),
        });
    }
    if !canonical_candidate.starts_with(&canonical_root) {
        return Err(StackError::CommandCwdOutsideWorkspace {
            requested: requested.to_owned(),
        });
    }
    Ok(ResolvedCommandCwd {
        path: canonical_candidate,
        #[cfg(unix)]
        identity: FileIdentity::from_metadata(&metadata),
    })
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
