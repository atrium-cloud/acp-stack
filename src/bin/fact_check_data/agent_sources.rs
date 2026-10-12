//! Provider tables read from agent source code, which is what the agents run; their docs drift.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;

use acp_stack::runtime::agent::provider_keys::{
    CODEX_AGENT_ID, CODEX_OPENAI_PROVIDER_ID, GOOSE_AGENT_ID, HERMES_AGENT_ID, KILO_AGENT_ID,
    KIMI_MODEL_ENV_VARS, KIMI_PROVIDER_TYPES, ProviderEnvMapping, ProviderKeyMapping,
    goose_host_env_for_native_provider_id, hermes_base_url_env_for_native_provider_id,
};
use serde::Deserialize;

use super::{Http, Report, env_var_for_agent_provider, native_provider_id};

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

const GOOSE_REPO: &str = "aaif-goose/goose";
const GOOSE_DEFINITIONS_DIR: &str = "crates/goose-providers/src/declarative/definitions";
const GOOSE_DEFINITION_EXTENSION: &str = ".json";
/// The Rust-implemented goose providers acp-stack ships, with the files holding each one's
/// `*_PROVIDER_NAME` constant and `ConfigKey` list. Every other goose provider is a declarative
/// definition.
const GOOSE_RUST_PROVIDERS: [(&str, &[&str]); 4] = [
    (
        "anthropic",
        &[
            "crates/goose-providers/src/anthropic.rs",
            "crates/goose-provider-types/src/formats/anthropic.rs",
        ],
    ),
    ("openai", &["crates/goose-providers/src/openai.rs"]),
    ("openrouter", &["crates/goose-providers/src/openrouter.rs"]),
    ("xai", &["crates/goose/src/providers/xai.rs"]),
];
const GOOSE_PROVIDER_NAME_SUFFIX: &str = "_PROVIDER_NAME";
const GOOSE_CONFIG_KEY_TYPE: &str = "ConfigKey";
const RUST_CONSTRUCTOR: &str = "new";

const CODEX_REPO: &str = "openai/codex";
const CODEX_PROVIDER_INFO_PATH: &str = "codex-rs/model-provider-info/src/lib.rs";
const CODEX_BUILT_IN_PROVIDERS_FN: &str = "built_in_model_providers";
const CODEX_PROVIDER_ID_SUFFIX: &str = "_PROVIDER_ID";
const CODEX_ACP_REPO: &str = "agentclientprotocol/codex-acp";
const CODEX_ACP_AUTH_METHOD_PATH: &str = "src/CodexAuthMethod.ts";
const CODEX_ACP_CLIENT_PATH: &str = "src/CodexAcpClient.ts";
const CODEX_ACP_KEY_READER_FN: &str = "readApiKeyFromEnv";

const KIMI_REPO: &str = "MoonshotAI/kimi-code";
/// The agent-core-v2 files that read Kimi's `KIMI_MODEL_*` launch env (its ACP server runs on
/// agent-core-v2).
const KIMI_MODEL_ENV_PATHS: [&str; 2] = [
    "packages/agent-core-v2/src/app/kosongConfig/configSection.ts",
    "packages/agent-core-v2/src/app/kosongConfig/envOverlay.ts",
];
const KIMI_PROVIDER_DEFINITION_PATH: &str =
    "packages/agent-core-v2/src/llm-adapter/provider/provider-definition.ts";

const KILO_REPO: &str = "Kilo-Org/kilocode";
const KILO_MODELS_PATH: &str = "packages/opencode/src/provider/models.ts";
const KILO_ENV_FIELD: &str = "env";

const TYPESCRIPT_ID_FIELD: &str = "id";
const CONST_KEYWORD: &str = "const";
/// Words that sit right before a function's name in a Rust or TypeScript definition.
const DEFINITION_KEYWORDS: [&str; 7] = [
    "fn",
    "function",
    "async",
    "static",
    "private",
    "protected",
    "public",
];
const GITHUB_CONTENT_DIR_KIND: &str = "dir";
const GITHUB_CONTENT_FILE_KIND: &str = "file";

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

/// A goose provider's declared names and the config keys it reads; goose resolves each key from
/// the same-named env var first.
#[derive(Debug, Default, PartialEq, Eq)]
struct GooseProvider {
    names: BTreeSet<String>,
    config_keys: BTreeSet<String>,
}

/// A goose declarative definition; `name` is the id `GOOSE_PROVIDER` selects.
#[derive(Debug, Deserialize)]
struct GooseDefinition {
    name: String,
    api_key_env: Option<String>,
}

impl From<GooseDefinition> for GooseProvider {
    fn from(definition: GooseDefinition) -> Self {
        Self {
            names: BTreeSet::from([definition.name]),
            config_keys: definition
                .api_key_env
                .into_iter()
                .filter(|env_var| !env_var.is_empty())
                .collect(),
        }
    }
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
    let plugin_dirs = github_directory_entries(http, HERMES_REPO, HERMES_PLUGINS_DIR)?
        .into_iter()
        .filter(|entry| entry.kind == GITHUB_CONTENT_DIR_KIND)
        .map(|entry| entry.name)
        .collect::<BTreeSet<_>>();
    if plugin_dirs.is_empty() {
        report.fail("Hermes `plugins/model-providers` listing is empty; its layout changed");
        return Ok(());
    }

    for native_id in agent_native_ids(mapping, HERMES_AGENT_ID) {
        if !providers.contains_key(native_id) && plugin_dirs.contains(native_id) {
            let plugin_source = http.github_raw_text(
                HERMES_REPO,
                &format!("{HERMES_PLUGINS_DIR}/{native_id}/{HERMES_PLUGIN_FILE}"),
            )?;
            if let Some(plugin) = hermes_plugin_providers(&plugin_source).remove(native_id) {
                providers.insert(native_id.to_owned(), plugin);
            }
        }
    }
    let own_rung_literals = if agent_native_ids(mapping, HERMES_AGENT_ID)
        .any(|native_id| native_id == HERMES_OWN_RUNG_PROVIDER)
    {
        strings(&tokens(
            &http.github_raw_text(HERMES_REPO, HERMES_RUNTIME_PROVIDER_PATH)?,
            Syntax::Python,
        ))
        .into_iter()
        .collect()
    } else {
        BTreeSet::new()
    };
    let upstream = HermesUpstream {
        providers,
        plugin_dirs,
        own_rung_literals,
    };
    evaluate_hermes(mapping, &upstream, verified, report);
    Ok(())
}

/// What the Hermes check compares against: registry rows plus the plugin profiles mirrored in for
/// shipped ids the rows lack, every plugin directory, and the string literals of the OpenRouter
/// rung.
struct HermesUpstream {
    providers: BTreeMap<String, HermesProvider>,
    plugin_dirs: BTreeSet<String>,
    own_rung_literals: BTreeSet<String>,
}

