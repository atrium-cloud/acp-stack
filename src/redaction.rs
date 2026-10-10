//! Shared redaction for text that leaves the process: a registry of known secret values plus
//! credential-shape detection, applied to persisted, returned, and logged text.

use std::borrow::Cow;
use std::ops::Range;
use std::sync::{PoisonError, RwLock};

use serde_json::Value;

// CONSTANTS

/// Placeholder written over every redacted span.
pub const REDACTION_PLACEHOLDER: &str = "[redacted]";

/// Shortest value treated as a secret; low-entropy settings are left alone.
pub const MIN_REDACTED_SECRET_LEN: usize = 6;

/// Shortest leading fragment of a secret redacted at the head of a front-truncated text.
const MIN_REDACTED_SECRET_FRAGMENT_LEN: usize = 8;

/// Shortest body after a credential prefix; excludes words like `sk-learn`.
const CREDENTIAL_PREFIX_MIN_BODY_LEN: usize = 9;

/// Shortest base64url segment of a JWT-shaped token.
const JWT_MIN_SEGMENT_LEN: usize = 10;
const JWT_SEGMENT_COUNT: usize = 3;

/// Characters that end a token for credential-shape matching, besides whitespace.
const TOKEN_SEPARATORS: &[char] = &[
    '"', '\'', '`', '=', ':', ',', ';', '(', ')', '[', ']', '{', '}', '<', '>', '/', '\\', '&', '?',
];

/// Characters that end a credential value once a header name or auth scheme has claimed it.
/// Base64 and opaque tokens carry `/`, `=`, `+`, and `:`, so those stay inside the value.
const VALUE_TERMINATORS: &[char] = &['"', '\'', '`', ',', ';', '&', ')', ']', '}', '<', '>'];

/// Characters that end a `Header: value` credential; header values carry spaces and `;`.
const LINE_VALUE_TERMINATORS: &[char] = &['\n', '\r', '"', '\'', '`', ')', ']', '}', '<', '>'];

/// Separator characters allowed between a sensitive field name and its value; `\` admits
/// escaped JSON such as `{\"api_key\":\"...\"}`.
const ASSIGNMENT_GAP_CHARS: &[char] = &[' ', '\t', '"', '\'', '\\', ':', '='];
const ASSIGNMENT_OPERATORS: &[char] = &[':', '='];

/// HTTP auth schemes whose following token is the credential.
const AUTH_SCHEME_WORDS: &[&str] = &["bearer", "basic", "token"];
const BEARER_SCHEME: &str = "bearer";
const BEARER_SCHEME_PREFIX: &str = "bearer ";

/// Shortest word after an auth scheme treated as a credential, so prose like
/// "invalid bearer token" survives.
const MIN_SCHEME_CREDENTIAL_LEN: usize = 8;

/// HTTP header names (lowercase, `-` folded to `_`) whose value runs to the end of the line.
const SENSITIVE_HEADER_NAMES: &[&str] = &[
    "authorization",
    "proxy_authorization",
    "cookie",
    "set_cookie",
    "x_api_key",
    "x_auth_token",
];

/// Field names (lowercase, `-` folded to `_`) whose value is a credential.
const SENSITIVE_FIELD_NAMES: &[&str] = &[
    "api_key",
    "apikey",
    "auth_token",
    "access_token",
    "refresh_token",
    "id_token",
    "session_token",
    "password",
    "passwd",
    "secret",
    "client_secret",
    "secret_key",
    "private_key",
];
const SENSITIVE_FIELD_NAME_SUFFIXES: &[&str] = &[
    "_api_key",
    "_apikey",
    "_secret",
    "_password",
    "_passwd",
    "_access_token",
    "_auth_token",
    "_refresh_token",
    "_secret_key",
    "_private_key",
    "_secret_access_key",
];

/// A token prefix that marks a credential, with the input screens that use it. The redactor
/// matches every entry; each screen keeps the subset it has always enforced, so widening this
/// table never changes which configs or native imports are accepted.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CredentialPrefix {
    pub(crate) prefix: &'static str,
    /// Rejected by config validation in secret ref names and template literals.
    pub(crate) config_screened: bool,
    /// Classified as a credential by native config import in path segments and arguments.
    pub(crate) native_import_screened: bool,
}

const fn credential_prefix(
    prefix: &'static str,
    config_screened: bool,
    native_import_screened: bool,
) -> CredentialPrefix {
    CredentialPrefix {
        prefix,
        config_screened,
        native_import_screened,
    }
}

