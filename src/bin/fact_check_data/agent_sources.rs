//! Provider tables read from agent source code, which is what the agents run; their docs drift.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;

use acp_stack::runtime::agent::provider_keys::{
    ProviderKeyMapping, hermes_base_url_env_for_native_provider_id,
};
use serde::Deserialize;

use super::{Http, Report, env_var_for_agent_provider, native_provider_id};

const HERMES_AGENT_ID: &str = "hermes";
const HERMES_REPO: &str = "NousResearch/hermes-agent";
const HERMES_AUTH_PATH: &str = "hermes_cli/auth.py";
const HERMES_PLUGINS_DIR: &str = "plugins/model-providers";
const HERMES_PLUGIN_FILE: &str = "__init__.py";
const HERMES_REGISTRY_ROWS_ANCHOR: &str = "\n_REGISTRY_ROWS";
const HERMES_REGISTRY_END_ANCHOR: &str = "\nPROVIDER_REGISTRY";
const HERMES_PROVIDER_CONFIG_CALL: &str = "ProviderConfig";
const HERMES_KEY_ENV_VARS_ARGUMENT: &str = "api_key_env_vars";
const HERMES_BASE_URL_ENV_ARGUMENT: &str = "base_url_env_var";
const HERMES_PLUGIN_NAME_ARGUMENT: &str = "name";
const HERMES_PLUGIN_ENV_VARS_ARGUMENT: &str = "env_vars";
// Hermes keeps OpenRouter out of its registry and resolves it on a dedicated rung, which reads
// the base-URL env itself.
const HERMES_OWN_RUNG_PROVIDER: &str = "openrouter";
const HERMES_RUNTIME_PROVIDER_PATH: &str = "hermes_cli/runtime_provider.py";
// Hermes splits a plugin's `env_vars` the same way: URL-suffixed names override the endpoint.
const BASE_URL_ENV_SUFFIX: &str = "_URL";
const GITHUB_CONTENT_DIR_KIND: &str = "dir";