fn evaluate_hermes(
    mapping: &ProviderKeyMapping,
    upstream: &HermesUpstream,
    verified: &mut BTreeSet<String>,
    report: &mut Report,
) {
    let mut shipped = BTreeSet::new();
    let mut managed_lane_candidates = Vec::new();
    for provider in agent_providers(mapping, HERMES_AGENT_ID) {
        let Some(native_id) = native_provider_id(provider, HERMES_AGENT_ID) else {
            continue;
        };
        shipped.insert(native_id.to_owned());
        let Some(upstream_provider) = upstream.providers.get(native_id) else {
            report.fail(format!(
                "Hermes has no provider `{native_id}` in its registry rows or model-provider plugins"
            ));
            continue;
        };
        let Some(env_var) = env_var_for_agent_provider(mapping, HERMES_AGENT_ID, native_id) else {
            continue;
        };
        if !upstream_provider
            .key_env_vars
            .iter()
            .any(|key| key == env_var)
        {
            report.fail(format!(
                "Hermes provider `{native_id}` reads {}, shipped data has `{env_var}`",
                upstream_provider.key_env_vars.join(", ")
            ));
            continue;
        }
        let expected_base_url_env = hermes_base_url_env_for_native_provider_id(native_id);
        let upstream_base_url_env = if native_id == HERMES_OWN_RUNG_PROVIDER {
            expected_base_url_env
                .filter(|env_var| upstream.own_rung_literals.contains(*env_var))
                .map(str::to_owned)
        } else {
            upstream_provider.base_url_env_var.clone()
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

    let unshipped: BTreeSet<&str> = upstream
        .providers
        .keys()
        .chain(upstream.plugin_dirs.iter())
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
}

/// Verifies every shipped Goose provider against the source `GOOSE_PROVIDER` resolves through:
/// the declarative JSON definitions, or the Rust provider for the ids in `GOOSE_RUST_PROVIDERS`.
/// Only definitions for shipped ids are fetched, which keeps the run under GitHub's rate limit.
pub(super) fn check_goose_sources(
    http: &Http,
    mapping: &ProviderKeyMapping,
    verified: &mut BTreeSet<String>,
    report: &mut Report,
) -> Result<(), Box<dyn Error>> {
    let definitions = github_directory_entries(http, GOOSE_REPO, GOOSE_DEFINITIONS_DIR)?
        .into_iter()
        .filter(|entry| entry.kind == GITHUB_CONTENT_FILE_KIND)
        .filter_map(|entry| {
            entry
                .name
                .strip_suffix(GOOSE_DEFINITION_EXTENSION)
                .map(str::to_owned)
        })
        .collect::<BTreeSet<_>>();
    if definitions.is_empty() {
        report.fail("Goose declarative definitions listing is empty; its layout changed");
        return Ok(());
    }

    let mut providers = BTreeMap::new();
    for native_id in agent_native_ids(mapping, GOOSE_AGENT_ID) {
        let provider = match GOOSE_RUST_PROVIDERS.iter().find(|(id, _)| *id == native_id) {
            Some((_, paths)) => {
                let sources = paths
                    .iter()
                    .map(|path| http.github_raw_text(GOOSE_REPO, path))
                    .collect::<Result<Vec<_>, _>>()?;
                goose_rust_provider(&sources)
            }
            None => {
                let stem = goose_definition_stem(native_id);
                if !definitions.contains(&stem) {
                    continue;
                }
                let path = format!("{GOOSE_DEFINITIONS_DIR}/{stem}{GOOSE_DEFINITION_EXTENSION}");
                let definition: GooseDefinition =
                    serde_json::from_str(&http.github_raw_text(GOOSE_REPO, &path)?)
                        .map_err(|source| format!("failed to parse Goose {path}: {source}"))?;
                definition.into()
            }
        };
        providers.insert(native_id.to_owned(), provider);
    }
    evaluate_goose(mapping, &definitions, &providers, verified, report);
    Ok(())
}

/// A declarative definition's file stem for a goose provider id.
fn goose_definition_stem(native_id: &str) -> String {
    native_id.replace('-', "_")
}

/// Compares shipped Goose rows against the upstream providers keyed by goose id; `definitions`
/// holds every declarative definition stem, for the providers acp-stack does not ship.
fn evaluate_goose(
    mapping: &ProviderKeyMapping,
    definitions: &BTreeSet<String>,
    providers: &BTreeMap<String, GooseProvider>,
    verified: &mut BTreeSet<String>,
    report: &mut Report,
) {
    let mut shipped_definitions = BTreeSet::new();
    for provider in agent_providers(mapping, GOOSE_AGENT_ID) {
        let Some(native_id) = native_provider_id(provider, GOOSE_AGENT_ID) else {
            continue;
        };
        let stem = goose_definition_stem(native_id);
        let Some(upstream) = providers.get(native_id) else {
            report.fail(format!(
                "Goose has no provider `{native_id}`: no declarative definition `{stem}{GOOSE_DEFINITION_EXTENSION}` and no GOOSE_RUST_PROVIDERS entry"
            ));
            continue;
        };
        shipped_definitions.insert(stem);
        let Some(env_var) = env_var_for_agent_provider(mapping, GOOSE_AGENT_ID, native_id) else {
            continue;
        };
        if !upstream.names.contains(native_id) {
            report.fail(format!(
                "Goose source for `{native_id}` declares provider name {}",
                list_or_none(&upstream.names)
            ));
            continue;
        }
        if !upstream.config_keys.contains(env_var) {
            report.fail(format!(
                "Goose provider `{native_id}` reads {}, shipped data has `{env_var}`",
                list_or_none(&upstream.config_keys)
            ));
            continue;
        }
        if let Some(host_env) = goose_host_env_for_native_provider_id(native_id)
            && !upstream.config_keys.contains(host_env)
        {
            report.fail(format!(
                "Goose provider `{native_id}` has no `{host_env}` setting, which acp-stack writes for endpoint overrides"
            ));
            continue;
        }
        verified.extend(provider.ids().iter().cloned());
        report.ok(format!(
            "Goose source verifies `{native_id}` reads `{env_var}`"
        ));
    }

    let unshipped: Vec<&str> = definitions
        .iter()
        .filter(|stem| !shipped_definitions.contains(*stem))
        .map(String::as_str)
        .collect();
    if !unshipped.is_empty() {
        report.info(format!(
            "Goose declarative definitions acp-stack does not ship: {}",
            unshipped.join(", ")
        ));
    }
}

/// Codex builds in a handful of providers and takes every other one from `[model_providers]`
/// tables, where a built-in id wins over a configured one. The source pins acp-stack's two
/// assumptions: `openai` is a built-in whose key codex-acp reads from the env, and no other
/// shipped id collides with a built-in.
pub(super) fn check_codex_sources(
    http: &Http,
    mapping: &ProviderKeyMapping,
    verified: &mut BTreeSet<String>,
    report: &mut Report,
) -> Result<(), Box<dyn Error>> {
    let built_ins =
        codex_built_in_provider_ids(&http.github_raw_text(CODEX_REPO, CODEX_PROVIDER_INFO_PATH)?);
    let key_env_vars = codex_acp_key_env_vars(
        &http.github_raw_text(CODEX_ACP_REPO, CODEX_ACP_AUTH_METHOD_PATH)?,
        &http.github_raw_text(CODEX_ACP_REPO, CODEX_ACP_CLIENT_PATH)?,
    );
    evaluate_codex(mapping, &built_ins, &key_env_vars, verified, report);
    Ok(())
}

fn evaluate_codex(
    mapping: &ProviderKeyMapping,
    built_ins: &BTreeSet<String>,
    key_env_vars: &[String],
    verified: &mut BTreeSet<String>,
    report: &mut Report,
) {
    if built_ins.is_empty() {
        report.fail("Codex `built_in_model_providers` yielded no providers; its format changed");
        return;
    }
    if key_env_vars.is_empty() {
        report.fail("codex-acp `readApiKeyFromEnv` yielded no env vars; its format changed");
        return;
    }

    let mut shipped = BTreeSet::new();
    for provider in agent_providers(mapping, CODEX_AGENT_ID) {
        let Some(native_id) = native_provider_id(provider, CODEX_AGENT_ID) else {
            continue;
        };
        shipped.insert(native_id);
        if native_id != CODEX_OPENAI_PROVIDER_ID {
            if built_ins.contains(native_id) {
                report.fail(format!(
                    "Codex builds in provider `{native_id}`, which wins over the `[model_providers.{native_id}]` table acp-stack writes"
                ));
            } else {
                report.ok(format!(
                    "Codex has no built-in `{native_id}`, so the `[model_providers.{native_id}]` table acp-stack writes applies"
                ));
            }
            continue;
        }
        if !built_ins.contains(native_id) {
            report.fail(format!(
                "Codex no longer builds in provider `{native_id}`, which acp-stack selects without a `[model_providers]` table"
            ));
            continue;
        }
        let Some(env_var) = env_var_for_agent_provider(mapping, CODEX_AGENT_ID, native_id) else {
            continue;
        };
        if !key_env_vars.iter().any(|key| key == env_var) {
            report.fail(format!(
                "codex-acp reads the API key from {}, shipped data has `{env_var}`",
                key_env_vars.join(", ")
            ));
            continue;
        }
        verified.extend(provider.ids().iter().cloned());
        report.ok(format!(
            "Codex builds in `{native_id}` and codex-acp reads `{env_var}`"
        ));
    }

    let unshipped: Vec<&str> = built_ins
        .iter()
        .map(String::as_str)
        .filter(|id| !shipped.contains(id))
        .collect();
    if !unshipped.is_empty() {
        report.info(format!(
            "Codex built-in providers acp-stack does not ship: {}",
            unshipped.join(", ")
        ));
    }
}

/// Kimi Code runs any vendor through the launch env acp-stack builds, so its source pins that
/// contract rather than per-row vendor data: the `KIMI_MODEL_*` names it reads and the provider
/// types it registers.
pub(super) fn check_kimi_sources(http: &Http, report: &mut Report) -> Result<(), Box<dyn Error>> {
    let mut literals = BTreeSet::new();
    for path in KIMI_MODEL_ENV_PATHS {
        literals.extend(strings(&tokens(
            &http.github_raw_text(KIMI_REPO, path)?,
            Syntax::TypeScript,
        )));
    }
    let registered =
        kimi_provider_types(&http.github_raw_text(KIMI_REPO, KIMI_PROVIDER_DEFINITION_PATH)?);
    evaluate_kimi(&literals, &registered, report);
    Ok(())
}

/// `literals` are the string literals of the files that read the launch env; `registered` the
/// provider types Kimi registers.
fn evaluate_kimi(literals: &BTreeSet<String>, registered: &BTreeSet<String>, report: &mut Report) {
    let unread: Vec<&str> = KIMI_MODEL_ENV_VARS
        .into_iter()
        .filter(|env_var| !literals.contains(*env_var))
        .collect();
    if unread.is_empty() {
        report.ok(format!(
            "Kimi agent-core-v2 reads every launch env var acp-stack exports: {}",
            KIMI_MODEL_ENV_VARS.join(", ")
        ));
    } else {
        report.fail(format!(
            "Kimi agent-core-v2 no longer reads {}, which acp-stack exports",
            unread.join(", ")
        ));
    }

    if registered.is_empty() {
        report.fail("Kimi provider-definition.ts yielded no provider types; its format changed");
        return;
    }
    let unregistered: Vec<&str> = KIMI_PROVIDER_TYPES
        .into_iter()
        .filter(|provider_type| !registered.contains(*provider_type))
        .collect();
    if unregistered.is_empty() {
        report.ok(format!(
            "Kimi registers every provider type acp-stack selects: {}",
            KIMI_PROVIDER_TYPES.join(", ")
        ));
    } else {
        report.fail(format!(
            "Kimi registers no provider type {}, which acp-stack's Kimi rows select",
            unregistered.join(", ")
        ));
    }
    let unused: Vec<&str> = registered
        .iter()
        .map(String::as_str)
        .filter(|provider_type| !KIMI_PROVIDER_TYPES.contains(provider_type))
        .collect();
    if !unused.is_empty() {
        report.info(format!(
            "Kimi provider types acp-stack does not use: {}",
            unused.join(", ")
        ));
    }
}

/// Kilo's provider set is models.dev plus the gateway overlay in its `models.ts`, which replaces
/// the models.dev `kilo` row; each shipped Kilo row must name a key env one of them declares.
pub(super) fn check_kilo_sources(
    http: &Http,
    mapping: &ProviderKeyMapping,
    models_dev_env_vars: &BTreeMap<String, Vec<String>>,
    verified: &mut BTreeSet<String>,
    report: &mut Report,
) -> Result<(), Box<dyn Error>> {
    let overlay = kilo_overlay_env_vars(&http.github_raw_text(KILO_REPO, KILO_MODELS_PATH)?);
    evaluate_kilo(mapping, &overlay, models_dev_env_vars, verified, report);
    Ok(())
}

fn evaluate_kilo(
    mapping: &ProviderKeyMapping,
    overlay: &BTreeMap<String, Vec<String>>,
    models_dev_env_vars: &BTreeMap<String, Vec<String>>,
    verified: &mut BTreeSet<String>,
    report: &mut Report,
) {
    if overlay.is_empty() {
        report.fail("Kilo models.ts yielded no provider overlay; its format changed");
        return;
    }
    for provider in agent_providers(mapping, KILO_AGENT_ID) {
        let Some(native_id) = native_provider_id(provider, KILO_AGENT_ID) else {
            continue;
        };
        let Some(env_var) = env_var_for_agent_provider(mapping, KILO_AGENT_ID, native_id) else {
            continue;
        };
        let (source, upstream) = match (overlay.get(native_id), models_dev_env_vars.get(native_id))
        {
            (Some(env_vars), _) => ("models.ts overlay", env_vars),
            (None, Some(env_vars)) => ("models.dev", env_vars),
            (None, None) => {
                report.fail(format!(
                    "Kilo has no provider `{native_id}` in its models.ts overlay or models.dev"
                ));
                continue;
            }
        };
        if !upstream.iter().any(|key| key == env_var) {
            report.fail(format!(
                "Kilo {source} maps `{native_id}` to {}, shipped data has `{env_var}`",
                list_or_none(upstream)
            ));
            continue;
        }
        verified.extend(provider.ids().iter().cloned());
        report.ok(format!(
            "Kilo {source} verifies `{native_id}` reads `{env_var}`"
        ));
    }
}

fn agent_providers<'a>(
    mapping: &'a ProviderKeyMapping,
    agent_id: &'a str,
) -> impl Iterator<Item = &'a ProviderEnvMapping> {
    mapping
        .providers()
        .iter()
        .filter(move |provider| provider.agents.iter().any(|agent| agent == agent_id))
}

