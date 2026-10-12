//! Dev tool: fact-check shipped data files against external metadata sources.

mod agent_sources;

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::process::ExitCode;
use std::time::Duration;

use acp_stack::runtime::agent::provider_keys::{ProviderEnvMapping, ProviderKeyMapping};
use acp_stack::runtime::install::agent_registry::{
    AdapterSpec, GithubInstall, InstallSet, RegistryCatalog, RegistryEntry, github_repo_from_url,
};
use acp_stack::runtime::install::skill_registry::SkillCatalog;
use serde::Deserialize;
use serde_json::Value;

const AGENTS_TOML: &str = include_str!("../../../data/agents.toml");
const ENV_VARS_TOML: &str = include_str!("../../../data/env_vars.toml");
const PROVIDERS_TOML: &str = include_str!("../../../data/providers.toml");
const SKILLS_TOML: &str = include_str!("../../../data/skills.toml");

const ACP_REGISTRY_REPO: &str = "agentclientprotocol/registry";
const ACP_REGISTRY_ASSET: &str = "registry.json";
const MODELS_DEV_API_URL: &str = "https://models.dev/api.json";
const OPENCODE_AGENT_ID: &str = "opencode";
const PI_REPO: &str = "earendil-works/pi";
// Pi's provider doc lists display names, not ids; this source is the id-to-env map Pi runs.
const PI_ENV_API_KEYS_PATH: &str = "packages/ai/src/env-api-keys.ts";
const AMP_ACP_REPO: &str = "tao12345666333/amp-acp";
// The Amp manual dropped its env-var appendix; the amp-acp adapter README is the
// document that names the headless credential acp-stack actually injects.
const AMP_ACP_README_PATH: &str = "README.md";
const OPENCODE_PROVIDER_DOC_URL: &str = "https://opencode.ai/docs/providers/";
const GITHUB_API_BASE: &str = "https://api.github.com";
const GITHUB_RAW_MEDIA_TYPE: &str = "application/vnd.github.raw";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const USER_AGENT: &str = concat!("acp-stack-fact-check/", env!("CARGO_PKG_VERSION"));