#[derive(Debug, Deserialize)]
struct GithubContent {
    name: String,
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct HermesProvider {
    key_env_vars: Vec<String>,
    base_url_env_var: Option<String>,
}

/// Verifies every shipped Hermes provider against the registry Hermes resolves `model.provider`
/// through: the `_REGISTRY_ROWS` in `hermes_cli/auth.py`, plus the model-provider plugins it
/// mirrors in for ids the rows lack.
pub(super) fn check_hermes_sources(
    http: &Http,
    mapping: &ProviderKeyMapping,
    verified: &mut BTreeSet<String>,
    report: &mut Report,
) -> Result<(), Box<dyn Error>> {
    let mut providers =
        hermes_registry_providers(&http.github_raw_text(HERMES_REPO, HERMES_AUTH_PATH)?);
    if providers.is_empty() {
        report.fail("Hermes auth.py `_REGISTRY_ROWS` yielded no providers; its format changed");
        return Ok(());
    }
    let plugin_dirs: BTreeSet<String> = http
        .github_json::<Vec<GithubContent>>(&format!(
            "/repos/{HERMES_REPO}/contents/{HERMES_PLUGINS_DIR}"
        ))?
        .into_iter()
        .filter(|entry| entry.kind == GITHUB_CONTENT_DIR_KIND)
        .map(|entry| entry.name)
        .collect();
    if plugin_dirs.is_empty() {
        report.fail("Hermes `plugins/model-providers` listing is empty; its layout changed");
        return Ok(());
    }

    let mut shipped = BTreeSet::new();
    let mut managed_lane_candidates = Vec::new();
    for provider in mapping
        .providers()
        .iter()
        .filter(|provider| provider.agents.iter().any(|agent| agent == HERMES_AGENT_ID))
    {
        let Some(native_id) = native_provider_id(provider, HERMES_AGENT_ID) else {
            continue;
        };
        shipped.insert(native_id.to_owned());
        if !providers.contains_key(native_id) && plugin_dirs.contains(native_id) {
            let plugin_source = http.github_raw_text(
                HERMES_REPO,
                &format!("{HERMES_PLUGINS_DIR}/{native_id}/{HERMES_PLUGIN_FILE}"),
            )?;
            if let Some(plugin) = hermes_plugin_providers(&plugin_source).remove(native_id) {
                providers.insert(native_id.to_owned(), plugin);
            }
        }
        let Some(upstream) = providers.get(native_id) else {
            report.fail(format!(
                "Hermes has no provider `{native_id}` in its registry rows or model-provider plugins"
            ));
            continue;
        };
        let Some(env_var) = env_var_for_agent_provider(mapping, HERMES_AGENT_ID, native_id) else {
            continue;
        };
        if !upstream.key_env_vars.iter().any(|key| key == env_var) {
            report.fail(format!(
                "Hermes provider `{native_id}` reads {}, shipped data has `{env_var}`",
                upstream.key_env_vars.join(", ")
            ));
            continue;
        }
        let expected_base_url_env = hermes_base_url_env_for_native_provider_id(native_id);
        let upstream_base_url_env = if native_id == HERMES_OWN_RUNG_PROVIDER {
            let runtime = tokens(
                &http.github_raw_text(HERMES_REPO, HERMES_RUNTIME_PROVIDER_PATH)?,
                Syntax::Python,
            );
            expected_base_url_env
                .filter(|env_var| runtime.contains(&Token::Str((*env_var).to_owned())))
                .map(str::to_owned)
        } else {
            upstream.base_url_env_var.clone()
        };
        match (expected_base_url_env, upstream_base_url_env.as_deref()) {
            (Some(expected), Some(actual)) if expected == actual => {}
            (Some(expected), actual) => {
                report.fail(format!(
                    "Hermes provider `{native_id}` reads base URL env {}, acp-stack exports `{expected}`",
                    actual.map_or("(none)".to_owned(), |actual| format!("`{actual}`"))
                ));
                continue;
            }
            (None, Some(actual)) => {
                managed_lane_candidates.push(format!("{native_id} ({actual})"));
            }
            (None, None) => {}
        }
        verified.extend(provider.ids().iter().cloned());
        report.ok(format!(
            "Hermes registry verifies `{native_id}` reads `{env_var}`"
        ));
    }

    let unshipped: BTreeSet<&str> = providers
        .keys()
        .chain(plugin_dirs.iter())
        .map(String::as_str)
        .filter(|id| !shipped.contains(*id))
        .collect();
    if !unshipped.is_empty() {
        report.info(format!(
            "Hermes providers acp-stack does not ship: {}",
            unshipped.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    if !managed_lane_candidates.is_empty() {
        report.info(format!(
            "Hermes providers with a base-URL env that acp-stack overrides through the managed entry instead: {}",
            managed_lane_candidates.join(", ")
        ));
    }
    Ok(())
}

/// Reads `_REGISTRY_ROWS`: `_api_key_provider` argument tuples
/// `(id, name, base_url, (key env vars...), base_url_env_var?, auth_type?)` and `ProviderConfig(...)` rows.
fn hermes_registry_providers(source: &str) -> BTreeMap<String, HermesProvider> {
    let Some(start) = source.find(HERMES_REGISTRY_ROWS_ANCHOR) else {
        return BTreeMap::new();
    };
    let block = &source[start..];
    let block = block
        .find(HERMES_REGISTRY_END_ANCHOR)
        .map_or(block, |end| &block[..end]);
    let tokens = tokens(block, Syntax::Python);
    let Some(rows_start) = tokens
        .iter()
        .position(|token| *token == Token::Punct('='))
        .and_then(|assign| {
            tokens[assign..]
                .iter()
                .position(|token| *token == Token::Punct('('))
                .map(|open| assign + open + 1)
        })
    else {
        return BTreeMap::new();
    };

    let mut providers = BTreeMap::new();
    for row in group_items(&tokens[rows_start..]) {
        let parsed = match row {
            [Token::Punct('('), arguments @ .., Token::Punct(')')] => {
                hermes_tuple_row(&group_items(arguments))
            }
            [
                Token::Word(call),
                Token::Punct('('),
                arguments @ ..,
                Token::Punct(')'),
            ] if call == HERMES_PROVIDER_CONFIG_CALL => {
                hermes_provider_config_row(&group_items(arguments))
            }
            _ => None,
        };
        if let Some((id, provider)) = parsed {
            providers.insert(id, provider);
        }
    }
    providers
}

fn hermes_tuple_row(arguments: &[&[Token]]) -> Option<(String, HermesProvider)> {
    let id = single_string(arguments.first()?)?;
    let key_env_vars = arguments.get(3).map_or_else(Vec::new, |keys| strings(keys));
    let base_url_env_var = arguments
        .get(4)
        .and_then(|argument| single_string(argument))
        .filter(|env_var| !env_var.is_empty());
    Some((
        id,
        HermesProvider {
            key_env_vars,
            base_url_env_var,
        },
    ))
}

fn hermes_provider_config_row(arguments: &[&[Token]]) -> Option<(String, HermesProvider)> {
    let id = single_string(arguments.first()?)?;
    let keyword = |name: &str| {
        arguments.iter().find_map(|argument| match argument {
            [Token::Word(key), Token::Punct('='), value @ ..] if key == name => Some(value),
            _ => None,
        })
    };
    Some((
        id,
        HermesProvider {
            key_env_vars: keyword(HERMES_KEY_ENV_VARS_ARGUMENT).map_or_else(Vec::new, strings),
            base_url_env_var: keyword(HERMES_BASE_URL_ENV_ARGUMENT)
                .and_then(single_string)
                .filter(|env_var| !env_var.is_empty()),
        },
    ))
}

/// Reads each `name="..."` profile in a model-provider plugin with the `env_vars=(...)` that
/// follows it.
fn hermes_plugin_providers(source: &str) -> BTreeMap<String, HermesProvider> {
    let tokens = tokens(source, Syntax::Python);
    let mut providers: BTreeMap<String, HermesProvider> = BTreeMap::new();
    let mut current: Option<String> = None;
    for (index, window) in tokens.windows(3).enumerate() {
        let [Token::Word(key), Token::Punct('='), value] = window else {
            continue;
        };
        match (key.as_str(), value) {
            (HERMES_PLUGIN_NAME_ARGUMENT, Token::Str(name)) => {
                providers.entry(name.clone()).or_default();
                current = Some(name.clone());
            }
            (HERMES_PLUGIN_ENV_VARS_ARGUMENT, Token::Punct('(')) => {
                let Some(provider) = current.as_ref().and_then(|name| providers.get_mut(name))
                else {
                    continue;
                };
                let env_vars = group_items(&tokens[index + 3..])
                    .into_iter()
                    .flat_map(strings)
                    .collect::<Vec<_>>();
                let (base_url_env_vars, key_env_vars): (Vec<_>, Vec<_>) = env_vars
                    .into_iter()
                    .partition(|env_var| env_var.ends_with(BASE_URL_ENV_SUFFIX));
                provider.key_env_vars = key_env_vars;
                provider.base_url_env_var = base_url_env_vars.into_iter().next();
            }
            _ => {}
        }
    }
    providers
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Str(String),
    Word(String),
    Punct(char),
}

#[derive(Debug, Clone, Copy)]
enum Syntax {
    Python,
}

impl Syntax {
    fn line_comment(self) -> &'static str {
        match self {
            Self::Python => "#",
        }
    }

    fn is_quote(self, ch: char) -> bool {
        match self {
            Self::Python => ch == '"' || ch == '\'',
        }
    }

    fn has_triple_quotes(self) -> bool {
        matches!(self, Self::Python)
    }
}

/// Splits source into string literals, words and punctuation, dropping comments, so table
/// parsers see the same structure whether a row sits on one line or wraps.
fn tokens(source: &str, syntax: Syntax) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut rest = source;
    while let Some(ch) = rest.chars().next() {
        if ch.is_whitespace() {
            rest = &rest[ch.len_utf8()..];
        } else if rest.starts_with(syntax.line_comment()) {
            rest = rest.find('\n').map_or("", |end| &rest[end..]);
        } else if syntax.is_quote(ch) {
            let (value, remaining) = string_literal(rest, ch, syntax);
            tokens.push(Token::Str(value));
            rest = remaining;
        } else if ch.is_alphanumeric() || ch == '_' {
            let end = rest
                .find(|next: char| !(next.is_alphanumeric() || next == '_'))
                .unwrap_or(rest.len());
            tokens.push(Token::Word(rest[..end].to_owned()));
            rest = &rest[end..];
        } else {
            tokens.push(Token::Punct(ch));
            rest = &rest[ch.len_utf8()..];
        }
    }
    tokens
}

fn string_literal(source: &str, quote: char, syntax: Syntax) -> (String, &str) {
    let triple = quote.to_string().repeat(3);
    let delimiter = if syntax.has_triple_quotes() && source.starts_with(&triple) {
        triple
    } else {
        quote.to_string()
    };
    let body = &source[delimiter.len()..];
    let mut value = String::new();
    let mut chars = body.char_indices();
    while let Some((index, ch)) = chars.next() {
        if ch == '\\' {
            if let Some((_, escaped)) = chars.next() {
                value.push(escaped);
            }
        } else if body[index..].starts_with(&delimiter) {
            return (value, &body[index + delimiter.len()..]);
        } else {
            value.push(ch);
        }
    }
    (value, "")
}

/// Splits a bracketed group's contents at its top-level commas, stopping at the group's closer.
fn group_items(tokens: &[Token]) -> Vec<&[Token]> {
    let mut items = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (index, token) in tokens.iter().enumerate() {
        match token {
            Token::Punct('(' | '[' | '{') => depth += 1,
            Token::Punct(')' | ']' | '}') if depth == 0 => {
                items.push(&tokens[start..index]);
                return items.into_iter().filter(|item| !item.is_empty()).collect();
            }
            Token::Punct(')' | ']' | '}') => depth -= 1,
            Token::Punct(',') if depth == 0 => {
                items.push(&tokens[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    items.push(&tokens[start..]);
    items.into_iter().filter(|item| !item.is_empty()).collect()
}

fn strings(tokens: &[Token]) -> Vec<String> {
    tokens
        .iter()
        .filter_map(|token| match token {
            Token::Str(value) => Some(value.clone()),
            _ => None,
        })
        .collect()
}

fn single_string(tokens: &[Token]) -> Option<String> {
    match tokens {
        [Token::Str(value)] => Some(value.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(keys: &[&str], base_url_env_var: Option<&str>) -> HermesProvider {
        HermesProvider {
            key_env_vars: keys.iter().map(|key| (*key).to_owned()).collect(),
            base_url_env_var: base_url_env_var.map(str::to_owned),
        }
    }

    #[test]
    fn tokens_drop_comments_and_keep_quoted_punctuation() {
        let tokens = tokens(
            "(\"a (b)\", 'c#d') # trailing (\n\"\"\"doc \"quoted\" text\"\"\"",
            Syntax::Python,
        );

        assert_eq!(
            tokens,
            [
                Token::Punct('('),
                Token::Str("a (b)".to_owned()),
                Token::Punct(','),
                Token::Str("c#d".to_owned()),
                Token::Punct(')'),
                Token::Str("doc \"quoted\" text".to_owned()),
            ]
        );
    }

    #[test]
    fn hermes_registry_reads_tuple_and_provider_config_rows() {
        let source = r#"
def _api_key_provider(id, name, inference_base_url, api_key_env_vars, base_url_env_var=""):
    return ProviderConfig(id=id, name=name)


# A tuple row is ``_api_key_provider(id, name, ...)``.
_REGISTRY_ROWS: tuple[Any, ...] = (
    ProviderConfig(
        "nous", "Nous Portal", "oauth_device_code", portal_base_url=DEFAULT_NOUS_PORTAL_URL,
        scope=DEFAULT_NOUS_SCOPE),
    ("openai-api", "OpenAI API", "https://api.openai.com/v1", ("OPENAI_API_KEY",), "OPENAI_BASE_URL"),
    ProviderConfig(
        "copilot-acp", "GitHub Copilot ACP", "external_process",
        inference_base_url=DEFAULT_COPILOT_ACP_BASE_URL, base_url_env_var="COPILOT_ACP_BASE_URL"),
    ("gemini", "Google AI Studio", "https://generativelanguage.googleapis.com/v1beta",
     ("GOOGLE_API_KEY", "GEMINI_API_KEY"), "GEMINI_BASE_URL"),
    # Comment rows sit between entries.
    ("kimi-coding-cn", "Kimi / Moonshot (China)", "https://api.moonshot.cn/v1", ("KIMI_CN_API_KEY",)),
    ("actual", "Actual Computer", DEFAULT_ACTUAL_BASE_URL, ("ACTUAL_API_KEY",), "ACTUAL_BASE_URL"),
    ("bedrock", "AWS Bedrock", "https://bedrock-runtime.us-east-1.amazonaws.com", (), "BEDROCK_BASE_URL",
     "aws_sdk"),
    ("vertex", "Google Vertex AI", "", (), "", "vertex"))
PROVIDER_REGISTRY: dict[str, ProviderConfig] = {
    p.id: p for p in (r if isinstance(r, ProviderConfig) else _api_key_provider(*r) for r in _REGISTRY_ROWS)
}
"#;

        let providers = hermes_registry_providers(source);

        assert_eq!(
            providers.keys().map(String::as_str).collect::<Vec<_>>(),
            [
                "actual",
                "bedrock",
                "copilot-acp",
                "gemini",
                "kimi-coding-cn",
                "nous",
                "openai-api",
                "vertex"
            ]
        );
        assert_eq!(
            providers["openai-api"],
            provider(&["OPENAI_API_KEY"], Some("OPENAI_BASE_URL"))
        );
        assert_eq!(
            providers["gemini"],
            provider(
                &["GOOGLE_API_KEY", "GEMINI_API_KEY"],
                Some("GEMINI_BASE_URL")
            )
        );
        assert_eq!(
            providers["kimi-coding-cn"],
            provider(&["KIMI_CN_API_KEY"], None)
        );
        assert_eq!(
            providers["actual"],
            provider(&["ACTUAL_API_KEY"], Some("ACTUAL_BASE_URL"))
        );
        assert_eq!(
            providers["bedrock"],
            provider(&[], Some("BEDROCK_BASE_URL"))
        );
        assert_eq!(providers["vertex"], provider(&[], None));
        assert_eq!(providers["nous"], provider(&[], None));
        assert_eq!(
            providers["copilot-acp"],
            provider(&[], Some("COPILOT_ACP_BASE_URL"))
        );
    }

    #[test]
    fn hermes_registry_without_rows_anchor_is_empty() {
        assert!(hermes_registry_providers("PROVIDER_REGISTRY = {}\n").is_empty());
    }

    #[test]
    fn hermes_plugins_split_base_url_env_from_keys() {
        let source = r#"
"""Command Code provider profile."""
from providers import register_provider

commandcode = ProviderProfile(
    name="commandcode", aliases=("command-code",), display_name="Command Code",
    env_vars=("COMMANDCODE_API_KEY", "COMMANDCODE_BASE_URL"),
)
anthropic_surface = ProviderProfile(
    name="commandcode-anthropic",
    env_vars=("COMMANDCODE_API_KEY",),
)
register_provider(commandcode)
"#;

        let providers = hermes_plugin_providers(source);

        assert_eq!(
            providers["commandcode"],
            provider(&["COMMANDCODE_API_KEY"], Some("COMMANDCODE_BASE_URL"))
        );
        assert_eq!(
            providers["commandcode-anthropic"],
            provider(&["COMMANDCODE_API_KEY"], None)
        );
        assert_eq!(providers.len(), 2);
    }
}