fn agent_native_ids<'a>(
    mapping: &'a ProviderKeyMapping,
    agent_id: &'a str,
) -> impl Iterator<Item = &'a str> {
    agent_providers(mapping, agent_id)
        .filter_map(move |provider| native_provider_id(provider, agent_id))
}

fn github_directory_entries(
    http: &Http,
    repo: &str,
    path: &str,
) -> Result<Vec<GithubContent>, Box<dyn Error>> {
    http.github_json(&format!("/repos/{repo}/contents/{path}"))
}

fn list_or_none<'a>(values: impl IntoIterator<Item = &'a String>) -> String {
    let values: Vec<&str> = values.into_iter().map(String::as_str).collect();
    if values.is_empty() {
        "(none)".to_owned()
    } else {
        values.join(", ")
    }
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
    profile_env_lists(
        &tokens(source, Syntax::Python),
        '=',
        HERMES_PLUGIN_NAME_ARGUMENT,
        HERMES_PLUGIN_ENV_VARS_ARGUMENT,
    )
    .into_iter()
    .map(|(name, env_vars)| {
        let (base_url_env_vars, key_env_vars): (Vec<_>, Vec<_>) = env_vars
            .into_iter()
            .partition(|env_var| env_var.ends_with(BASE_URL_ENV_SUFFIX));
        let provider = HermesProvider {
            key_env_vars,
            base_url_env_var: base_url_env_vars.into_iter().next(),
        };
        (name, provider)
    })
    .collect()
}