pub(crate) const CREDENTIAL_PREFIXES: [CredentialPrefix; 15] = [
    credential_prefix("acps_", true, false),
    credential_prefix("sk-", true, true),
    credential_prefix("pk-", false, true),
    credential_prefix("rk-", false, true),
    credential_prefix("ghp_", true, true),
    credential_prefix("gho_", false, true),
    credential_prefix("ghu_", false, true),
    credential_prefix("ghs_", false, true),
    credential_prefix("ghr_", false, true),
    credential_prefix("github_pat_", true, true),
    credential_prefix("glpat-", false, true),
    credential_prefix("xoxb-", true, true),
    credential_prefix("xoxp-", true, true),
    credential_prefix("xoxa-", true, true),
    credential_prefix("xoxs-", false, true),
];

/// Known secret values, longest first.
static KNOWN_SECRET_VALUES: RwLock<Vec<String>> = RwLock::new(Vec::new());

/// Register secret values for redaction. Values shorter than [`MIN_REDACTED_SECRET_LEN`] are
/// skipped; values stay registered for the life of the process.
pub fn register_secret_values<'a>(values: impl IntoIterator<Item = &'a str>) {
    let candidates: Vec<&str> = values
        .into_iter()
        .filter(|value| value.len() >= MIN_REDACTED_SECRET_LEN)
        .collect();
    if candidates.is_empty() {
        return;
    }
    let mut fresh: Vec<&str> = {
        let registry = KNOWN_SECRET_VALUES
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        candidates
            .into_iter()
            .filter(|candidate| !registry.iter().any(|known| known == candidate))
            .collect()
    };
    fresh.sort_unstable();
    fresh.dedup();
    if fresh.is_empty() {
        return;
    }
    let mut registry = KNOWN_SECRET_VALUES
        .write()
        .unwrap_or_else(PoisonError::into_inner);
    for value in fresh {
        if !registry.iter().any(|known| known == value) {
            registry.push(value.to_owned());
        }
    }
    registry.sort_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
}

/// Replace registered secret values and credential-shaped tokens with [`REDACTION_PLACEHOLDER`].
pub fn redact_text(text: &str) -> Cow<'_, str> {
    let mut spans = registered_value_spans(text);
    spans.extend(credential_shape_spans(text));
    if spans.is_empty() {
        return Cow::Borrowed(text);
    }
    Cow::Owned(replace_spans(text, spans))
}