fn main() -> ExitCode {
    match run() {
        Ok(report) => {
            report.print();
            if report.has_failures() {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(err) => {
            eprintln!("Error: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<Report, Box<dyn Error>> {
    let mut report = Report::default();
    let http = Http::new()?;

    let agents = RegistryCatalog::from_toml(AGENTS_TOML)?;
    let provider_mapping = ProviderKeyMapping::from_toml_parts(ENV_VARS_TOML, PROVIDERS_TOML)?;
    let skills = SkillCatalog::from_toml(SKILLS_TOML)?;
    report.ok("embedded data parsers accepted agents/providers/env-vars/skills");

    check_acp_registry(&http, &agents, &mut report)?;
    check_npm_packages(&http, &agents, &mut report)?;
    check_github_release_assets(&http, &agents, &mut report)?;
    check_skill_sources(&http, &skills, &mut report)?;
    check_provider_sources(&http, &provider_mapping, &mut report)?;

    Ok(report)
}

fn check_acp_registry(
    http: &Http,
    catalog: &RegistryCatalog,
    report: &mut Report,
) -> Result<(), Box<dyn Error>> {
    let release: GithubRelease =
        http.github_json(&format!("/repos/{ACP_REGISTRY_REPO}/releases/latest"))?;
    let registry_asset = release
        .assets
        .iter()
        .find(|asset| asset.name == ACP_REGISTRY_ASSET)
        .ok_or_else(|| {
            format!(
                "{ACP_REGISTRY_REPO} latest release `{}` has no `{ACP_REGISTRY_ASSET}` asset",
                release.tag_name
            )
        })?;
    let registry_url = registry_asset.browser_download_url.as_deref().ok_or_else(|| {
        format!(
            "{ACP_REGISTRY_REPO} latest release `{}` asset `{ACP_REGISTRY_ASSET}` has no download URL",
            release.tag_name
        )
    })?;
    let upstream: AcpRegistry = http.get_json(registry_url)?;
    report.ok(format!(
        "loaded `{ACP_REGISTRY_ASSET}` from {ACP_REGISTRY_REPO} latest release `{}`",
        release.tag_name
    ));
    let upstream_ids: BTreeSet<_> = upstream.agents.into_iter().map(|agent| agent.id).collect();
    let embedded_ids = embedded_sync_ids(catalog);
    let missing: Vec<_> = embedded_ids
        .iter()
        .filter(|id| !upstream_ids.contains(*id))
        .cloned()
        .collect();
    if missing.is_empty() {
        report.ok(format!(
            "ACP registry contains all embedded sync ids ({})",
            embedded_ids.len()
        ));
    } else {
        for id in missing {
            report.fail(format!("ACP registry is missing embedded sync id `{id}`"));
        }
    }

    let unembedded_count = upstream_ids.difference(&embedded_ids).count();
    if unembedded_count > 0 {
        report.info(format!(
            "ACP registry has {unembedded_count} additional entries not embedded by acp-stack"
        ));
    }
    Ok(())
}

fn check_npm_packages(
    http: &Http,
    catalog: &RegistryCatalog,
    report: &mut Report,
) -> Result<(), Box<dyn Error>> {
    let packages = npm_packages(catalog);
    for package in &packages {
        let encoded = package.replace('/', "%2F");
        let url = format!("https://registry.npmjs.org/{encoded}/latest");
        let response: NpmLatest = http.get_json(&url)?;
        if response.version.trim().is_empty() {
            report.fail(format!("npm package `{package}` latest version is empty"));
        } else {
            report.ok(format!(
                "npm package `{package}` exists at {}",
                response.version
            ));
        }
    }
    if packages.is_empty() {
        report.info("no npm install packages declared");
    }
    Ok(())
}

fn check_github_release_assets(
    http: &Http,
    catalog: &RegistryCatalog,
    report: &mut Report,
) -> Result<(), Box<dyn Error>> {
    let checks = github_install_checks(catalog)?;
    for check in &checks {
        let release: GithubRelease = http.github_json(&format!(
            "/repos/{}/releases/latest",
            check.repo.trim_matches('/')
        ))?;
        let assets: Vec<_> = release
            .assets
            .iter()
            .map(|asset| asset.name.as_str())
            .collect();
        for pattern in github_asset_patterns(&check.install.asset_pattern, check.install) {
            let matches = assets
                .iter()
                .filter(|asset| glob_match(&pattern, asset))
                .count();
            match matches {
                0 => report.fail(format!(
                    "{} latest release `{}` has no asset matching `{pattern}`",
                    check.label, release.tag_name
                )),
                1 => report.ok(format!(
                    "{} latest release `{}` has asset matching `{pattern}`",
                    check.label, release.tag_name
                )),
                count => report.fail(format!(
                    "{} latest release `{}` has {count} assets matching `{pattern}`",
                    check.label, release.tag_name
                )),
            }
        }
    }
    if checks.is_empty() {
        report.info("no GitHub release install specs declared");
    }
    Ok(())
}

fn check_skill_sources(
    http: &Http,
    catalog: &SkillCatalog,
    report: &mut Report,
) -> Result<(), Box<dyn Error>> {
    for source in catalog.sources() {
        let repo = format!("{}/{}", source.owner, source.repo);
        let branch: GithubCommit = http.github_json(&format!(
            "/repos/{repo}/commits/{}",
            source.branch.trim_matches('/')
        ))?;
        report.ok(format!(
            "skill source `{}` branch `{}` exists at {}",
            source.id, source.branch, branch.sha
        ));

        if let Some(commit) = source.verified_commit.as_deref() {
            let verified: GithubCommit =
                http.github_json(&format!("/repos/{repo}/commits/{commit}"))?;
            report.ok(format!(
                "skill source `{}` pinned commit exists at {}",
                source.id, verified.sha
            ));
            if verified.sha != branch.sha {
                report.manual(format!(
                    "skill source `{}` branch head is {}; pinned reviewed commit is {}",
                    source.id, branch.sha, verified.sha
                ));
            }
        }
        if let Some(commit) = source.indexed_commit.as_deref() {
            let indexed: GithubCommit =
                http.github_json(&format!("/repos/{repo}/commits/{commit}"))?;
            report.ok(format!(
                "skill source `{}` indexed commit exists at {}",
                source.id, indexed.sha
            ));
        }

        let directory_ref = source
            .indexed_commit
            .as_deref()
            .or(source.verified_commit.as_deref())
            .unwrap_or(&source.branch);
        for directory in &source.directories {
            let path = directory.path.trim_matches('/');
            let endpoint = if path.is_empty() {
                format!("/repos/{repo}/contents?ref={directory_ref}")
            } else {
                format!("/repos/{repo}/contents/{path}?ref={directory_ref}")
            };
            let _: Value = http.github_json(&endpoint)?;
            report.ok(format!(
                "skill source `{}` directory `{}` exists at `{directory_ref}`",
                source.id, directory.path
            ));
        }
    }
    Ok(())
}

fn check_provider_sources(
    http: &Http,
    mapping: &ProviderKeyMapping,
    report: &mut Report,
) -> Result<(), Box<dyn Error>> {
    let models_dev: Value = http.get_json(MODELS_DEV_API_URL)?;
    let models_dev_providers = models_dev_provider_names(&models_dev);
    let models_dev_env_vars = models_dev_provider_env_vars(&models_dev);
    let pi_env_api_keys = http.github_raw_text(PI_REPO, PI_ENV_API_KEYS_PATH)?;
    let amp_acp_readme = http.github_raw_text(AMP_ACP_REPO, AMP_ACP_README_PATH)?;
    let opencode_doc = http.get_text(OPENCODE_PROVIDER_DOC_URL)?;

    let mut verified = BTreeSet::new();
    check_pi_env_api_keys(mapping, &pi_env_api_keys, &mut verified, report);
    agent_sources::check_hermes_sources(http, mapping, &mut verified, report)?;
    agent_sources::check_goose_sources(http, mapping, &mut verified, report)?;
    agent_sources::check_codex_sources(http, mapping, &mut verified, report)?;
    agent_sources::check_kimi_sources(http, report)?;
    agent_sources::check_kilo_sources(http, mapping, &models_dev_env_vars, &mut verified, report)?;
    check_agent_env_doc(
        "amp-code",
        "amp-acp README",
        "AMP_API_KEY",
        &amp_acp_readme,
        &mut verified,
        report,
    );
    check_opencode_doc(&opencode_doc, &mut verified, report);

    let mut models_dev_tier = BTreeSet::new();
    for provider in models_dev_scoped_providers(mapping) {
        for provider_id in provider.ids() {
            match models_dev_providers.get(provider_id) {
                Some(name) if name == &provider.name => {
                    models_dev_tier.insert(provider_id.clone());
                    report.ok(format!(
                        "models.dev provider `{provider_id}` matches display name `{name}`"
                    ));
                }
                Some(name) => {
                    report.fail(format!(
                        "models.dev provider `{provider_id}` has display name `{name}`, shipped data has `{}`",
                        provider.name
                    ));
                }
                None => {
                    report.fail(format!(
                        "models.dev does not list OpenCode-only provider `{provider_id}`"
                    ));
                }
            }
        }
    }
    check_opencode_models_dev_ids(mapping, &models_dev_env_vars, &mut models_dev_tier, report);

    let all_provider_ids: BTreeSet<_> = mapping
        .providers()
        .iter()
        .flat_map(|provider| provider.ids().iter().cloned())
        .collect();
    let manual: Vec<_> = all_provider_ids
        .difference(&verified)
        .filter(|id| !models_dev_tier.contains(*id))
        .cloned()
        .collect();

    report.info(format!(
        "provider confidence tiers: verified={}, models-dev={}, manual={}",
        verified.len(),
        models_dev_tier.len(),
        manual.len()
    ));
    if !manual.is_empty() {
        report.manual(format!(
            "provider ids requiring manual source review: {}",
            manual.join(", ")
        ));
    }
    Ok(())
}

fn check_pi_env_api_keys(
    mapping: &ProviderKeyMapping,
    source: &str,
    verified: &mut BTreeSet<String>,
    report: &mut Report,
) {
    let pi_env_vars = pi_provider_env_vars(source);
    if pi_env_vars.is_empty() {
        report.fail("Pi env-api-keys.ts yielded no provider env mappings; its format changed");
        return;
    }
    for provider in mapping
        .providers()
        .iter()
        .filter(|provider| provider.agents.iter().any(|agent| agent == "pi"))
    {
        let Some(native_id) = native_provider_id(provider, "pi") else {
            continue;
        };
        let Some(env_var) = env_var_for_agent_provider(mapping, "pi", native_id) else {
            continue;
        };
        match pi_env_vars.get(native_id) {
            Some(env_vars) if env_vars.contains(env_var) => {
                for provider_id in provider.ids() {
                    verified.insert(provider_id.clone());
                }
                report.ok(format!(
                    "Pi env-api-keys.ts verifies `{native_id}` uses `{env_var}`"
                ));
            }
            Some(env_vars) => report.fail(format!(
                "Pi env-api-keys.ts maps `{native_id}` to {}, shipped data has `{env_var}`",
                env_vars.iter().cloned().collect::<Vec<_>>().join(", ")
            )),
            None => report.fail(format!(
                "Pi env-api-keys.ts has no provider `{native_id}` (shipped env ref `{env_var}`)"
            )),
        }
    }
}

/// Reads the provider-to-env-var table out of Pi's `getApiKeyEnvVars`: the `envMap`
/// entries plus the `if (provider === "...")` early returns, whose arrays may name
/// exported string constants.
fn pi_provider_env_vars(source: &str) -> BTreeMap<String, BTreeSet<String>> {
    let constants: BTreeMap<&str, &str> = source
        .lines()
        .filter_map(|line| {
            let (name, value) = line
                .trim()
                .strip_prefix("export const ")?
                .split_once(" = ")?;
            Some((name, quoted_env_var(value.trim_end_matches(';'))?))
        })
        .collect();
    let mut env_vars: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut early_return_provider: Option<&str> = None;
    for line in source.lines().map(str::trim) {
        if let Some(provider_id) = early_return_provider.take()
            && let Some(items) = line
                .strip_prefix("return [")
                .and_then(|rest| rest.strip_suffix("];"))
        {
            let resolved = items.split(',').filter_map(|item| {
                let item = item.trim();
                quoted_env_var(item).or_else(|| constants.get(item).copied())
            });
            env_vars
                .entry(provider_id.to_owned())
                .or_default()
                .extend(resolved.map(str::to_owned));
            continue;
        }
        if let Some((_, rest)) = line.split_once("provider === \"") {
            early_return_provider = rest.split_once('"').map(|(provider_id, _)| provider_id);
            continue;
        }
        let Some((key, value)) = line.trim_end_matches(',').split_once(": ") else {
            continue;
        };
        let key = key
            .strip_prefix('"')
            .and_then(|key| key.strip_suffix('"'))
            .unwrap_or(key);
        let key_is_provider_id = !key.is_empty()
            && key
                .chars()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-');
        if !key_is_provider_id {
            continue;
        }
        if let Some(env_var) = quoted_env_var(value) {
            env_vars
                .entry(key.to_owned())
                .or_default()
                .insert(env_var.to_owned());
        }
    }
    env_vars
}

fn quoted_env_var(value: &str) -> Option<&str> {
    let inner = value.strip_prefix('"')?.strip_suffix('"')?;
    let is_env_var = !inner.is_empty()
        && inner
            .chars()
            .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_');
    is_env_var.then_some(inner)
}

fn check_agent_env_doc(
    provider_id: &str,
    label: &str,
    env_var: &str,
    body: &str,
    verified: &mut BTreeSet<String>,
    report: &mut Report,
) {
    if body.contains(env_var) {
        verified.insert(provider_id.to_owned());
        report.ok(format!("{label} docs verify direct env ref `{env_var}`"));
    } else {
        report.fail(format!(
            "{label} docs do not verify direct env ref `{env_var}`"
        ));
    }
}

fn check_opencode_doc(body: &str, verified: &mut BTreeSet<String>, report: &mut Report) {
    for (provider_id, label) in [("opencode", "OpenCode Zen"), ("opencode-go", "OpenCode Go")] {
        if body.contains(label) {
            verified.insert(provider_id.to_owned());
            report.ok(format!("OpenCode docs verify `{label}`"));
        } else {
            report.fail(format!("OpenCode docs do not verify `{label}`"));
        }
    }
}

/// OpenCode resolves the `provider.<id>` acp-stack writes against models.dev, so every other
/// OpenCode-enabled row's native id must be a models.dev provider. The key travels as
/// `options.apiKey`, which leaves models.dev's env names out of this contract.
fn check_opencode_models_dev_ids(
    mapping: &ProviderKeyMapping,
    models_dev_providers: &BTreeMap<String, Vec<String>>,
    models_dev_tier: &mut BTreeSet<String>,
    report: &mut Report,
) {
    let opencode_only: BTreeSet<&str> = models_dev_scoped_providers(mapping)
        .into_iter()
        .flat_map(|provider| provider.ids().iter().map(String::as_str))
        .collect();
    let mut listed = 0;
    let mut unlisted = 0;
    for provider in mapping.providers() {
        let Some(native_id) = native_provider_id(provider, OPENCODE_AGENT_ID) else {
            continue;
        };
        if opencode_only.contains(native_id) {
            continue;
        }
        if models_dev_providers.contains_key(native_id) {
            listed += 1;
            models_dev_tier.extend(provider.ids().iter().cloned());
        } else {
            unlisted += 1;
            report.fail(format!(
                "models.dev does not list `{native_id}`, the OpenCode provider id acp-stack writes for `{}`",
                provider.ids().first().map_or(native_id, String::as_str)
            ));
        }
    }
    if unlisted == 0 {
        report.ok(format!(
            "models.dev lists the OpenCode provider id of all {listed} rows outside the OpenCode-only name check"
        ));
    }
}

fn models_dev_scoped_providers(mapping: &ProviderKeyMapping) -> Vec<&ProviderEnvMapping> {
    mapping
        .providers()
        .iter()
        .filter(|provider| provider.agents.len() == 1 && provider.agents[0] == OPENCODE_AGENT_ID)
        .filter(|provider| {
            provider
                .ids()
                .iter()
                .all(|provider_id| primary_api_key_for_provider(mapping, provider_id).is_none())
        })
        .collect()
}

fn embedded_sync_ids(catalog: &RegistryCatalog) -> BTreeSet<String> {
    catalog
        .entries()
        .iter()
        .filter(|entry| !entry.sync_exempt)
        .map(|entry| {
            entry
                .adapter
                .as_ref()
                .map(|adapter| {
                    adapter
                        .sync_id
                        .clone()
                        .unwrap_or_else(|| adapter.id.clone())
                })
                .or_else(|| entry.sync_id.clone())
                .unwrap_or_else(|| entry.id.clone())
        })
        .collect()
}

fn npm_packages(catalog: &RegistryCatalog) -> BTreeSet<String> {
    let mut packages = BTreeSet::new();
    for entry in catalog.entries() {
        collect_npm_packages(
            &entry.harness.as_ref().expect("validated harness").install,
            &mut packages,
        );
        if let Some(adapter) = &entry.adapter {
            collect_npm_packages(&adapter.install, &mut packages);
        }
    }
    packages
}

fn collect_npm_packages(install: &InstallSet, packages: &mut BTreeSet<String>) {
    if let Some(npm) = &install.npm {
        packages.insert(npm.package.clone());
    }
}

fn github_install_checks(
    catalog: &RegistryCatalog,
) -> Result<Vec<GithubInstallCheck<'_>>, Box<dyn Error>> {
    let mut checks = Vec::new();
    for entry in catalog.entries() {
        let harness = entry.harness.as_ref().expect("validated harness");
        if let Some(github) = &harness.install.github {
            let repo = github_repo_from_url(&entry.id, "github", entry.github.as_deref().unwrap())?;
            checks.push(GithubInstallCheck {
                label: format!("agent `{}` harness `{}`", entry.id, harness.id),
                repo,
                install: github,
            });
        }
        if let Some(adapter) = &entry.adapter {
            collect_adapter_github_check(entry, adapter, &mut checks)?;
        }
    }
    Ok(checks)
}

fn collect_adapter_github_check<'a>(
    entry: &'a RegistryEntry,
    adapter: &'a AdapterSpec,
    checks: &mut Vec<GithubInstallCheck<'a>>,
) -> Result<(), Box<dyn Error>> {
    if let Some(github) = &adapter.install.github {
        let repo = github_repo_from_url(
            &entry.id,
            "adapter.github",
            adapter.github.as_deref().unwrap(),
        )?;
        checks.push(GithubInstallCheck {
            label: format!("agent `{}` adapter `{}`", entry.id, adapter.id),
            repo,
            install: github,
        });
    }
    Ok(())
}

fn github_asset_patterns(pattern: &str, install: &GithubInstall) -> BTreeSet<String> {
    let mut patterns = BTreeSet::new();
    if pattern.contains("{arch}") {
        for token in [
            install.arch.x86_64.as_deref(),
            install.arch.aarch64.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            patterns.insert(pattern.replace("{arch}", token));
        }
    } else {
        patterns.insert(pattern.to_owned());
    }
    if let Some(checksums_asset) = &install.checksums_asset {
        patterns.insert(checksums_asset.clone());
    }
    patterns
}

fn models_dev_provider_object(value: &Value) -> Option<&serde_json::Map<String, Value>> {
    value
        .get("providers")
        .and_then(Value::as_object)
        .or_else(|| value.as_object())
}

fn models_dev_provider_names(value: &Value) -> BTreeMap<String, String> {
    let Some(provider_object) = models_dev_provider_object(value) else {
        return BTreeMap::new();
    };
    provider_object
        .iter()
        .filter_map(|(id, provider)| provider_display_name(provider).map(|name| (id.clone(), name)))
        .collect()
}

/// Each provider's `env` array, the key env vars OpenCode-family agents read for it.
fn models_dev_provider_env_vars(value: &Value) -> BTreeMap<String, Vec<String>> {
    models_dev_provider_object(value)
        .into_iter()
        .flatten()
        .map(|(id, provider)| {
            let env_vars = provider
                .get("env")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect();
            (id.clone(), env_vars)
        })
        .collect()
}

fn provider_display_name(provider: &Value) -> Option<String> {
    for key in ["name", "title", "displayName", "display_name"] {
        if let Some(name) = provider.get(key).and_then(Value::as_str)
            && !name.trim().is_empty()
        {
            return Some(name.to_owned());
        }
    }
    None
}

fn env_var_for_agent_provider<'a>(
    mapping: &'a ProviderKeyMapping,
    agent_id: &str,
    provider_id: &str,
) -> Option<&'a str> {
    let provider = mapping
        .providers()
        .iter()
        .find(|provider| provider.ids().iter().any(|id| id == provider_id))?;
    if !provider.agents.iter().any(|agent| agent == agent_id) {
        return None;
    }
    provider
        .api_key_env_vars
        .get(agent_id)
        .map(String::as_str)
        .or_else(|| primary_api_key_for_provider(mapping, provider_id))
}

fn primary_api_key_for_provider<'a>(
    mapping: &'a ProviderKeyMapping,
    provider_id: &str,
) -> Option<&'a str> {
    mapping
        .api_keys()
        .iter()
        .find(|api_key| api_key.provider_ids.iter().any(|id| id == provider_id))
        .map(|api_key| api_key.env_var.as_str())
        .or_else(|| {
            let provider = mapping
                .providers()
                .iter()
                .find(|provider| provider.ids().iter().any(|id| id == provider_id))?;
            mapping
                .api_keys()
                .iter()
                .find(|api_key| {
                    api_key.provider_ids.iter().any(|api_provider_id| {
                        provider.ids().iter().any(|id| id == api_provider_id)
                    })
                })
                .map(|api_key| api_key.env_var.as_str())
        })
}

