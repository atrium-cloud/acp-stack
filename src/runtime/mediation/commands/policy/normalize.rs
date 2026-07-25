use super::matching::{command_word_index, redirection_operator_end};

pub(super) struct NormalizedShellWords {
    pub(super) text: String,
    pub(super) command_text: Option<String>,
    pub(super) command_word_constructed: bool,
}

pub(super) struct NormalizedShellWord {
    pub(super) text: String,
    pub(super) constructed: bool,
    pub(super) assignment_operator_index: Option<usize>,
    pub(super) assignment_name_constructed: bool,
    pub(super) redirection_operator_end: Option<usize>,
}

pub(super) fn normalize_shell_words(input: &str) -> NormalizedShellWords {
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

pub(super) fn push_normalized_candidates(candidates: &mut Vec<String>, input: &str) {
    let normalized = normalize_shell_words(input);
    push_normalized_shell_words(candidates, input, normalized);
}

pub(super) fn push_normalized_shell_words(
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