/// Redact every string leaf of `value` in place; object keys are kept.
pub fn redact_json(value: &mut Value) {
    match value {
        Value::String(text) => {
            if let Cow::Owned(redacted) = redact_text(text) {
                *text = redacted;
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact_json),
        Value::Object(object) => object.values_mut().for_each(redact_json),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// Keep at most `max_bytes` of `text`, cut back to a char boundary, and append a
/// ` [truncated N bytes]` marker. The marker is outside the `max_bytes` budget.
pub fn bounded(text: &str, max_bytes: usize) -> Cow<'_, str> {
    if text.len() <= max_bytes {
        return Cow::Borrowed(text);
    }
    let mut cut = max_bytes;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let dropped = text.len() - cut;
    Cow::Owned(format!("{} [truncated {dropped} bytes]", &text[..cut]))
}

/// Redact an explicit list of values from `text`. A front-truncated text can begin with the
/// tail of a value that straddled the cut, so its head is also checked for a value suffix.
pub fn redact_values(text: &mut String, values: &[String], front_truncated: bool) {
    // Longest first: a shorter value nested in a longer one would otherwise mask the longer
    // value's match and leave its remainder visible.
    let mut ordered: Vec<&String> = values.iter().collect();
    ordered.sort_by_key(|value| std::cmp::Reverse(value.len()));
    for value in ordered {
        if value.len() < MIN_REDACTED_SECRET_LEN {
            continue;
        }
        if text.contains(value.as_str()) {
            *text = text.replace(value.as_str(), REDACTION_PLACEHOLDER);
        }
        if front_truncated {
            redact_leading_secret_fragment(text, value);
        }
    }
}

/// Redact the longest suffix of `value` that `text` begins with.
fn redact_leading_secret_fragment(text: &mut String, value: &str) {
    let max = value.len().min(text.len());
    for len in (MIN_REDACTED_SECRET_FRAGMENT_LEN..=max).rev() {
        let suffix_start = value.len() - len;
        if !value.is_char_boundary(suffix_start) {
            continue;
        }
        let suffix = &value[suffix_start..];
        if text.starts_with(suffix) {
            text.replace_range(..suffix.len(), REDACTION_PLACEHOLDER);
            return;
        }
    }
}

/// Whether `token` starts (ASCII case-insensitively) with `prefix` and carries a credential-sized body.
pub(crate) fn has_credential_prefix(token: &str, prefix: &str) -> bool {
    token
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
        && token.len() >= prefix.len() + CREDENTIAL_PREFIX_MIN_BODY_LEN
}

/// Three dot-separated base64url segments, each long enough to rule out version strings.
pub(crate) fn looks_like_jwt(text: &str) -> bool {
    let segments = text.split('.').collect::<Vec<_>>();
    segments.len() == JWT_SEGMENT_COUNT
        && segments.iter().all(|segment| {
            segment.len() >= JWT_MIN_SEGMENT_LEN && segment.chars().all(is_base64url_char)
        })
}

/// Whether `text` begins with a `Bearer ` auth scheme.
pub(crate) fn starts_with_bearer_scheme(text: &str) -> bool {
    text.trim_start()
        .get(..BEARER_SCHEME_PREFIX.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(BEARER_SCHEME_PREFIX))
}

fn is_base64url_char(value: char) -> bool {
    value.is_ascii_alphanumeric() || value == '_' || value == '-'
}

fn registered_value_spans(text: &str) -> Vec<Range<usize>> {
    let registry = KNOWN_SECRET_VALUES
        .read()
        .unwrap_or_else(PoisonError::into_inner);
    registry
        .iter()
        .flat_map(|value| {
            text.match_indices(value.as_str())
                .map(|(start, matched)| start..start + matched.len())
        })
        .collect()
}

fn credential_shape_spans(text: &str) -> Vec<Range<usize>> {
    let tokens = token_spans(text);
    let mut spans = Vec::new();
    for (index, token_span) in tokens.iter().enumerate() {
        let token = &text[token_span.clone()];
        if let Some(len) = credential_token_len(token) {
            spans.push(token_span.start..token_span.start + len);
            continue;
        }
        let Some(next) = tokens.get(index + 1) else {
            continue;
        };
        let gap = &text[token_span.end..next.start];
        if token.eq_ignore_ascii_case(BEARER_SCHEME) && is_inline_whitespace(gap) {
            let end = value_end(text, next.start, ValueExtent::Word);
            if end - next.start >= MIN_SCHEME_CREDENTIAL_LEN {
                spans.push(next.start..end);
            }
            continue;
        }
        let Some(name) = sensitive_field_name(token) else {
            continue;
        };
        let Some(opening_quote) = assignment_opening_quote(gap) else {
            continue;
        };
        let extent = match opening_quote {
            OpeningQuote::Quoted(closing) => ValueExtent::Quoted(closing),
            OpeningQuote::EmptyValue => continue,
            OpeningQuote::Unquoted if name == FieldName::Header && gap.contains(':') => {
                ValueExtent::Line
            }
            OpeningQuote::Unquoted => ValueExtent::Word,
        };
        let mut value_start = next.start;
        let mut minimum_len = 1;
        let value_token = &text[next.clone()];
        if AUTH_SCHEME_WORDS
            .iter()
            .any(|scheme| value_token.eq_ignore_ascii_case(scheme))
        {
            match tokens.get(index + 2) {
                Some(credential) if is_inline_whitespace(&text[next.end..credential.start]) => {
                    value_start = credential.start;
                    minimum_len = MIN_SCHEME_CREDENTIAL_LEN;
                }
                _ => continue,
            }
        }
        let end = value_end(text, value_start, extent);
        if end - value_start >= minimum_len {
            spans.push(value_start..end);
        }
    }
    spans
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FieldName {
    Header,
    Field,
}

enum OpeningQuote {
    Unquoted,
    Quoted(&'static str),
    EmptyValue,
}

#[derive(Clone, Copy)]
enum ValueExtent {
    /// Up to whitespace or a closing delimiter.
    Word,
    /// Up to the end of the line or a closing delimiter; header values carry spaces and `;`.
    Line,
    /// Up to the given closing quote.
    Quoted(&'static str),
}

/// The quote that opens the value after an assignment gap (`:` or `=` plus spacing and quotes),
/// or `None` when the gap is not an assignment.
fn assignment_opening_quote(gap: &str) -> Option<OpeningQuote> {
    if !gap.contains(ASSIGNMENT_OPERATORS)
        || !gap
            .chars()
            .all(|character| ASSIGNMENT_GAP_CHARS.contains(&character))
    {
        return None;
    }
    let operator = gap.rfind(ASSIGNMENT_OPERATORS)?;
    let after_operator = &gap[operator + 1..];
    let quotes: Vec<char> = after_operator
        .chars()
        .filter(|character| matches!(character, '"' | '\''))
        .collect();
    Some(match quotes.as_slice() {
        [] => OpeningQuote::Unquoted,
        ['\''] => OpeningQuote::Quoted("'"),
        ['"'] if after_operator.ends_with("\\\"") => OpeningQuote::Quoted("\\\""),
        ['"'] => OpeningQuote::Quoted("\""),
        _ => OpeningQuote::EmptyValue,
    })
}

/// The redacted length of a self-identifying credential token, if it is one.
fn credential_token_len(token: &str) -> Option<usize> {
    if CREDENTIAL_PREFIXES
        .iter()
        .any(|entry| has_credential_prefix(token, entry.prefix))
    {
        return Some(token.len());
    }
    let candidate = token.trim_end_matches('.');
    looks_like_jwt(candidate).then_some(candidate.len())
}

fn token_spans(text: &str) -> Vec<Range<usize>> {
    let mut spans = Vec::new();
    let mut start = None;
    for (index, character) in text.char_indices() {
        let separator = character.is_whitespace() || TOKEN_SEPARATORS.contains(&character);
        match (separator, start) {
            (true, Some(token_start)) => {
                spans.push(token_start..index);
                start = None;
            }
            (false, None) => start = Some(index),
            (true, None) | (false, Some(_)) => {}
        }
    }
    if let Some(token_start) = start {
        spans.push(token_start..text.len());
    }
    spans
}

fn value_end(text: &str, start: usize, extent: ValueExtent) -> usize {
    let rest = &text[start..];
    let offset = match extent {
        ValueExtent::Word => rest.find(|character: char| {
            character.is_whitespace() || VALUE_TERMINATORS.contains(&character)
        }),
        ValueExtent::Line => {
            let line_end = rest.find(LINE_VALUE_TERMINATORS).unwrap_or(rest.len());
            Some(rest[..line_end].trim_end().len())
        }
        ValueExtent::Quoted(closing) => rest.find(closing).or_else(|| rest.find(['\n', '\r'])),
    };
    offset.map_or(text.len(), |offset| start + offset)
}

fn is_inline_whitespace(gap: &str) -> bool {
    !gap.is_empty() && gap.chars().all(|character| matches!(character, ' ' | '\t'))
}

fn sensitive_field_name(token: &str) -> Option<FieldName> {
    let normalized = token
        .trim_start_matches('-')
        .to_ascii_lowercase()
        .replace('-', "_");
    if SENSITIVE_HEADER_NAMES.contains(&normalized.as_str()) {
        return Some(FieldName::Header);
    }
    let field = SENSITIVE_FIELD_NAMES.contains(&normalized.as_str())
        || SENSITIVE_FIELD_NAME_SUFFIXES
            .iter()
            .any(|suffix| normalized.ends_with(suffix));
    field.then_some(FieldName::Field)
}

/// Replace the union of `spans` with one placeholder per merged run, in a single pass over the
/// original text so a placeholder is never re-matched.
fn replace_spans(text: &str, mut spans: Vec<Range<usize>>) -> String {
    spans.sort_by_key(|span| span.start);
    let mut output = String::with_capacity(text.len());
    let mut cursor = 0;
    let mut pending: Option<Range<usize>> = None;
    for span in spans {
        match pending.as_mut() {
            Some(current) if span.start <= current.end => current.end = current.end.max(span.end),
            _ => {
                if let Some(done) = pending.replace(span) {
                    output.push_str(&text[cursor..done.start]);
                    output.push_str(REDACTION_PLACEHOLDER);
                    cursor = done.end;
                }
            }
        }
    }
    if let Some(done) = pending {
        output.push_str(&text[cursor..done.start]);
        output.push_str(REDACTION_PLACEHOLDER);
        cursor = done.end;
    }
    output.push_str(&text[cursor..]);
    output
}

#[cfg(test)]
mod tests;