fn native_provider_id<'a>(provider: &'a ProviderEnvMapping, agent_id: &str) -> Option<&'a str> {
    if !provider.agents.iter().any(|agent| agent == agent_id) {
        return None;
    }
    provider
        .provider_ids
        .iter()
        .find_map(|(provider_id, mapped_agent_id)| {
            (mapped_agent_id == agent_id).then_some(provider_id.as_str())
        })
        .or_else(|| provider.ids().first().map(String::as_str))
}

fn glob_match(pattern: &str, target: &str) -> bool {
    let pattern = pattern.as_bytes();
    let target = target.as_bytes();
    let mut pattern_index = 0;
    let mut target_index = 0;
    let mut star_pattern_index = None;
    let mut star_target_index = 0;

    while target_index < target.len() {
        if pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
            star_pattern_index = Some(pattern_index);
            star_target_index = target_index;
            pattern_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == target[target_index] {
            pattern_index += 1;
            target_index += 1;
        } else if let Some(star_index) = star_pattern_index {
            pattern_index = star_index + 1;
            star_target_index += 1;
            target_index = star_target_index;
        } else {
            return false;
        }
    }
    while pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

#[derive(Debug)]
struct GithubInstallCheck<'a> {
    label: String,
    repo: String,
    install: &'a GithubInstall,
}

