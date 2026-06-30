//! Dev tool: refresh or check the generated Agent Skills catalog index.
//!
//! Usage:
//!
//! ```sh
//! cargo run --features dev-tools --bin sync-skills-catalog -- --check
//! cargo run --features dev-tools --bin sync-skills-catalog -- --write
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;
use std::process::ExitCode;
use std::time::Duration;

use acp_stack::runtime::install::skill_registry::{SkillCatalog, SkillSource};
use reqwest::blocking::Client;
use serde::Deserialize;

const SKILLS_TOML_PATH: &str = "data/skills.toml";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const CURL_TIMEOUT_SECONDS: &str = "60";
const CURL_RETRY_COUNT: &str = "3";
const CURL_RETRY_DELAY_SECONDS: &str = "2";
const CURL_JSON_ATTEMPTS: usize = 3;
const SKILL_DESCRIPTOR: &str = "SKILL.md";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("Error: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mode = Mode::from_args(std::env::args().skip(1))?;
    let body = std::fs::read_to_string(SKILLS_TOML_PATH)?;
    let catalog = SkillCatalog::from_toml(&body)?;
    let mut sources = catalog.sources().to_vec();

    if mode == Mode::Write {
        let http = GithubClient::new()?;
        refresh_sources(&http, mode, &mut sources)?;
    }

    let rendered = render_catalog(&sources);
    SkillCatalog::from_toml(&rendered)?;
    if rendered == body {
        println!("skills catalog is current");
        return Ok(());
    }

    match mode {
        Mode::Check => Err("data/skills.toml is stale; run sync-skills-catalog -- --write".into()),
        Mode::Write => {
            std::fs::write(SKILLS_TOML_PATH, rendered)?;
            Err("data/skills.toml was updated; stage it and rerun checks".into())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Check,
    Write,
}

impl Mode {
    fn from_args(
        args: impl IntoIterator<Item = String>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let mut mode = None;
        for arg in args {
            match arg.as_str() {
                "--check" => set_mode(&mut mode, Mode::Check)?,
                "--write" => set_mode(&mut mode, Mode::Write)?,
                _ => return Err(format!("unknown argument `{arg}`").into()),
            }
        }
        Ok(mode.unwrap_or(Mode::Check))
    }
}

fn set_mode(slot: &mut Option<Mode>, next: Mode) -> Result<(), Box<dyn std::error::Error>> {
    if slot.replace(next).is_some() {
        return Err("pass only one of --check or --write".into());
    }
    Ok(())
}

fn refresh_sources(
    http: &impl GithubApi,
    mode: Mode,
    sources: &mut [SkillSource],
) -> Result<(), Box<dyn std::error::Error>> {
    if mode == Mode::Check {
        return Ok(());
    }
    for source in sources {
        refresh_source(http, source)?;
    }
    Ok(())
}

fn refresh_source(
    http: &impl GithubApi,
    source: &mut SkillSource,
) -> Result<(), Box<dyn std::error::Error>> {
    let repo = format!("{}/{}", source.owner, source.repo);
    let reference = if let Some(commit) = source.verified_commit.as_deref() {
        source.indexed_commit = None;
        commit.to_owned()
    } else {
        let commit = http.commit(&repo, &source.branch)?;
        source.indexed_commit = Some(commit.sha.clone());
        commit.sha
    };
    let tree = http.tree(&repo, &reference)?;
    if tree.truncated {
        return Err(format!("GitHub tree for `{repo}` at `{reference}` was truncated").into());
    }

    for directory in &mut source.directories {
        directory.indexed_names = if directory.installable {
            discover_skill_names(&tree, &directory.path)
        } else {
            Vec::new()
        };
    }

    for plugin_bundle in &mut source.plugin_bundles {
        let snapshot = discover_plugins(&tree, &plugin_bundle.path)?;
        plugin_bundle.installable_plugins = snapshot.installable_plugins;
        plugin_bundle.excluded_plugins = snapshot.excluded_plugins;
    }
    Ok(())
}

fn discover_skill_names(tree: &GithubTree, base_path: &str) -> Vec<String> {
    let prefix = format!("{}/", base_path.trim_matches('/'));
    let suffix = format!("/{SKILL_DESCRIPTOR}");
    let mut names = BTreeSet::new();
    for item in &tree.tree {
        if item.kind != "blob" || !is_regular_file_mode(&item.mode) {
            continue;
        }
        let Some(rest) = item.path.strip_prefix(&prefix) else {
            continue;
        };
        let Some(name) = rest.strip_suffix(&suffix) else {
            continue;
        };
        if name.contains('/') || !is_catalog_name(name) {
            continue;
        }
        names.insert(name.to_owned());
    }
    names.into_iter().collect()
}

fn discover_plugins(
    tree: &GithubTree,
    base_path: &str,
) -> Result<PluginSnapshot, Box<dyn std::error::Error>> {
    let base = base_path.trim_matches('/');
    let plugin_prefix = format!("{base}/");
    let mut plugins = BTreeSet::new();
    let mut skill_counts: BTreeMap<String, usize> = BTreeMap::new();

    for item in &tree.tree {
        if item.kind == "tree"
            && let Some(rest) = item.path.strip_prefix(&plugin_prefix)
            && !rest.contains('/')
        {
            if !is_catalog_name(rest) {
                return Err(format!("upstream plugin name `{rest}` is not dash-case").into());
            }
            plugins.insert(rest.to_owned());
        }
        if item.kind != "blob" || !is_regular_file_mode(&item.mode) {
            continue;
        }
        let Some(rest) = item.path.strip_prefix(&plugin_prefix) else {
            continue;
        };
        let parts = rest.split('/').collect::<Vec<_>>();
        if parts.len() != 4 || parts[1] != "skills" || parts[3] != SKILL_DESCRIPTOR {
            continue;
        }
        let plugin_name = parts[0];
        let skill_name = parts[2];
        if !is_catalog_name(plugin_name) || !is_catalog_name(skill_name) {
            continue;
        }
        plugins.insert(plugin_name.to_owned());
        *skill_counts.entry(plugin_name.to_owned()).or_default() += 1;
    }

    let mut installable_plugins = Vec::new();
    let mut excluded_plugins = Vec::new();
    for plugin in plugins {
        if skill_counts.get(&plugin).copied().unwrap_or_default() > 0 {
            installable_plugins.push(plugin);
        } else {
            excluded_plugins.push(plugin);
        }
    }
    Ok(PluginSnapshot {
        installable_plugins,
        excluded_plugins,
    })
}

fn is_regular_file_mode(mode: &str) -> bool {
    matches!(mode, "100644" | "100755")
}

fn is_catalog_name(name: &str) -> bool {
    !name.is_empty()
        && name.split('-').all(|segment| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
}

struct PluginSnapshot {
    installable_plugins: Vec<String>,
    excluded_plugins: Vec<String>,
}

struct GithubClient {
    client: Client,
}

trait GithubApi {
    fn commit(&self, repo: &str, branch: &str) -> Result<GithubCommit, Box<dyn std::error::Error>>;

    fn tree(&self, repo: &str, commit: &str) -> Result<GithubTree, Box<dyn std::error::Error>>;
}

impl GithubClient {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let client = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .user_agent(concat!("acp-stack-skills-sync/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self { client })
    }
}

impl GithubApi for GithubClient {
    fn commit(&self, repo: &str, branch: &str) -> Result<GithubCommit, Box<dyn std::error::Error>> {
        self.github_json(&format!(
            "https://api.github.com/repos/{repo}/commits/{}",
            branch.trim_matches('/')
        ))
    }

    fn tree(&self, repo: &str, commit: &str) -> Result<GithubTree, Box<dyn std::error::Error>> {
        self.github_json(&format!(
            "https://api.github.com/repos/{repo}/git/trees/{commit}?recursive=1"
        ))
    }
}

impl GithubClient {
    fn github_json<T: for<'de> Deserialize<'de>>(
        &self,
        url: &str,
    ) -> Result<T, Box<dyn std::error::Error>> {
        let reqwest_result = self
            .client
            .get(url)
            .header(reqwest::header::ACCEPT_ENCODING, "identity")
            .send()
            .and_then(|response| response.error_for_status())
            .and_then(|response| response.json());
        match reqwest_result {
            Ok(parsed) => Ok(parsed),
            Err(reqwest_error) => {
                // Keep the dev hook usable in environments where rustls-backed
                // reqwest cannot fetch or decode GitHub responses but curl can.
                curl_json(url).map_err(|curl_error| {
                    format!("request failed with reqwest ({reqwest_error}) and curl ({curl_error})")
                        .into()
                })
            }
        }
    }
}

fn curl_json<T: for<'de> Deserialize<'de>>(url: &str) -> Result<T, Box<dyn std::error::Error>> {
    let mut last_error = None;
    for _ in 0..CURL_JSON_ATTEMPTS {
        let output = Command::new("curl")
            .args([
                "-fsSL",
                "--http1.1",
                "--max-time",
                CURL_TIMEOUT_SECONDS,
                "--retry",
                CURL_RETRY_COUNT,
                "--retry-all-errors",
                "--retry-delay",
                CURL_RETRY_DELAY_SECONDS,
                "-H",
                "Accept-Encoding: identity",
                "-H",
                concat!(
                    "User-Agent: acp-stack-skills-sync/",
                    env!("CARGO_PKG_VERSION")
                ),
                url,
            ])
            .output()?;
        if !output.status.success() {
            last_error = Some(format!(
                "curl exited with status {}",
                output
                    .status
                    .code()
                    .map_or_else(|| "unknown".to_owned(), |code| code.to_string())
            ));
            continue;
        }
        match serde_json::from_slice(&output.stdout) {
            Ok(parsed) => return Ok(parsed),
            Err(source) => last_error = Some(source.to_string()),
        }
    }
    Err(format!(
        "curl response was not valid JSON after {CURL_JSON_ATTEMPTS} attempts: {}",
        last_error.unwrap_or_else(|| "unknown error".to_owned())
    )
    .into())
}

#[derive(Debug, Clone, Deserialize)]
struct GithubCommit {
    sha: String,
}

#[derive(Debug, Clone, Deserialize)]
struct GithubTree {
    tree: Vec<GithubTreeItem>,
    #[serde(default)]
    truncated: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct GithubTreeItem {
    path: String,
    mode: String,
    #[serde(rename = "type")]
    kind: String,
}

fn render_catalog(sources: &[SkillSource]) -> String {
    let mut output = String::from(
        "# Official Agent Skills Catalog\n\
         # Generated indexes are maintained by `sync-skills-catalog`.\n",
    );
    for source in sources {
        output.push_str("\n[[sources]]\n");
        push_field(&mut output, "id", &source.id);
        push_field(&mut output, "name", &source.name);
        push_field(&mut output, "owner", &source.owner);
        push_field(&mut output, "repo", &source.repo);
        push_field(&mut output, "url", &source.url);
        push_array(&mut output, "docs", &source.docs);
        output.push_str(&format!("official = {}\n", source.official));
        output.push_str(&format!("trusted = {}\n", source.trusted));
        push_field(&mut output, "reviewed_at", &source.reviewed_at);
        push_field(&mut output, "branch", &source.branch);
        if let Some(commit) = source.verified_commit.as_deref() {
            push_field(&mut output, "verified_commit", commit);
        }
        if let Some(commit) = source.indexed_commit.as_deref() {
            push_field(&mut output, "indexed_commit", commit);
        }
        push_field(&mut output, "descriptor", &source.descriptor);

        for directory in &source.directories {
            output.push_str("\n[[sources.directories]]\n");
            push_field(&mut output, "path", &directory.path);
            push_field(&mut output, "source_url", &directory.source_url);
            output.push_str(&format!("verified = {}\n", directory.verified));
            output.push_str(&format!("installable = {}\n", directory.installable));
            push_array(&mut output, "indexed_names", &directory.indexed_names);
            push_array_if_not_empty(&mut output, "essential_names", &directory.essential_names);
        }

        for plugin_bundle in &source.plugin_bundles {
            output.push_str("\n[[sources.plugin_bundles]]\n");
            push_field(&mut output, "path", &plugin_bundle.path);
            push_field(&mut output, "source_url", &plugin_bundle.source_url);
            output.push_str(&format!("verified = {}\n", plugin_bundle.verified));
            push_array(
                &mut output,
                "installable_plugins",
                &plugin_bundle.installable_plugins,
            );
            push_array_if_not_empty(
                &mut output,
                "essential_plugins",
                &plugin_bundle.essential_plugins,
            );
            push_array(
                &mut output,
                "excluded_plugins",
                &plugin_bundle.excluded_plugins,
            );
        }
    }
    output
}

fn push_field(output: &mut String, key: &str, value: &str) {
    output.push_str(&format!("{key} = \"{}\"\n", toml_escape(value)));
}

fn push_array(output: &mut String, key: &str, values: &[String]) {
    if values.is_empty() {
        output.push_str(&format!("{key} = []\n"));
        return;
    }
    output.push_str(&format!("{key} = [\n"));
    for value in values {
        output.push_str(&format!("  \"{}\",\n", toml_escape(value)));
    }
    output.push_str("]\n");
}

fn push_array_if_not_empty(output: &mut String, key: &str, values: &[String]) {
    if !values.is_empty() {
        push_array(output, key, values);
    }
}

fn toml_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    use acp_stack::runtime::install::skill_registry::PluginBundleDirectory;

    const OLD_COMMIT: &str = "1111111111111111111111111111111111111111";
    const NEW_COMMIT: &str = "2222222222222222222222222222222222222222";
    const VERIFIED_COMMIT: &str = "3333333333333333333333333333333333333333";

    #[test]
    fn mode_defaults_to_check() {
        assert_eq!(
            Mode::from_args(Vec::<String>::new()).expect("mode"),
            Mode::Check
        );
    }

    #[test]
    fn discovers_plugins_with_and_without_skills() {
        let tree = GithubTree {
            tree: vec![
                tree_dir("plugins/cloudflare"),
                tree_blob("plugins/cloudflare/skills/wrangler/SKILL.md"),
                tree_dir("plugins/no-skills"),
            ],
            truncated: false,
        };

        let snapshot = discover_plugins(&tree, "plugins").expect("snapshot");

        assert_eq!(snapshot.installable_plugins, vec!["cloudflare"]);
        assert_eq!(snapshot.excluded_plugins, vec!["no-skills"]);
    }

    #[test]
    fn check_mode_does_not_refresh_sources() {
        let http = FakeGithubApi::new(NEW_COMMIT, plugin_tree());
        let mut source = plugin_source(Some(OLD_COMMIT), None);

        refresh_sources(&http, Mode::Check, std::slice::from_mut(&mut source))
            .expect("refresh sources");

        assert!(http.commit_calls.borrow().is_empty());
        assert!(http.tree_calls.borrow().is_empty());
        assert_eq!(source.indexed_commit.as_deref(), Some(OLD_COMMIT));
        assert!(source.plugin_bundles[0].installable_plugins.is_empty());
    }

    #[test]
    fn write_mode_advances_indexed_commit_to_branch_head() {
        let http = FakeGithubApi::new(NEW_COMMIT, plugin_tree());
        let mut source = plugin_source(Some(OLD_COMMIT), None);

        refresh_sources(&http, Mode::Write, std::slice::from_mut(&mut source))
            .expect("refresh sources");

        assert_eq!(
            http.commit_calls.borrow().as_slice(),
            &[("openai/plugins".to_owned(), "main".to_owned())]
        );
        assert_eq!(
            http.tree_calls.borrow().as_slice(),
            &[("openai/plugins".to_owned(), NEW_COMMIT.to_owned())]
        );
        assert_eq!(source.indexed_commit.as_deref(), Some(NEW_COMMIT));
    }

    #[test]
    fn verified_commit_clears_indexed_commit_and_uses_verified_reference_on_write() {
        let http = FakeGithubApi::new(NEW_COMMIT, plugin_tree());
        let mut source = plugin_source(Some(OLD_COMMIT), Some(VERIFIED_COMMIT));

        refresh_sources(&http, Mode::Write, std::slice::from_mut(&mut source))
            .expect("refresh sources");

        assert!(http.commit_calls.borrow().is_empty());
        assert_eq!(
            http.tree_calls.borrow().as_slice(),
            &[("openai/plugins".to_owned(), VERIFIED_COMMIT.to_owned())]
        );
        assert_eq!(source.indexed_commit, None);
    }

    #[test]
    fn discovers_direct_skill_names() {
        let tree = GithubTree {
            tree: vec![
                tree_blob("skills/.curated/repo-map/SKILL.md"),
                tree_blob("skills/.curated/bad_name/SKILL.md"),
                tree_blob("skills/.system/internal/SKILL.md"),
            ],
            truncated: false,
        };

        assert_eq!(
            discover_skill_names(&tree, "skills/.curated"),
            vec!["repo-map"]
        );
    }

    #[test]
    fn rendered_catalog_preserves_essential_fields() {
        let body = r#"
[[sources]]
id = "openai-plugins"
name = "OpenAI Plugin Skills"
owner = "openai"
repo = "plugins"
url = "https://github.com/openai/plugins"
docs = ["https://github.com/openai/plugins"]
official = true
trusted = true
reviewed_at = "2026-06-23"
branch = "main"
descriptor = "SKILL.md"

[[sources.plugin_bundles]]
path = "plugins"
source_url = "https://github.com/openai/plugins/tree/main/plugins"
verified = true
installable_plugins = ["github"]
essential_plugins = ["github"]
excluded_plugins = []
"#;
        let catalog = SkillCatalog::from_toml(body).expect("catalog");

        let rendered = render_catalog(catalog.sources());

        assert!(rendered.contains("essential_plugins = ["));
        assert!(rendered.contains("  \"github\","));
    }

    #[test]
    fn github_tree_records_truncated_flag() {
        let parsed: GithubTree =
            serde_json::from_str(r#"{"tree":[],"truncated":true}"#).expect("tree");
        assert!(parsed.truncated);
    }

    fn tree_dir(path: &str) -> GithubTreeItem {
        GithubTreeItem {
            path: path.to_owned(),
            mode: "040000".to_owned(),
            kind: "tree".to_owned(),
        }
    }

    fn tree_blob(path: &str) -> GithubTreeItem {
        GithubTreeItem {
            path: path.to_owned(),
            mode: "100644".to_owned(),
            kind: "blob".to_owned(),
        }
    }

    fn plugin_tree() -> GithubTree {
        GithubTree {
            tree: vec![
                tree_dir("plugins/cloudflare"),
                tree_blob("plugins/cloudflare/skills/wrangler/SKILL.md"),
            ],
            truncated: false,
        }
    }

    fn plugin_source(indexed_commit: Option<&str>, verified_commit: Option<&str>) -> SkillSource {
        SkillSource {
            id: "openai-plugins".to_owned(),
            name: "OpenAI Plugin Skills".to_owned(),
            owner: "openai".to_owned(),
            repo: "plugins".to_owned(),
            url: "https://github.com/openai/plugins".to_owned(),
            docs: vec!["https://github.com/openai/plugins".to_owned()],
            official: true,
            trusted: true,
            reviewed_at: "2026-06-23".to_owned(),
            branch: "main".to_owned(),
            verified_commit: verified_commit.map(str::to_owned),
            indexed_commit: indexed_commit.map(str::to_owned),
            descriptor: SKILL_DESCRIPTOR.to_owned(),
            directories: Vec::new(),
            plugin_bundles: vec![PluginBundleDirectory {
                path: "plugins".to_owned(),
                source_url: "https://github.com/openai/plugins/tree/main/plugins".to_owned(),
                verified: true,
                installable_plugins: Vec::new(),
                essential_plugins: Vec::new(),
                excluded_plugins: Vec::new(),
            }],
        }
    }

    struct FakeGithubApi {
        commit_sha: String,
        tree: GithubTree,
        commit_calls: RefCell<Vec<(String, String)>>,
        tree_calls: RefCell<Vec<(String, String)>>,
    }

    impl FakeGithubApi {
        fn new(commit_sha: &str, tree: GithubTree) -> Self {
            Self {
                commit_sha: commit_sha.to_owned(),
                tree,
                commit_calls: RefCell::new(Vec::new()),
                tree_calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl GithubApi for FakeGithubApi {
        fn commit(
            &self,
            repo: &str,
            branch: &str,
        ) -> Result<GithubCommit, Box<dyn std::error::Error>> {
            self.commit_calls
                .borrow_mut()
                .push((repo.to_owned(), branch.to_owned()));
            Ok(GithubCommit {
                sha: self.commit_sha.clone(),
            })
        }

        fn tree(&self, repo: &str, commit: &str) -> Result<GithubTree, Box<dyn std::error::Error>> {
            self.tree_calls
                .borrow_mut()
                .push((repo.to_owned(), commit.to_owned()));
            Ok(self.tree.clone())
        }
    }
}
