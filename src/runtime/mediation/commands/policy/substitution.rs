use super::normalize::{
    normalize_shell_words, push_normalized_candidates, push_normalized_shell_words,
};

pub(super) struct ShellCommandAnalysis {
    pub(super) segments: Vec<String>,
    pub(super) normalized_segments: Vec<String>,
    pub(super) composed: bool,
    pub(super) command_word_constructed: bool,
}

pub(super) fn analyze_shell_command(command: &str) -> ShellCommandAnalysis {
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