struct Http {
    client: reqwest::blocking::Client,
    github_token: Option<String>,
}

impl Http {
    fn new() -> Result<Self, Box<dyn Error>> {
        let client = reqwest::blocking::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .user_agent(USER_AGENT)
            .build()?;
        let github_token = std::env::var("GITHUB_TOKEN")
            .ok()
            .filter(|token| !token.trim().is_empty());
        Ok(Self {
            client,
            github_token,
        })
    }

    fn get_json<T>(&self, url: &str) -> Result<T, Box<dyn Error>>
    where
        T: for<'de> Deserialize<'de>,
    {
        self.client
            .get(url)
            .header("Accept", "application/json")
            .send()
            .map_err(|source| format!("failed to GET {url}: {source}"))?
            .error_for_status()
            .map_err(|source| format!("GET {url} returned an error status: {source}"))?
            .json()
            .map_err(|source| format!("failed to decode JSON from {url}: {source}").into())
    }

    fn get_text(&self, url: &str) -> Result<String, Box<dyn Error>> {
        Self::text_response(self.client.get(url), url)
    }

    /// A file on a repository's default branch, read through the contents API, whose rate limit
    /// follows the token; raw.githubusercontent.com throttles per-file source checks even when
    /// authenticated.
    fn github_raw_text(&self, repo: &str, path: &str) -> Result<String, Box<dyn Error>> {
        let url = format!("{GITHUB_API_BASE}/repos/{repo}/contents/{path}");
        let mut request = self
            .client
            .get(&url)
            .header("Accept", GITHUB_RAW_MEDIA_TYPE);
        if let Some(token) = &self.github_token {
            request = request.bearer_auth(token);
        }
        Self::text_response(request, &url)
    }