/// The provider names and config keys across a Rust goose provider's files.
fn goose_rust_provider(sources: &[String]) -> GooseProvider {
    let mut provider = GooseProvider::default();
    for source in sources {
        let tokens = tokens(source, Syntax::Rust);
        provider.names.extend(
            string_constants(&tokens)
                .into_iter()
                .filter(|(name, _)| name.ends_with(GOOSE_PROVIDER_NAME_SUFFIX))
                .map(|(_, value)| value),
        );
        provider
            .config_keys
            .extend(tokens.windows(6).filter_map(|window| match window {
                [
                    Token::Word(config_type),
                    Token::Punct(':'),
                    Token::Punct(':'),
                    Token::Word(constructor),
                    Token::Punct('('),
                    Token::Str(key),
                ] if config_type == GOOSE_CONFIG_KEY_TYPE && constructor == RUST_CONSTRUCTOR => {
                    Some(key.clone())
                }
                _ => None,
            }));
    }
    provider
}

/// Ids `built_in_model_providers` returns, resolved from the `*_PROVIDER_ID` constants it names.
fn codex_built_in_provider_ids(source: &str) -> BTreeSet<String> {
    let tokens = tokens(source, Syntax::Rust);
    let constants = string_constants(&tokens);
    function_body(&tokens, CODEX_BUILT_IN_PROVIDERS_FN)
        .into_iter()
        .flatten()
        .filter_map(|token| match token {
            Token::Word(name) if name.ends_with(CODEX_PROVIDER_ID_SUFFIX) => {
                constants.get(name).cloned()
            }
            _ => None,
        })
        .collect()
}

/// The env vars codex-acp's `readApiKeyFromEnv` tries in order, resolved through the constants
/// `CodexAuthMethod.ts` exports.
fn codex_acp_key_env_vars(auth_method_source: &str, client_source: &str) -> Vec<String> {
    let constants = string_constants(&tokens(auth_method_source, Syntax::TypeScript));
    let client = tokens(client_source, Syntax::TypeScript);
    let mut env_vars: Vec<String> = Vec::new();
    for token in function_body(&client, CODEX_ACP_KEY_READER_FN)
        .into_iter()
        .flatten()
    {
        if let Token::Word(name) = token
            && let Some(env_var) = constants.get(name)
            && !env_vars.contains(env_var)
        {
            env_vars.push(env_var.clone());
        }
    }
    env_vars
}

/// The `id: '...'` values registered in provider-definition.ts: the `KIMI_MODEL_PROVIDER_TYPE`
/// values Kimi resolves.
fn kimi_provider_types(source: &str) -> BTreeSet<String> {
    tokens(source, Syntax::TypeScript)
        .windows(3)
        .filter_map(|window| match window {
            [Token::Word(field), Token::Punct(':'), Token::Str(value)]
                if field == TYPESCRIPT_ID_FIELD =>
            {
                Some(value.clone())
            }
            _ => None,
        })
        .collect()
}

/// The `{ id: "...", env: [...] }` provider literals Kilo's models.ts lays over models.dev.
fn kilo_overlay_env_vars(source: &str) -> BTreeMap<String, Vec<String>> {
    profile_env_lists(
        &tokens(source, Syntax::TypeScript),
        ':',
        TYPESCRIPT_ID_FIELD,
        KILO_ENV_FIELD,
    )
}

