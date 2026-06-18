//! Dev tool: fact-check shipped data files against external metadata sources.
//!
//! Usage:
//!
//! ```sh
//! cargo run --features dev-tools --bin fact-check-data
//! ```

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

const AGENTS_TOML: &str = include_str!("../../data/agents.toml");
const ENV_VARS_TOML: &str = include_str!("../../data/env_vars.toml");
const PROVIDERS_TOML: &str = include_str!("../../data/providers.toml");
const SKILLS_TOML: &str = include_str!("../../data/skills.toml");

const ACP_REGISTRY_REPO: &str = "agentclientprotocol/registry";
const ACP_REGISTRY_ASSET: &str = "registry.json";
const MODELS_DEV_API_URL: &str = "https://models.dev/api.json";
const PI_PROVIDER_DOC_URL: &str = "https://raw.githubusercontent.com/earendil-works/pi/main/packages/coding-agent/docs/providers.md";
const AMP_MANUAL_URL: &str = "https://ampcode.com/manual";
const OPENCODE_PROVIDER_DOC_URL: &str = "https://opencode.ai/docs/providers/";
const GITHUB_API_BASE: &str = "https://api.github.com";
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

        let directory_ref = source.verified_commit.as_deref().unwrap_or(&source.branch);
        for directory in &source.directories {
            let path = directory.path.trim_matches('/');
            let _: Value = http.github_json(&format!(
                "/repos/{repo}/contents/{path}?ref={directory_ref}"
            ))?;
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
    let pi_doc = http.get_text(PI_PROVIDER_DOC_URL)?;
    let amp_manual = http.get_text(AMP_MANUAL_URL)?;
    let opencode_doc = http.get_text(OPENCODE_PROVIDER_DOC_URL)?;

    let mut primary_doc = BTreeSet::new();
    check_pi_provider_doc(mapping, &pi_doc, &mut primary_doc, report);
    check_agent_env_doc(
        "amp-code",
        "Amp",
        "AMP_API_KEY",
        &amp_manual,
        &mut primary_doc,
        report,
    );
    check_opencode_doc(&opencode_doc, &mut primary_doc, report);

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

    let all_provider_ids: BTreeSet<_> = mapping
        .providers()
        .iter()
        .flat_map(|provider| provider.ids().iter().cloned())
        .collect();
    let manual: Vec<_> = all_provider_ids
        .difference(&primary_doc)
        .filter(|id| !models_dev_tier.contains(*id))
        .cloned()
        .collect();

    report.info(format!(
        "provider confidence tiers: primary-doc={}, models-dev={}, manual={}",
        primary_doc.len(),
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

fn check_pi_provider_doc(
    mapping: &ProviderKeyMapping,
    pi_doc: &str,
    primary_doc: &mut BTreeSet<String>,
    report: &mut Report,
) {
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
        let matching_line = pi_doc
            .lines()
            .find(|line| line.contains(native_id) && line.contains(env_var));
        if matching_line.is_some() {
            for provider_id in provider.ids() {
                primary_doc.insert(provider_id.clone());
            }
            report.ok(format!(
                "Pi provider docs verify `{native_id}` uses `{env_var}`"
            ));
        } else {
            report.fail(format!(
                "Pi provider docs do not verify provider `{native_id}` with env ref `{env_var}`"
            ));
        }
    }
}

fn check_agent_env_doc(
    provider_id: &str,
    label: &str,
    env_var: &str,
    body: &str,
    primary_doc: &mut BTreeSet<String>,
    report: &mut Report,
) {
    if body.contains(env_var) {
        primary_doc.insert(provider_id.to_owned());
        report.ok(format!("{label} docs verify direct env ref `{env_var}`"));
    } else {
        report.fail(format!(
            "{label} docs do not verify direct env ref `{env_var}`"
        ));
    }
}

fn check_opencode_doc(body: &str, primary_doc: &mut BTreeSet<String>, report: &mut Report) {
    for (provider_id, label) in [("opencode", "OpenCode Zen"), ("opencode-go", "OpenCode Go")] {
        if body.contains(label) {
            primary_doc.insert(provider_id.to_owned());
            report.ok(format!("OpenCode docs verify `{label}`"));
        } else {
            report.fail(format!("OpenCode docs do not verify `{label}`"));
        }
    }
}

fn models_dev_scoped_providers(mapping: &ProviderKeyMapping) -> Vec<&ProviderEnvMapping> {
    mapping
        .providers()
        .iter()
        .filter(|provider| provider.agents.len() == 1 && provider.agents[0] == "opencode")
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
    patterns
}

fn models_dev_provider_names(value: &Value) -> BTreeMap<String, String> {
    let provider_object = value
        .get("providers")
        .and_then(Value::as_object)
        .or_else(|| value.as_object());
    let Some(provider_object) = provider_object else {
        return BTreeMap::new();
    };
    provider_object
        .iter()
        .filter_map(|(id, provider)| provider_display_name(provider).map(|name| (id.clone(), name)))
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
        self.client
            .get(url)
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
    fn embedded_sync_ids_use_adapter_ids() {
        let catalog = RegistryCatalog::from_toml(AGENTS_TOML).expect("agents");
        let ids = embedded_sync_ids(&catalog);

        assert!(ids.contains("amp-acp"));
        assert!(ids.contains("codex-acp"));
        assert!(ids.contains("claude-acp"));
        assert!(!ids.contains("amp"));
        assert!(!ids.contains("claude-agent-acp"));
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
        assert!(providers.contains("github-models"));
        assert!(!providers.contains("openai"));
        assert!(!providers.contains("opencode"));
    }
}