    fn text_response(
        request: reqwest::blocking::RequestBuilder,
        url: &str,
    ) -> Result<String, Box<dyn Error>> {
        request
            .send()
            .map_err(|source| format!("failed to GET {url}: {source}"))?
            .error_for_status()
            .map_err(|source| format!("GET {url} returned an error status: {source}"))?
            .text()
            .map_err(|source| format!("failed to decode text from {url}: {source}").into())
    }

    fn github_json<T>(&self, path: &str) -> Result<T, Box<dyn Error>>
    where
        T: for<'de> Deserialize<'de>,
    {
        let url = format!("{GITHUB_API_BASE}{path}");
        let mut request = self
            .client
            .get(&url)
            .header("Accept", "application/vnd.github+json");
        if let Some(token) = &self.github_token {
            request = request.bearer_auth(token);
        }
        request
            .send()
            .map_err(|source| format!("failed to GET {url}: {source}"))?
            .error_for_status()
            .map_err(|source| format!("GET {url} returned an error status: {source}"))?
            .json()
            .map_err(|source| format!("failed to decode JSON from {url}: {source}").into())
    }
}

#[derive(Default)]
struct Report {
    ok: Vec<String>,
    info: Vec<String>,
    manual: Vec<String>,
    failures: Vec<String>,
}