/// Reads each `<id_field><assign>"id"` profile with the `<env_field><assign>(...)` or `[...]` list
/// that follows it, the shape Hermes plugin keyword arguments and Kilo's object literals share.
fn profile_env_lists(
    tokens: &[Token],
    assign: char,
    id_field: &str,
    env_field: &str,
) -> BTreeMap<String, Vec<String>> {
    let mut profiles: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut current: Option<String> = None;
    for (index, window) in tokens.windows(3).enumerate() {
        let [Token::Word(key), Token::Punct(separator), value] = window else {
            continue;
        };
        if *separator != assign {
            continue;
        }
        match value {
            Token::Str(id) if key == id_field => {
                profiles.entry(id.clone()).or_default();
                current = Some(id.clone());
            }
            Token::Punct('(' | '[') if key == env_field => {
                let Some(env_vars) = current.as_ref().and_then(|id| profiles.get_mut(id)) else {
                    continue;
                };
                *env_vars = group_items(&tokens[index + 3..])
                    .into_iter()
                    .flat_map(strings)
                    .collect();
            }
            _ => {}
        }
    }
    profiles
}

/// `const NAME[: type] = "value";` declarations, the shape Rust and TypeScript share.
fn string_constants(tokens: &[Token]) -> BTreeMap<String, String> {
    let mut constants = BTreeMap::new();
    for (index, window) in tokens.windows(2).enumerate() {
        let [Token::Word(keyword), Token::Word(name)] = window else {
            continue;
        };
        if keyword != CONST_KEYWORD {
            continue;
        }
        let declaration = &tokens[index + 2..];
        let Some(assign) = declaration
            .iter()
            .position(|token| matches!(token, Token::Punct('=' | ';')))
        else {
            continue;
        };
        if let [Token::Punct('='), Token::Str(value), Token::Punct(';'), ..] =
            &declaration[assign..]
        {
            constants.insert(name.clone(), value.clone());
        }
    }
    constants
}

/// The brace-delimited body of the function named `name`. Its definition is the occurrence that
/// follows a definition keyword or starts a statement or class member, and whose parameter list
/// runs into a return type or the body; a call site fails one of the two.
fn function_body<'a>(tokens: &'a [Token], name: &str) -> Option<&'a [Token]> {
    tokens.iter().enumerate().find_map(|(index, token)| {
        if !matches!(token, Token::Word(word) if word == name) {
            return None;
        }
        let opens_definition = match index.checked_sub(1).map(|previous| &tokens[previous]) {
            Some(Token::Word(previous)) => DEFINITION_KEYWORDS.contains(&previous.as_str()),
            Some(Token::Punct('{' | '}' | ';')) | None => true,
            Some(_) => false,
        };
        if !opens_definition {
            return None;
        }
        let parameters_end = index + 1 + balanced_group_len(&tokens[index + 1..])?;
        if !matches!(tokens.get(parameters_end)?, Token::Punct('{' | ':' | '-')) {
            return None;
        }
        let open = parameters_end
            + tokens[parameters_end..]
                .iter()
                .position(|token| *token == Token::Punct('{'))?;
        let close = open + balanced_group_len(&tokens[open..])?;
        Some(&tokens[open + 1..close - 1])
    })
}

/// Length of the bracketed group that opens at `tokens[0]`, its closer included.
fn balanced_group_len(tokens: &[Token]) -> Option<usize> {
    if !matches!(tokens.first(), Some(Token::Punct('(' | '[' | '{'))) {
        return None;
    }
    let mut depth = 0usize;
    for (index, token) in tokens.iter().enumerate() {
        match token {
            Token::Punct('(' | '[' | '{') => depth += 1,
            Token::Punct(')' | ']' | '}') => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(index + 1);
                }
            }
            _ => {}
        }
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Str(String),
    Word(String),
    Punct(char),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Syntax {
    Python,
    Rust,
    TypeScript,
}

