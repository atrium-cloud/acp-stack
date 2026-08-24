//! Secret-reference templates: `Bearer ${NAME}`-style values in config
//! positions, validated at declaration time and resolved against the
//! `SecretStore` by pure string composition.

use super::validate::primitives::validate_secret_ref_name_value;
use crate::error::{Result, StackError};
use crate::secrets::SecretStore;

// === CONSTANTS ===
const REF_OPEN: &str = "${";
const REF_CLOSE: char = '}';
const DOLLAR: char = '$';
const ENV_ASSIGN: char = '=';
const REASON_UNTERMINATED: &str = "unterminated `${`; close it with `}`";
const REASON_EMPTY_REF: &str = "`${}` is empty; name the secret ref";
const REASON_BARE_DOLLAR: &str =
    "unescaped `$`; write `$$` for a literal dollar or `${NAME}` for a ref";
const REASON_NO_REF: &str =
    "template contains no `${NAME}` reference; use a bare secret ref for a whole-value reference";
const REASON_EMPTY_ENV_VAR_NAME: &str = "env entry has an empty name left of `=`";
/// Rough allowance per resolved ref when pre-sizing the composed value.
const RESOLVED_REF_SIZE_HINT: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateSegment {
    Literal(String),
    Ref(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretTemplate {
    segments: Vec<TemplateSegment>,
}

impl SecretTemplate {
    /// Parse a template (`${NAME}` ref, `$$` literal `$`, bare `$` rejected).
    /// At least one ref is required: a pure literal in a secret position is
    /// almost always a pasted credential, which config must never carry.
    pub fn parse(field: &'static str, raw: &str) -> Result<Self> {
        let segments = parse_segments(raw)
            .map_err(|reason| StackError::SecretTemplateInvalid { field, reason })?;
        for segment in &segments {
            if let TemplateSegment::Ref(name) = segment {
                validate_secret_ref_name_value(name)?;
            }
        }
        if !segments
            .iter()
            .any(|segment| matches!(segment, TemplateSegment::Ref(_)))
        {
            return Err(StackError::SecretTemplateInvalid {
                field,
                reason: REASON_NO_REF,
            });
        }
        Ok(Self { segments })
    }

    /// Ref names in declaration order; a name referenced twice appears twice.
    pub fn ref_names(&self) -> impl Iterator<Item = &str> {
        self.segments.iter().filter_map(|segment| match segment {
            TemplateSegment::Ref(name) => Some(name.as_str()),
            TemplateSegment::Literal(_) => None,
        })
    }

    pub fn literals(&self) -> impl Iterator<Item = &str> {
        self.segments.iter().filter_map(|segment| match segment {
            TemplateSegment::Literal(text) => Some(text.as_str()),
            TemplateSegment::Ref(_) => None,
        })
    }

    pub fn resolve(&self, store: &SecretStore) -> Result<String> {
        let literal_size: usize = self.literals().map(str::len).sum();
        let ref_count = self.ref_names().count();
        let mut composed = String::with_capacity(literal_size + ref_count * RESOLVED_REF_SIZE_HINT);
        for segment in &self.segments {
            match segment {
                TemplateSegment::Literal(text) => composed.push_str(text),
                TemplateSegment::Ref(name) => composed.push_str(store.get(name)?),
            }
        }
        Ok(composed)
    }
}

fn parse_segments(raw: &str) -> std::result::Result<Vec<TemplateSegment>, &'static str> {
    let mut segments = Vec::new();
    let mut literal = String::new();
    let mut rest = raw;
    while let Some(dollar_index) = rest.find(DOLLAR) {
        literal.push_str(&rest[..dollar_index]);
        let after_dollar = &rest[dollar_index + DOLLAR.len_utf8()..];
        if let Some(after_escape) = after_dollar.strip_prefix(DOLLAR) {
            literal.push(DOLLAR);
            rest = after_escape;
        } else if let Some(after_open) = rest[dollar_index..].strip_prefix(REF_OPEN) {
            let Some(close_index) = after_open.find(REF_CLOSE) else {
                return Err(REASON_UNTERMINATED);
            };
            let name = &after_open[..close_index];
            if name.is_empty() {
                return Err(REASON_EMPTY_REF);
            }
            if !literal.is_empty() {
                segments.push(TemplateSegment::Literal(std::mem::take(&mut literal)));
            }
            segments.push(TemplateSegment::Ref(name.to_owned()));
            rest = &after_open[close_index + REF_CLOSE.len_utf8()..];
        } else {
            return Err(REASON_BARE_DOLLAR);
        }
    }
    literal.push_str(rest);
    if !literal.is_empty() {
        segments.push(TemplateSegment::Literal(literal));
    }
    Ok(segments)
}

/// Ref names inside a template, ignoring syntax errors, for report-only
/// callers that must never fail a status render.
pub fn ref_names_lossy(raw: &str) -> Vec<String> {
    match parse_segments(raw) {
        Ok(segments) => segments
            .into_iter()
            .filter_map(|segment| match segment {
                TemplateSegment::Ref(name) if super::is_valid_secret_ref_name(&name) => Some(name),
                _ => None,
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Every literal fragment and ref name in a template, unvalidated; on a syntax
/// error the whole raw string comes back as one piece so screening still sees
/// the full text. Must run BEFORE name validation: a screening rejection
/// redacts the value, a name-validation rejection echoes it.
pub fn template_pieces_lossy(raw: &str) -> Vec<String> {
    match parse_segments(raw) {
        Ok(segments) => segments
            .into_iter()
            .map(|segment| match segment {
                TemplateSegment::Literal(text) => text,
                TemplateSegment::Ref(name) => name,
            })
            .collect(),
        Err(_) => vec![raw.to_owned()],
    }
}

fn screen_piece(field: &'static str, piece: &str) -> Result<()> {
    if super::validate::primitives::secret_ref_looks_like_value(piece) {
        return Err(StackError::SecretRefLooksLikeValue { field });
    }
    Ok(())
}

/// Reject a ref name that looks like a pasted credential. Must run BEFORE any
/// name-shape validation: this rejection redacts the value, while
/// `InvalidSecretRefName` echoes it.
pub fn screen_ref_name(field: &'static str, name: &str) -> Result<()> {
    screen_piece(field, name)
}

/// Screen every literal fragment and ref name of a template. Same
/// screening-before-echo contract as [`screen_ref_name`].
pub fn screen_template(field: &'static str, raw: &str) -> Result<()> {
    let Ok(segments) = parse_segments(raw) else {
        // Screened whole so the heuristic still sees the full text.
        return screen_piece(field, raw);
    };
    let mut concatenated_literals = String::new();
    let mut literal_count = 0usize;
    for segment in &segments {
        match segment {
            TemplateSegment::Literal(text) => {
                concatenated_literals.push_str(text);
                literal_count += 1;
                screen_piece(field, text)?;
            }
            TemplateSegment::Ref(name) => screen_piece(field, name)?,
        }
    }
    // A credential straddling a `${}` boundary trips no single fragment, so
    // the concatenated literals reassemble its shape. The ref-name length
    // ceiling is skipped here: it guards names, not static text.
    if literal_count > 1 && super::validate::primitives::secret_value_shape(&concatenated_literals)
    {
        return Err(StackError::SecretRefLooksLikeValue { field });
    }
    Ok(())
}

/// Screen an env entry across both forms (`NAME` and `VAR=template`). Same
/// screening-before-echo contract as [`screen_ref_name`].
pub fn screen_env_entry(field: &'static str, raw: &str) -> Result<()> {
    match raw.split_once(ENV_ASSIGN) {
        None => screen_piece(field, raw),
        Some((var_name, template_raw)) => {
            screen_piece(field, var_name)?;
            screen_template(field, template_raw)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvEntry {
    /// Bare `NAME`: the ref name doubles as the env var name.
    WholeValueRef(String),
    /// `VAR=template`: var named on the left, value composed on the right.
    Templated {
        var_name: String,
        template: SecretTemplate,
    },
}

pub fn parse_env_entry(field: &'static str, raw: &str) -> Result<EnvEntry> {
    match raw.split_once(ENV_ASSIGN) {
        None => {
            validate_secret_ref_name_value(raw)?;
            Ok(EnvEntry::WholeValueRef(raw.to_owned()))
        }
        Some((var_name, template_raw)) => {
            if var_name.is_empty() {
                return Err(StackError::SecretTemplateInvalid {
                    field,
                    reason: REASON_EMPTY_ENV_VAR_NAME,
                });
            }
            validate_secret_ref_name_value(var_name)?;
            let template = SecretTemplate::parse(field, template_raw)?;
            Ok(EnvEntry::Templated {
                var_name: var_name.to_owned(),
                template,
            })
        }
    }
}

/// The env var name an entry produces: left of `=`, else the whole entry.
/// Infallible on purpose: membership checks run on unvalidated input.
pub fn env_entry_var_name(raw: &str) -> &str {
    match raw.split_once(ENV_ASSIGN) {
        Some((var_name, _)) => var_name,
        None => raw,
    }
}

/// Secret ref names an env entry depends on, ignoring syntax errors.
pub fn env_entry_ref_names_lossy(raw: &str) -> Vec<String> {
    match raw.split_once(ENV_ASSIGN) {
        Some((_, template_raw)) => ref_names_lossy(template_raw),
        None => {
            if super::is_valid_secret_ref_name(raw) {
                vec![raw.to_owned()]
            } else {
                Vec::new()
            }
        }
    }
}

pub fn resolve_env_entry(
    field: &'static str,
    raw: &str,
    store: &SecretStore,
) -> Result<(String, String)> {
    match parse_env_entry(field, raw)? {
        EnvEntry::WholeValueRef(name) => {
            let value = store.get(&name)?.to_owned();
            Ok((name, value))
        }
        EnvEntry::Templated { var_name, template } => {
            let value = template.resolve(store)?;
            Ok((var_name, value))
        }
    }
}

/// Whether an env list already declares `var_name`, across both entry forms.
pub fn agent_env_declares(env: &[String], var_name: &str) -> bool {
    env.iter()
        .any(|entry| env_entry_var_name(entry) == var_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const FIELD: &str = "test.field";

    fn store_with(pairs: &[(&str, &str)]) -> (TempDir, SecretStore) {
        let home = TempDir::new().expect("tempdir");
        let mut store = SecretStore::open_or_create(home.path()).expect("open store");
        store.set_many(pairs.iter().copied()).expect("set secrets");
        (home, store)
    }

    fn parse(raw: &str) -> SecretTemplate {
        SecretTemplate::parse(FIELD, raw).expect("template should parse")
    }

    fn parse_err(raw: &str) -> StackError {
        SecretTemplate::parse(FIELD, raw).expect_err("template should be rejected")
    }

    #[test]
    fn parses_multi_segment_template() {
        let template = parse("Bearer ${KEY} of ${REALM}!");
        assert_eq!(
            template.ref_names().collect::<Vec<_>>(),
            vec!["KEY", "REALM"]
        );
        assert_eq!(
            template.literals().collect::<Vec<_>>(),
            vec!["Bearer ", " of ", "!"]
        );
    }

    #[test]
    fn parses_leading_and_trailing_literals() {
        let template = parse("${A}-suffix");
        assert_eq!(template.literals().collect::<Vec<_>>(), vec!["-suffix"]);
        let template = parse("prefix-${A}");
        assert_eq!(template.literals().collect::<Vec<_>>(), vec!["prefix-"]);
    }

    #[test]
    fn parses_adjacent_refs() {
        let template = parse("${A}${B}");
        assert_eq!(template.ref_names().collect::<Vec<_>>(), vec!["A", "B"]);
        assert_eq!(template.literals().count(), 0);
    }

    #[test]
    fn repeated_ref_appears_twice() {
        let template = parse("${A}:${A}");
        assert_eq!(template.ref_names().collect::<Vec<_>>(), vec!["A", "A"]);
    }

    #[test]
    fn dollar_dollar_escapes_literal_dollar() {
        let template = parse("cost $$5 ${A}");
        assert_eq!(template.literals().collect::<Vec<_>>(), vec!["cost $5 "]);
    }

    #[test]
    fn escaped_open_brace_is_literal() {
        let template = parse("$${NOT_A_REF} ${A}");
        assert_eq!(
            template.literals().collect::<Vec<_>>(),
            vec!["${NOT_A_REF} "]
        );
        assert_eq!(template.ref_names().collect::<Vec<_>>(), vec!["A"]);
    }

    #[test]
    fn rejects_bare_dollar() {
        assert!(matches!(
            parse_err("Bearer $KEY"),
            StackError::SecretTemplateInvalid { field: FIELD, .. }
        ));
    }

    #[test]
    fn rejects_unterminated_ref() {
        assert!(matches!(
            parse_err("Bearer ${KEY"),
            StackError::SecretTemplateInvalid { .. }
        ));
    }

    #[test]
    fn rejects_empty_ref() {
        assert!(matches!(
            parse_err("Bearer ${}"),
            StackError::SecretTemplateInvalid { .. }
        ));
    }

    #[test]
    fn rejects_invalid_ref_name() {
        assert!(matches!(
            parse_err("Bearer ${9BAD-NAME}"),
            StackError::InvalidSecretRefName { .. }
        ));
    }

    #[test]
    fn rejects_pure_literal() {
        assert!(matches!(
            parse_err("Bearer abc123"),
            StackError::SecretTemplateInvalid { .. }
        ));
        assert!(matches!(
            parse_err(""),
            StackError::SecretTemplateInvalid { .. }
        ));
    }

    #[test]
    fn resolves_composed_value() {
        let (_home, store) = store_with(&[("KEY", "secret-1"), ("REALM", "prod")]);
        let template = parse("Bearer ${KEY}/${REALM}$$x");
        assert_eq!(
            template.resolve(&store).expect("resolve"),
            "Bearer secret-1/prod$x"
        );
    }

    #[test]
    fn resolve_missing_ref_is_secret_not_found() {
        let (_home, store) = store_with(&[]);
        let template = parse("Bearer ${MISSING}");
        assert!(matches!(
            template.resolve(&store),
            Err(StackError::SecretNotFound { .. })
        ));
    }

    #[test]
    fn env_entry_bare_is_whole_value_ref() {
        assert_eq!(
            parse_env_entry(FIELD, "API_KEY").expect("parse"),
            EnvEntry::WholeValueRef("API_KEY".to_owned())
        );
    }

    #[test]
    fn env_entry_with_template() {
        let entry =
            parse_env_entry(FIELD, "DATABASE_URL=postgres://u:${DB_PASS}@h/db").expect("parse");
        let EnvEntry::Templated { var_name, template } = entry else {
            panic!("expected templated entry");
        };
        assert_eq!(var_name, "DATABASE_URL");
        assert_eq!(template.ref_names().collect::<Vec<_>>(), vec!["DB_PASS"]);
    }

    #[test]
    fn env_entry_rejects_empty_var_name() {
        assert!(matches!(
            parse_env_entry(FIELD, "=${A}"),
            Err(StackError::SecretTemplateInvalid { .. })
        ));
    }

    #[test]
    fn env_entry_rejects_invalid_var_name() {
        assert!(matches!(
            parse_env_entry(FIELD, "BAD NAME=${A}"),
            Err(StackError::InvalidSecretRefName { .. })
        ));
    }

    #[test]
    fn env_entry_rejects_pure_literal_value() {
        assert!(matches!(
            parse_env_entry(FIELD, "VAR=plaintext"),
            Err(StackError::SecretTemplateInvalid { .. })
        ));
    }

    #[test]
    fn env_entry_rejects_invalid_bare_ref() {
        assert!(matches!(
            parse_env_entry(FIELD, "not a ref"),
            Err(StackError::InvalidSecretRefName { .. })
        ));
    }

    #[test]
    fn screens_credential_split_across_ref_boundary() {
        assert!(matches!(
            screen_template(FIELD, "sk${X}-ABCDEF0123456789"),
            Err(StackError::SecretRefLooksLikeValue { .. })
        ));
    }

    #[test]
    fn screens_jwt_split_across_ref_boundaries() {
        let template = concat!(
            "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9${A}",
            ".eyJzdWIiOiIxMjM0NTY3ODkwIiwiaWF0IjoxNTE2MjM5MDIyfQ${B}",
            ".SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJVadQssw5c"
        );
        assert!(matches!(
            screen_template(FIELD, template),
            Err(StackError::SecretRefLooksLikeValue { .. })
        ));
    }

    #[test]
    fn benign_split_literals_pass_screening() {
        screen_template(FIELD, "https://${HOST}.example/${PATH}?token=${TOK}").expect("benign");
        screen_template(FIELD, "Bearer ${A}").expect("single literal");
    }

    #[test]
    fn long_concatenated_literals_do_not_trip_the_length_rule() {
        // Each fragment is under the 128-char name ceiling but their sum is
        // over it.
        let template = format!("https://{}${{A}}{}", "x".repeat(70), "y".repeat(80));
        screen_template(FIELD, &template).expect("long static text is not a credential");
    }

    #[test]
    fn env_entry_var_name_both_forms() {
        assert_eq!(env_entry_var_name("API_KEY"), "API_KEY");
        assert_eq!(env_entry_var_name("VAR=x${A}y"), "VAR");
    }

    #[test]
    fn env_entry_ref_names_lossy_both_forms() {
        assert_eq!(env_entry_ref_names_lossy("API_KEY"), vec!["API_KEY"]);
        assert_eq!(env_entry_ref_names_lossy("URL=x${A}-${B}"), vec!["A", "B"]);
        assert!(env_entry_ref_names_lossy("???").is_empty());
        assert!(env_entry_ref_names_lossy("VAR=${broken").is_empty());
    }

    #[test]
    fn ref_names_lossy_on_garbage_is_empty() {
        assert!(ref_names_lossy("no refs here").is_empty());
        assert!(ref_names_lossy("${unclosed").is_empty());
        assert!(ref_names_lossy("$bare").is_empty());
    }

    #[test]
    fn resolve_env_entry_both_forms() {
        let (_home, store) = store_with(&[("API_KEY", "k1"), ("TOK", "t1")]);
        assert_eq!(
            resolve_env_entry(FIELD, "API_KEY", &store).expect("resolve"),
            ("API_KEY".to_owned(), "k1".to_owned())
        );
        assert_eq!(
            resolve_env_entry(FIELD, "AUTH=Bearer ${TOK}", &store).expect("resolve"),
            ("AUTH".to_owned(), "Bearer t1".to_owned())
        );
    }

    #[test]
    fn agent_env_declares_both_forms() {
        let env = vec!["API_KEY".to_owned(), "AUTH=Bearer ${TOK}".to_owned()];
        assert!(agent_env_declares(&env, "API_KEY"));
        assert!(agent_env_declares(&env, "AUTH"));
        assert!(!agent_env_declares(&env, "TOK"));
    }
}