impl Report {
    fn ok(&mut self, message: impl Into<String>) {
        self.ok.push(message.into());
    }

    fn info(&mut self, message: impl Into<String>) {
        self.info.push(message.into());
    }

    fn manual(&mut self, message: impl Into<String>) {
        self.manual.push(message.into());
    }

    fn fail(&mut self, message: impl Into<String>) {
        self.failures.push(message.into());
    }

    fn has_failures(&self) -> bool {
        !self.failures.is_empty()
    }

    fn print(&self) {
        println!("acp-stack shipped data fact-check");
        println!("=================================");
        print_section("fail", &self.failures);
        print_section("manual", &self.manual);
        print_section("info", &self.info);
        print_section("ok", &self.ok);
        println!();
        println!(
            "summary: {} ok, {} manual, {} info, {} fail",
            self.ok.len(),
            self.manual.len(),
            self.info.len(),
            self.failures.len()
        );
    }
}

fn print_section(name: &str, messages: &[String]) {
    if messages.is_empty() {
        return;
    }
    println!();
    println!("[{name}]");
    for message in messages {
        println!("  - {message}");
    }
}

#[derive(Debug, Deserialize)]
struct AcpRegistry {
    #[serde(default)]
    agents: Vec<AcpAgent>,
}

#[derive(Debug, Deserialize)]
struct AcpAgent {
    id: String,
}