impl Syntax {
    fn line_comment(self) -> &'static str {
        match self {
            Self::Python => "#",
            Self::Rust | Self::TypeScript => "//",
        }
    }

    fn block_comment(self) -> Option<(&'static str, &'static str)> {
        match self {
            Self::Python => None,
            Self::Rust | Self::TypeScript => Some(("/*", "*/")),
        }
    }

    /// Rust's `'` opens a char literal or a lifetime instead, handled apart from strings.
    fn is_quote(self, ch: char) -> bool {
        match self {
            Self::Python => ch == '"' || ch == '\'',
            Self::Rust => ch == '"',
            Self::TypeScript => ch == '"' || ch == '\'' || ch == '`',
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
        let block_comment = syntax
            .block_comment()
            .filter(|(open, _)| rest.starts_with(*open));
        if ch.is_whitespace() {
            rest = &rest[ch.len_utf8()..];
        } else if rest.starts_with(syntax.line_comment()) {
            rest = rest.find('\n').map_or("", |end| &rest[end..]);
        } else if let Some((open, close)) = block_comment {
            let body = &rest[open.len()..];
            rest = body
                .find(close)
                .map_or("", |end| &body[end + close.len()..]);
        } else if syntax == Syntax::Rust
            && let Some((value, remaining)) = rust_raw_string(rest)
        {
            tokens.push(Token::Str(value));
            rest = remaining;
        } else if syntax == Syntax::Rust && ch == '\'' {
            // Char literals never carry table data; skipping them keeps a quote or bracket
            // inside one from skewing the stream. A lifetime stays punctuation.
            match rust_char_literal_len(rest) {
                Some(len) => rest = &rest[len..],
                None => {
                    tokens.push(Token::Punct(ch));
                    rest = &rest[ch.len_utf8()..];
                }
            }
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

/// A Rust raw string (`r"..."`, `r#"..."#`, `br#"..."#`) at the start of `source`, which takes no
/// escapes and may hold bare quotes.
fn rust_raw_string(source: &str) -> Option<(String, &str)> {
    let after_prefix = source
        .strip_prefix("br")
        .or_else(|| source.strip_prefix('r'))?;
    let hashes = after_prefix.len() - after_prefix.trim_start_matches('#').len();
    let body = after_prefix[hashes..].strip_prefix('"')?;
    let closer = format!("\"{}", "#".repeat(hashes));
    let end = body.find(&closer)?;
    Some((body[..end].to_owned(), &body[end + closer.len()..]))
}

/// Byte length of a Rust char literal at the start of `source`; `None` when the `'` opens a
/// lifetime.
fn rust_char_literal_len(source: &str) -> Option<usize> {
    let body = source.strip_prefix('\'')?;
    let first = body.chars().next()?;
    let closing_quote = if first == '\\' {
        2 + body.get(2..)?.find('\'')?
    } else {
        let width = first.len_utf8();
        body[width..].starts_with('\'').then_some(width)?
    };
    Some(closing_quote + 2)
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

    fn owned(values: &[&str]) -> BTreeSet<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
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
    fn rust_tokens_skip_char_literals_and_read_raw_strings() {
        let tokens = tokens(
            "f::<'a>('\"', '[', '\\'', r#\"{\"k\": \"v\"}\"#) /* \"( */ // \"\n\"end\"",
            Syntax::Rust,
        );

        assert_eq!(
            tokens,
            [
                Token::Word("f".to_owned()),
                Token::Punct(':'),
                Token::Punct(':'),
                Token::Punct('<'),
                Token::Punct('\''),
                Token::Word("a".to_owned()),
                Token::Punct('>'),
                Token::Punct('('),
                Token::Punct(','),
                Token::Punct(','),
                Token::Punct(','),
                Token::Str("{\"k\": \"v\"}".to_owned()),
                Token::Punct(')'),
                Token::Str("end".to_owned()),
            ]
        );
    }

    #[test]
    fn typescript_tokens_read_template_literals_and_drop_block_comments() {
        let tokens = tokens(
            "/* 'x */ const a = `${b} \"q\" '`; // c'\nf('d')",
            Syntax::TypeScript,
        );

        assert_eq!(
            tokens,
            [
                Token::Word("const".to_owned()),
                Token::Word("a".to_owned()),
                Token::Punct('='),
                Token::Str("${b} \"q\" '".to_owned()),
                Token::Punct(';'),
                Token::Word("f".to_owned()),
                Token::Punct('('),
                Token::Str("d".to_owned()),
                Token::Punct(')'),
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

    #[test]
    fn goose_rust_provider_reads_name_constants_and_config_keys_across_files() {
        let provider_source = r##"
use goose_provider_types::formats::anthropic::{ANTHROPIC_PROVIDER_NAME, thinking_type};

pub const ANTHROPIC_DEFAULT_MODEL: &str = "claude-sonnet-5";

impl ProviderDef for AnthropicProvider {
    fn metadata() -> ProviderMetadata {
        ProviderMetadata::new(
            ANTHROPIC_PROVIDER_NAME,
            "Anthropic",
            vec![
                ConfigKey::new("ANTHROPIC_API_KEY", true, true, None, true),
                ConfigKey::new(
                    "ANTHROPIC_HOST",
                    true,
                    false,
                    Some("https://api.anthropic.com"),
                    false,
                ),
            ],
        )
    }
}

fn split_host<'a>(host: &'a str) -> &'a str {
    host.split(['/', '?']).next().unwrap_or(host)
}

#[test]
fn escapes() {
    let body = r#"{"text":"escaped quote \" then"}"#;
    assert!(body.contains('"'));
}
"##;
        let formats_source = r#"
/// Anthropic's name in goose's provider registry.
pub const ANTHROPIC_PROVIDER_NAME: &str = "anthropic";
"#;

        let provider =
            goose_rust_provider(&[provider_source.to_owned(), formats_source.to_owned()]);

        assert_eq!(provider.names, owned(&["anthropic"]));
        assert_eq!(
            provider.config_keys,
            owned(&["ANTHROPIC_API_KEY", "ANTHROPIC_HOST"])
        );
        assert_eq!(goose_rust_provider(&[]), GooseProvider::default());
    }

    #[test]
    fn goose_definition_maps_name_and_key_env() {
        let definition: GooseDefinition = serde_json::from_str(
            r#"{
  "name": "mistral",
  "engine": "openai",
  "display_name": "Mistral AI",
  "api_key_env": "MISTRAL_API_KEY",
  "base_url": "https://api.mistral.ai/v1/chat/completions",
  "models": [{ "name": "mistral-medium-latest", "context_limit": 128000 }]
}"#,
        )
        .expect("definition");

        assert_eq!(
            GooseProvider::from(definition),
            GooseProvider {
                names: owned(&["mistral"]),
                config_keys: owned(&["MISTRAL_API_KEY"]),
            }
        );
        let keyless: GooseDefinition =
            serde_json::from_str(r#"{"name": "lmstudio", "api_key_env": ""}"#).expect("keyless");
        assert!(GooseProvider::from(keyless).config_keys.is_empty());
    }

    #[test]
    fn codex_built_ins_come_from_the_returned_array_not_every_id_constant() {
        let source = r#"
pub const OPENAI_PROVIDER_ID: &str = "openai";
pub const AMAZON_BEDROCK_PROVIDER_ID: &str = "amazon-bedrock";
pub const LEGACY_OLLAMA_CHAT_PROVIDER_ID: &str = "ollama-chat";
pub const OLLAMA_OSS_PROVIDER_ID: &str = "ollama";

fn defaults() -> HashMap<String, ModelProviderInfo> {
    for (id, _) in built_in_model_providers(None) {
        let _ = (id, LEGACY_OLLAMA_CHAT_PROVIDER_ID);
    }
    built_in_model_providers(None)
}

/// Built-in default provider list.
pub fn built_in_model_providers(
    openai_base_url: Option<String>,
) -> HashMap<String, ModelProviderInfo> {
    use ModelProviderInfo as P;
    let amazon_bedrock_provider = P::create_amazon_bedrock_provider(/*aws*/ None);
    [
        (OPENAI_PROVIDER_ID, P::create_openai_provider(openai_base_url)),
        (AMAZON_BEDROCK_PROVIDER_ID, amazon_bedrock_provider),
        (OLLAMA_OSS_PROVIDER_ID, create_oss_provider(DEFAULT_OLLAMA_PORT, WireApi::Responses)),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

pub fn merge_configured_model_providers() {
    let _ = LEGACY_OLLAMA_CHAT_PROVIDER_ID;
}
"#;

        assert_eq!(
            codex_built_in_provider_ids(source),
            owned(&["amazon-bedrock", "ollama", "openai"])
        );
        assert!(
            codex_built_in_provider_ids("pub const OPENAI_PROVIDER_ID: &str = \"openai\";")
                .is_empty()
        );
    }

    #[test]
    fn codex_acp_key_env_vars_follow_the_reader_definition_in_order() {
        let auth_method = r#"
import type {AuthMethod} from "@agentclientprotocol/sdk";

export const CODEX_API_KEY_ENV_VAR = "CODEX_API_KEY";
export const OPENAI_API_KEY_ENV_VAR = "OPENAI_API_KEY";
export const UNRELATED_ENV_VAR = "NO_BROWSER";
"#;
        let client = r#"
import {CODEX_API_KEY_ENV_VAR, OPENAI_API_KEY_ENV_VAR} from "./CodexAuthMethod";

export class CodexAcpClient {
    async authenticate(authRequest: AuthenticateRequest): Promise<boolean> {
        const apiKey = authRequest._meta?.["api-key"]?.apiKey ?? this.readApiKeyFromEnv();
        const fallback = useEnv ? this.readApiKeyFromEnv() : "";
        if (fallback) { console.log(UNRELATED_ENV_VAR); }
        return apiKey.length > 0;
    }

    private readApiKeyFromEnv(): string {
        for (const envVar of [CODEX_API_KEY_ENV_VAR, OPENAI_API_KEY_ENV_VAR]) {
            const value = process.env[envVar]?.trim();
            if (value) {
                return value;
            }
        }
        throw RequestError.internalError(
            {envVars: [CODEX_API_KEY_ENV_VAR, OPENAI_API_KEY_ENV_VAR]},
            "No API key found",
        );
    }
}
"#;

        assert_eq!(
            codex_acp_key_env_vars(auth_method, client),
            ["CODEX_API_KEY", "OPENAI_API_KEY"]
        );
        assert!(codex_acp_key_env_vars(auth_method, "export class Empty {}").is_empty());
    }

    #[test]
    fn kimi_provider_types_read_registered_ids_only() {
        let source = r#"
export interface ProviderDefinition<N extends Protocol = Protocol> {
  readonly id: string;
}

export function getProviderDefinition(
  id: string,
  protocol?: Protocol,
): ProviderDefinition | undefined {
  throw new BugIndicatingError(
    `provider definition '${definition.id}' is already registered for protocol '${definition.baseProtocol}'`,
  );
}

registerProviderDefinition({
  id: 'anthropic',
  baseProtocol: 'anthropic',
  endpoint: { apiKeyEnv: 'ANTHROPIC_API_KEY', baseUrlEnv: 'ANTHROPIC_BASE_URL' },
});

registerProviderDefinition({
  id: 'openai_responses',
  baseProtocol: 'openai_responses',
});

registerProviderDefinition({
  id: 'kimi',
  baseProtocol: 'openai',
});
"#;

        assert_eq!(
            kimi_provider_types(source),
            owned(&["anthropic", "kimi", "openai_responses"])
        );
        assert!(kimi_provider_types("export const PROVIDERS = [];").is_empty());
    }

    #[test]
    fn kilo_overlay_reads_provider_literals() {
        let source = r#"
// kilocode_change - new file
function baseURL(url: string | undefined, org: string | undefined) {
  const base = url.replace(/\/+$/, "")
  if (base.endsWith("/api")) return `${base}/openrouter`
  return `${base}/api/openrouter`
}

const get = Effect.fn("ModelsDev.get")(function* () {
  providers.apertis = {
    id: "apertis",
    name: "Apertis",
    env: ["APERTIS_API_KEY"],
    api: aptURL,
  }
  providers.kilo = {
    id: "kilo",
    name: "Kilo Gateway",
    env: ["KILO_API_KEY"],
    api: KILO_OPENROUTER_BASE.endsWith("/") ? KILO_OPENROUTER_BASE : `${KILO_OPENROUTER_BASE}/`,
  }
})
"#;

        let overlay = kilo_overlay_env_vars(source);

        assert_eq!(overlay["kilo"], ["KILO_API_KEY"]);
        assert_eq!(overlay["apertis"], ["APERTIS_API_KEY"]);
        assert_eq!(overlay.len(), 2);
        assert!(kilo_overlay_env_vars("export const layer = Layer.empty").is_empty());
    }

    /// One `[[api_keys]]` and one `[[providers]]` entry per `(provider id, key env)`, each run by
    /// `agent` alone; `profile` is appended to every provider entry.
    fn single_agent_mapping(
        agent: &str,
        rows: &[(&str, &str)],
        profile: &str,
    ) -> ProviderKeyMapping {
        let mut body = String::new();
        for (provider_id, env_var) in rows {
            body.push_str(&format!(
                "[[api_keys]]\nenv_var = \"{env_var}\"\nprovider_ids = [\"{provider_id}\"]\n\n"
            ));
        }
        for (provider_id, _) in rows {
            body.push_str(&format!(
                "[[providers]]\nid = [\"{provider_id}\"]\nname = \"{provider_id}\"\nagents = [\"{agent}\"]\n{profile}\n"
            ));
        }
        ProviderKeyMapping::from_toml(&body).expect("fixture mapping")
    }

    fn strings_of(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn failures_mentioning(report: &Report, needles: &[&str]) {
        assert_eq!(
            report.failures.len(),
            needles.len(),
            "failures: {:?}",
            report.failures
        );
        for needle in needles {
            assert!(
                report
                    .failures
                    .iter()
                    .any(|failure| failure.contains(needle)),
                "no failure mentions {needle}: {:?}",
                report.failures
            );
        }
    }

    #[test]
    fn hermes_evaluation_fails_unknown_ids_key_and_base_url_mismatches() {
        let mapping = single_agent_mapping(
            HERMES_AGENT_ID,
            &[
                ("deepseek", "DEEPSEEK_API_KEY"),
                ("anthropic", "ANTHROPIC_API_KEY"),
                ("xai", "XAI_API_KEY"),
                ("gemini", "GEMINI_API_KEY"),
                ("openrouter", "OPENROUTER_API_KEY"),
                ("gone", "GONE_API_KEY"),
            ],
            "[providers.hermes]\n",
        );
        let mut upstream = HermesUpstream {
            providers: BTreeMap::from([
                (
                    "deepseek".to_owned(),
                    provider(&["DEEPSEEK_API_KEY"], Some("DEEPSEEK_BASE_URL")),
                ),
                ("anthropic".to_owned(), provider(&["ANTHROPIC_TOKEN"], None)),
                (
                    "xai".to_owned(),
                    provider(&["XAI_API_KEY"], Some("XAI_API_BASE")),
                ),
                (
                    "gemini".to_owned(),
                    provider(
                        &["GOOGLE_API_KEY", "GEMINI_API_KEY"],
                        Some("GEMINI_BASE_URL"),
                    ),
                ),
                (
                    "openrouter".to_owned(),
                    provider(&["OPENROUTER_API_KEY"], None),
                ),
                ("nous".to_owned(), provider(&[], None)),
            ]),
            plugin_dirs: owned(&["commandcode", "openrouter"]),
            own_rung_literals: owned(&["OPENROUTER_BASE_URL"]),
        };
        let mut verified = BTreeSet::new();
        let mut report = Report::default();

        evaluate_hermes(&mapping, &upstream, &mut verified, &mut report);

        failures_mentioning(
            &report,
            &[
                "no provider `gone`",
                "`anthropic` reads ANTHROPIC_TOKEN",
                "`xai` reads base URL env `XAI_API_BASE`",
            ],
        );
        assert_eq!(verified, owned(&["deepseek", "gemini", "openrouter"]));
        assert!(
            report
                .info
                .iter()
                .any(|line| line.ends_with(": commandcode, nous"))
        );
        assert!(
            report
                .info
                .iter()
                .any(|line| line.contains("gemini (GEMINI_BASE_URL)"))
        );

        upstream.own_rung_literals.clear();
        let mut report = Report::default();
        evaluate_hermes(&mapping, &upstream, &mut BTreeSet::new(), &mut report);
        assert!(
            report
                .failures
                .iter()
                .any(|failure| failure.contains("`openrouter` reads base URL env (none)"))
        );
    }

    #[test]
    fn goose_evaluation_fails_missing_providers_names_keys_and_host_settings() {
        let mapping = single_agent_mapping(
            GOOSE_AGENT_ID,
            &[
                ("openai", "OPENAI_API_KEY"),
                ("mistral", "MISTRAL_API_KEY"),
                ("groq", "GROQ_API_KEY"),
                ("xai", "XAI_API_KEY"),
                ("cerebras", "CEREBRAS_API_KEY"),
                ("together", "TOGETHER_API_KEY"),
            ],
            "",
        );
        let goose = |names: &[&str], config_keys: &[&str]| GooseProvider {
            names: owned(names),
            config_keys: owned(config_keys),
        };
        let providers = BTreeMap::from([
            (
                "openai".to_owned(),
                goose(&["openai"], &["OPENAI_API_KEY", "OPENAI_HOST"]),
            ),
            (
                "mistral".to_owned(),
                goose(&["mistral"], &["MISTRAL_API_KEY"]),
            ),
            ("xai".to_owned(), goose(&["xai"], &["XAI_API_KEY"])),
            (
                "cerebras".to_owned(),
                goose(&["cerebras_ai"], &["CEREBRAS_API_KEY"]),
            ),
            (
                "together".to_owned(),
                goose(&["together"], &["TOGETHER_KEY"]),
            ),
        ]);
        let mut verified = BTreeSet::new();
        let mut report = Report::default();

        evaluate_goose(
            &mapping,
            &owned(&["cerebras", "deepseek", "mistral", "together"]),
            &providers,
            &mut verified,
            &mut report,
        );

        failures_mentioning(
            &report,
            &[
                "no provider `groq`",
                "no `XAI_HOST` setting",
                "`cerebras` declares provider name cerebras_ai",
                "`together` reads TOGETHER_KEY",
            ],
        );
        assert_eq!(verified, owned(&["mistral", "openai"]));
        assert!(report.info.iter().any(|line| line.ends_with(": deepseek")));
    }

    #[test]
    fn codex_evaluation_fails_lost_built_ins_collisions_and_key_mismatches() {
        let mapping = single_agent_mapping(
            CODEX_AGENT_ID,
            &[
                ("openai", "OPENAI_API_KEY"),
                ("openrouter", "OPENROUTER_API_KEY"),
            ],
            "",
        );
        let reader = strings_of(&["CODEX_API_KEY", "OPENAI_API_KEY"]);

        let mut verified = BTreeSet::new();
        let mut report = Report::default();
        evaluate_codex(
            &mapping,
            &owned(&["ollama", "openai"]),
            &reader,
            &mut verified,
            &mut report,
        );
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(verified, owned(&["openai"]));
        assert!(report.info.iter().any(|line| line.ends_with(": ollama")));

        let mut report = Report::default();
        evaluate_codex(
            &mapping,
            &owned(&["ollama", "openrouter"]),
            &reader,
            &mut BTreeSet::new(),
            &mut report,
        );
        failures_mentioning(
            &report,
            &[
                "no longer builds in provider `openai`",
                "builds in provider `openrouter`",
            ],
        );

        let mut report = Report::default();
        evaluate_codex(
            &mapping,
            &owned(&["openai"]),
            &strings_of(&["CODEX_API_KEY"]),
            &mut BTreeSet::new(),
            &mut report,
        );
        failures_mentioning(&report, &["shipped data has `OPENAI_API_KEY`"]);

        let mut report = Report::default();
        evaluate_codex(
            &mapping,
            &BTreeSet::new(),
            &reader,
            &mut BTreeSet::new(),
            &mut report,
        );
        failures_mentioning(&report, &["yielded no providers"]);
    }

    #[test]
    fn kimi_evaluation_fails_unread_env_vars_and_unregistered_types() {
        let all_env_vars: BTreeSet<String> =
            KIMI_MODEL_ENV_VARS.into_iter().map(str::to_owned).collect();
        let mut literals = all_env_vars.clone();
        literals.remove("KIMI_MODEL_DISPLAY_NAME");

        let mut report = Report::default();
        evaluate_kimi(
            &literals,
            &owned(&["anthropic", "kimi", "openai"]),
            &mut report,
        );
        failures_mentioning(
            &report,
            &[
                "no longer reads KIMI_MODEL_DISPLAY_NAME",
                "no provider type openai_responses",
            ],
        );

        let mut report = Report::default();
        evaluate_kimi(
            &all_env_vars,
            &owned(&[
                "anthropic",
                "google-genai",
                "kimi",
                "openai",
                "openai_responses",
            ]),
            &mut report,
        );
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert!(
            report
                .info
                .iter()
                .any(|line| line.ends_with(": google-genai"))
        );

        let mut report = Report::default();
        evaluate_kimi(&all_env_vars, &BTreeSet::new(), &mut report);
        failures_mentioning(&report, &["yielded no provider types"]);
    }

    #[test]
    fn kilo_evaluation_fails_unknown_providers_and_env_mismatches() {
        let mapping = single_agent_mapping(
            KILO_AGENT_ID,
            &[
                ("kilo", "KILO_API_KEY"),
                ("openrouter", "OPENROUTER_API_KEY"),
                ("venice", "VENICE_API_KEY"),
                ("zenmux", "ZENMUX_API_KEY"),
            ],
            "",
        );
        let overlay = BTreeMap::from([("kilo".to_owned(), strings_of(&["KILO_API_KEY"]))]);
        let models_dev = BTreeMap::from([
            ("openrouter".to_owned(), strings_of(&["OPENROUTER_API_KEY"])),
            ("venice".to_owned(), strings_of(&["VENICE_KEY"])),
        ]);
        let mut verified = BTreeSet::new();
        let mut report = Report::default();

        evaluate_kilo(&mapping, &overlay, &models_dev, &mut verified, &mut report);

        failures_mentioning(
            &report,
            &[
                "no provider `zenmux`",
                "models.dev maps `venice` to VENICE_KEY",
            ],
        );
        assert_eq!(verified, owned(&["kilo", "openrouter"]));

        let mut report = Report::default();
        evaluate_kilo(
            &mapping,
            &BTreeMap::new(),
            &models_dev,
            &mut BTreeSet::new(),
            &mut report,
        );
        failures_mentioning(&report, &["yielded no provider overlay"]);
    }
}