#[derive(Debug, Deserialize)]
struct NpmLatest {
    version: String,
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    #[serde(default)]
    assets: Vec<GithubAsset>,
}

#[derive(Debug, Deserialize)]
struct GithubAsset {
    name: String,
    #[serde(default)]
    browser_download_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GithubCommit {
    sha: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn glob_match_supports_release_asset_patterns() {
        assert!(glob_match(
            "codex-acp-*-x86_64-unknown-linux-gnu.tar.gz",
            "codex-acp-v0.16.0-x86_64-unknown-linux-gnu.tar.gz"
        ));
        assert!(!glob_match(
            "codex-acp-*-aarch64-unknown-linux-gnu.tar.gz",
            "codex-acp-v0.16.0-x86_64-unknown-linux-gnu.tar.gz"
        ));
    }

    #[test]
    fn models_dev_provider_names_accepts_top_level_provider_map() {
        let providers = models_dev_provider_names(&json!({
            "deepinfra": { "name": "Deep Infra" },
            "venice": { "title": "Venice AI" },
        }));

        assert_eq!(providers.get("deepinfra"), Some(&"Deep Infra".to_owned()));
        assert_eq!(providers.get("venice"), Some(&"Venice AI".to_owned()));
    }

    #[test]
    fn models_dev_provider_names_accepts_nested_provider_map() {
        let providers = models_dev_provider_names(&json!({
            "providers": {
                "github-models": { "displayName": "GitHub Models" }
            }
        }));

        assert_eq!(
            providers.get("github-models"),
            Some(&"GitHub Models".to_owned())
        );
    }

    #[test]
    fn models_dev_provider_env_vars_reads_env_arrays() {
        let env_vars = models_dev_provider_env_vars(&json!({
            "openrouter": { "name": "OpenRouter", "env": ["OPENROUTER_API_KEY"] },
            "lmstudio": { "name": "LMStudio" }
        }));

        assert_eq!(env_vars["openrouter"], ["OPENROUTER_API_KEY"]);
        assert!(env_vars["lmstudio"].is_empty());
    }

    #[test]
    fn opencode_models_dev_ids_fail_for_an_unlisted_native_id() {
        let mapping = ProviderKeyMapping::from_toml(
            r#"
[[api_keys]]
env_var = "LISTED_API_KEY"
provider_ids = ["listed"]

[[api_keys]]
env_var = "RENAMED_API_KEY"
provider_ids = ["renamed"]

[[providers]]
id = ["listed"]
name = "Listed"
agents = ["pi", "opencode"]

[[providers]]
id = ["renamed", "renamed-native"]
name = "Renamed"
agents = ["pi", "opencode"]

[providers.provider_ids]
renamed-native = "opencode"
"#,
        )
        .expect("mapping");
        let models_dev = models_dev_provider_env_vars(&json!({
            "listed": { "name": "Listed" },
            "renamed": { "name": "Renamed" }
        }));
        let mut models_dev_tier = BTreeSet::new();
        let mut report = Report::default();

        check_opencode_models_dev_ids(&mapping, &models_dev, &mut models_dev_tier, &mut report);

        assert_eq!(report.failures.len(), 1);
        assert!(report.failures[0].contains("`renamed-native`"));
        assert!(report.ok.is_empty(), "no summary pass beside a failure");
        assert_eq!(models_dev_tier, BTreeSet::from(["listed".to_owned()]));
    }

    #[test]
    fn pi_provider_env_vars_reads_env_map_and_early_returns() {
        let source = r#"
export const ANTHROPIC_AUTH_TOKEN_ENV = "ANTHROPIC_AUTH_TOKEN";
export const ANTHROPIC_API_KEY_ENV = "ANTHROPIC_API_KEY";
function getApiKeyEnvVars(provider: string): readonly string[] | undefined {
	if (provider === "github-copilot") {
		return ["COPILOT_GITHUB_TOKEN"];
	}
	if (provider === "anthropic") {
		return [ANTHROPIC_AUTH_TOKEN_ENV, ANTHROPIC_API_KEY_ENV];
	}
	const envMap: Record<string, string> = {
		"ant-ling": "ANT_LING_API_KEY",
		azure: "AZURE_OPENAI_API_KEY",
	};
}
	if (provider === "google-vertex") {
		const hasCredentials = hasVertexAdcCredentials(env);
		return "<authenticated>";
	}
"#;

        let env_vars = pi_provider_env_vars(source);

        let expected = |values: &[&str]| -> BTreeSet<String> {
            values.iter().map(|value| (*value).to_owned()).collect()
        };
        assert_eq!(
            env_vars["github-copilot"],
            expected(&["COPILOT_GITHUB_TOKEN"])
        );
        assert_eq!(
            env_vars["anthropic"],
            expected(&["ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_API_KEY"])
        );
        assert_eq!(env_vars["ant-ling"], expected(&["ANT_LING_API_KEY"]));
        assert_eq!(env_vars["azure"], expected(&["AZURE_OPENAI_API_KEY"]));
        assert_eq!(env_vars.len(), 4);
    }

    #[test]
    fn embedded_sync_ids_use_adapter_ids() {
        let catalog = RegistryCatalog::from_toml(AGENTS_TOML).expect("agents");
        let ids = embedded_sync_ids(&catalog);

        assert!(ids.contains("amp-acp"));
        assert!(ids.contains("codex-acp"));
        assert!(ids.contains("claude-acp"));
        assert!(!ids.contains("amp"));
        assert!(!ids.contains("claude-agent-acp"));
        assert!(!ids.contains("pi-acp"));
        assert!(!ids.contains("hermes-agent-acp"));
    }

    #[test]
    fn models_dev_scope_finds_opencode_only_providers_without_default_key() {
        let mapping =
            ProviderKeyMapping::from_toml_parts(ENV_VARS_TOML, PROVIDERS_TOML).expect("mapping");
        let providers: BTreeSet<_> = models_dev_scoped_providers(&mapping)
            .into_iter()
            .flat_map(|provider| provider.ids().iter().cloned())
            .collect();

        assert!(providers.contains("deepinfra"));
        assert!(providers.contains("gitlab"));
        assert!(!providers.contains("openai"));
        assert!(!providers.contains("opencode"));
    }
}
