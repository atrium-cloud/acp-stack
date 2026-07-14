//! Refresh or validate the checked-in Agent Skills catalog.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Duration;

use acp_stack::runtime::install::skill_registry::{
    CatalogSkill, SkillCatalog, SkillDiscovery, SkillSource,
};
use acp_stack::runtime::workspace_sources::safe_download::{DownloadOpts, download_to_file};
use acp_stack::runtime::workspace_sources::safe_extract::{ExtractOpts, extract_archive};
use reqwest::blocking::Client;
use serde::Deserialize;
use sha2::{Digest, Sha256};

const SKILLS_TOML_PATH: &str = "data/skills.toml";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const CURL_TIMEOUT_SECONDS: &str = "60";
const CURL_RETRY_COUNT: &str = "3";
const CURL_RETRY_DELAY_SECONDS: &str = "2";
const CURL_JSON_ATTEMPTS: usize = 3;
const GITHUB_ARCHIVE_MAX_BYTES: u64 = 200 * 1024 * 1024;
const SKILL_DESCRIPTOR: &str = "SKILL.md";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let options = Options::from_args(std::env::args().skip(1))?;
    let body = std::fs::read_to_string(SKILLS_TOML_PATH)?;
    let catalog = SkillCatalog::from_toml(&body)?;
    let mut sources = catalog.sources().to_vec();

    if options.mode == Mode::Write {
        let github = GithubClient::new()?;
        refresh_sources(&github, &mut sources)?;
    }

    let rendered = render_catalog(&sources);
    SkillCatalog::from_toml(&rendered)?;
    if rendered == body {
        println!("skills catalog is current");
        return Ok(());
    }

    match options.mode {
        Mode::Check => Err("data/skills.toml is stale; run sync-skills-catalog -- --write".into()),
        Mode::Write => {
            std::fs::write(SKILLS_TOML_PATH, rendered)?;
            Err("data/skills.toml was updated; stage it and rerun checks".into())
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Options {
    mode: Mode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Check,
    Write,
}

impl Options {
    fn from_args(
        args: impl IntoIterator<Item = String>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let mut mode = None;
        for argument in args {
            match argument.as_str() {
                "--check" | "--write" => {
                    let next = if argument == "--check" {
                        Mode::Check
                    } else {
                        Mode::Write
                    };
                    if mode.replace(next).is_some() {
                        return Err("pass only one of --check or --write".into());
                    }
                }
                _ => return Err(format!("unknown argument `{argument}`").into()),
            }
        }
        let mode = mode.unwrap_or(Mode::Check);
        Ok(Self { mode })
    }
}

fn refresh_sources(
    github: &GithubClient,
    sources: &mut [SkillSource],
) -> Result<(), Box<dyn std::error::Error>> {
    for source in sources {
        refresh_source(github, source)?;
    }
    Ok(())
}

fn refresh_source(
    github: &GithubClient,
    source: &mut SkillSource,
) -> Result<(), Box<dyn std::error::Error>> {
    let repo = format!("{}/{}", source.owner, source.repo);
    let reference = if let Some(commit) = source.verified_commit.as_deref() {
        source.indexed_commit = None;
        commit.to_owned()
    } else {
        let commit = github.commit(&repo, &source.branch)?;
        source.indexed_commit = Some(commit.sha.clone());
        commit.sha
    };

    let temporary = tempfile::tempdir()?;
    let archive = temporary.path().join("source.tar.gz");
    let extracted = temporary.path().join("extracted");
    let archive_url = format!(
        "https://codeload.github.com/{}/{}/tar.gz/{reference}",
        source.owner, source.repo
    );
    println!("refreshing {repo} at {reference}");
    download_archive(&archive_url, &archive)?;
    let report = extract_archive(&archive, &extracted, &ExtractOpts::default())?;
    let top_level = report.top_level_dir.ok_or_else(|| {
        format!("GitHub archive for `{repo}` did not contain one top-level directory")
    })?;
    let root = extracted.join(top_level);
    let indexed_skills = discover_source_skills(&root, source)?;
    report_index_changes(source, &indexed_skills);
    source.indexed_skills = indexed_skills;
    println!(
        "indexed {} skills from {} at {} ({} excluded)",
        source.indexed_skills.len(),
        repo,
        reference,
        source.excluded_skills.len()
    );
    Ok(())
}

fn report_index_changes(source: &SkillSource, indexed_skills: &[CatalogSkill]) {
    let previous = source
        .indexed_skills
        .iter()
        .map(|skill| skill.path.as_str())
        .collect::<BTreeSet<_>>();
    let refreshed = indexed_skills
        .iter()
        .map(|skill| skill.path.as_str())
        .collect::<BTreeSet<_>>();
    for path in refreshed.difference(&previous) {
        println!("review new skill candidate for {}: {path}", source.id);
    }
    for path in previous.difference(&refreshed) {
        println!("review removed skill path for {}: {path}", source.id);
    }
}

fn download_archive(url: &str, destination: &Path) -> Result<(), Box<dyn std::error::Error>> {
    // GitHub codeload has repeatedly stalled in the rustls-backed client used
    // by the development hook, while curl succeeds against the same endpoint.
    // Prefer the already-required curl path and retain the safe downloader as
    // a bounded fallback for environments without a working curl transport.
    let curl_error = match curl_archive(url, destination) {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };
    let options = DownloadOpts {
        max_bytes: GITHUB_ARCHIVE_MAX_BYTES,
        connect_timeout: Duration::from_secs(15),
        read_timeout: Duration::from_secs(60),
        ..DownloadOpts::default()
    };
    download_to_file(url, destination, &options)
        .map(|_| ())
        .map_err(|error| {
            format!("archive download failed with curl ({curl_error}) and reqwest ({error})").into()
        })
}

fn curl_archive(url: &str, destination: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let max_filesize = GITHUB_ARCHIVE_MAX_BYTES.to_string();
    let output = Command::new("curl")
        .args([
            "-fsSL",
            "--http1.1",
            "--max-time",
            CURL_TIMEOUT_SECONDS,
            "--max-filesize",
            max_filesize.as_str(),
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
            "-o",
        ])
        .arg(destination)
        .arg(url)
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "archive download failed for `{url}` with curl status {}: {}",
            output
                .status
                .code()
                .map_or_else(|| "unknown".to_owned(), |code| code.to_string()),
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    // curl only enforces `--max-filesize` when the response advertises a
    // length; codeload streams chunked, so this on-disk check is the bound
    // that actually holds.
    let bytes = std::fs::metadata(destination)?.len();
    if bytes > GITHUB_ARCHIVE_MAX_BYTES {
        return Err(format!(
            "archive download for `{url}` exceeded {GITHUB_ARCHIVE_MAX_BYTES} bytes"
        )
        .into());
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct CandidateSkill {
    name: String,
    path: String,
    digest: String,
}

fn discover_source_skills(
    repository_root: &Path,
    source: &SkillSource,
) -> Result<Vec<CatalogSkill>, Box<dyn std::error::Error>> {
    let mut candidates = BTreeMap::<String, CandidateSkill>::new();
    for directory in source
        .directories
        .iter()
        .filter(|directory| directory.installable)
    {
        let discovery_root = repository_root.join(&directory.path);
        match source.discovery {
            SkillDiscovery::Direct => {
                discover_direct_skills(repository_root, &discovery_root, &mut candidates)?
            }
            SkillDiscovery::Recursive => {
                discover_recursive_skills(repository_root, &discovery_root, &mut candidates)?
            }
        }
    }

    for excluded in &source.excluded_skills {
        if !candidates.contains_key(excluded) {
            return Err(format!(
                "skill source `{}` has stale excluded path `{excluded}`",
                source.id
            )
            .into());
        }
    }
    for excluded in &source.excluded_skills {
        candidates.remove(excluded);
    }

    let mut by_name = BTreeMap::<String, Vec<CandidateSkill>>::new();
    for candidate in candidates.into_values() {
        by_name
            .entry(candidate.name.clone())
            .or_default()
            .push(candidate);
    }

    let mut indexed = Vec::new();
    for (name, candidates) in by_name {
        let mut by_digest = BTreeMap::<String, Vec<CandidateSkill>>::new();
        for candidate in candidates {
            by_digest
                .entry(candidate.digest.clone())
                .or_default()
                .push(candidate);
        }
        let mut variants = Vec::new();
        for copies in by_digest.into_values() {
            variants.push(select_canonical_copy(source, &name, copies)?);
        }
        variants.sort_by(|left, right| left.path.cmp(&right.path));
        if variants.len() == 1 {
            let candidate = variants.remove(0);
            indexed.push(CatalogSkill {
                selector: normalized_install_name_selector(&name),
                name: candidate.name,
                path: candidate.path,
            });
        } else {
            for candidate in variants {
                indexed.push(CatalogSkill {
                    selector: contextual_selector(&candidate.path),
                    name: candidate.name,
                    path: candidate.path,
                });
            }
        }
    }
    disambiguate_contextual_selectors(&mut indexed)?;
    indexed.sort_by(|left, right| left.selector.cmp(&right.selector));
    Ok(indexed)
}

fn discover_direct_skills(
    repository_root: &Path,
    discovery_root: &Path,
    candidates: &mut BTreeMap<String, CandidateSkill>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut entries = read_directory_sorted(discovery_root)?;
    for entry in entries.drain(..) {
        let metadata = std::fs::symlink_metadata(&entry)?;
        if metadata.file_type().is_symlink() {
            return Err(format!("refusing symlink in skill source `{}`", entry.display()).into());
        }
        if !metadata.is_dir() || !entry.join(SKILL_DESCRIPTOR).exists() {
            continue;
        }
        insert_candidate(repository_root, &entry, candidates)?;
    }
    Ok(())
}

fn discover_recursive_skills(
    repository_root: &Path,
    discovery_root: &Path,
    candidates: &mut BTreeMap<String, CandidateSkill>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut pending = vec![discovery_root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in read_directory_sorted(&directory)? {
            let metadata = std::fs::symlink_metadata(&entry)?;
            if metadata.file_type().is_symlink() {
                return Err(
                    format!("refusing symlink in skill source `{}`", entry.display()).into(),
                );
            }
            if metadata.is_dir() {
                pending.push(entry);
                continue;
            }
            if !metadata.is_file()
                || entry.file_name().and_then(|name| name.to_str()) != Some(SKILL_DESCRIPTOR)
            {
                continue;
            }
            let Some(skill_directory) = entry.parent() else {
                continue;
            };
            let relative = skill_directory.strip_prefix(discovery_root)?;
            if !relative
                .components()
                .any(|component| matches!(component, Component::Normal(value) if value == "skills"))
            {
                continue;
            }
            insert_candidate(repository_root, skill_directory, candidates)?;
        }
    }
    Ok(())
}

fn insert_candidate(
    repository_root: &Path,
    skill_directory: &Path,
    candidates: &mut BTreeMap<String, CandidateSkill>,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = normalized_relative_path(repository_root, skill_directory)?;
    let descriptor = skill_directory.join(SKILL_DESCRIPTOR);
    let name = parse_skill_name(&descriptor)?;
    validate_install_name(&name)?;
    let digest = hash_skill_tree(skill_directory)?;
    let candidate = CandidateSkill {
        name,
        path: path.clone(),
        digest,
    };
    if candidates.insert(path.clone(), candidate).is_some() {
        return Err(format!("duplicate discovered skill path `{path}`").into());
    }
    Ok(())
}

fn parse_skill_name(descriptor: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let metadata = std::fs::symlink_metadata(descriptor)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "skill descriptor `{}` must be a regular file",
            descriptor.display()
        )
        .into());
    }
    let body = std::fs::read_to_string(descriptor)?;
    let mut lines = body.lines();
    if lines.next() != Some("---") {
        return Err(format!(
            "skill descriptor `{}` is missing YAML frontmatter",
            descriptor.display()
        )
        .into());
    }
    let mut yaml = String::new();
    let mut closed = false;
    for line in lines {
        if line == "---" {
            closed = true;
            break;
        }
        yaml.push_str(line);
        yaml.push('\n');
    }
    if !closed {
        return Err(format!(
            "skill descriptor `{}` has unterminated YAML frontmatter",
            descriptor.display()
        )
        .into());
    }
    let frontmatter: SkillFrontmatter = serde_norway::from_str(&yaml)?;
    Ok(frontmatter.name)
}

#[derive(Deserialize)]
struct SkillFrontmatter {
    name: String,
}

fn validate_install_name(name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let valid = !name.is_empty()
        && name.split('/').all(|segment| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b':'))
                && segment
                    .bytes()
                    .next()
                    .is_some_and(|byte| byte.is_ascii_alphanumeric())
                && segment
                    .bytes()
                    .next_back()
                    .is_some_and(|byte| byte.is_ascii_alphanumeric())
        });
    if valid {
        Ok(())
    } else {
        Err(format!("upstream skill frontmatter name `{name}` is not a safe install path").into())
    }
}

fn normalized_install_name_selector(name: &str) -> String {
    name.split('/')
        .map(normalize_selector_segment)
        .collect::<Vec<_>>()
        .join("/")
}

fn hash_skill_tree(root: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let mut entries = Vec::new();
    collect_tree_entries(root, root, &mut entries)?;
    entries.sort();
    let mut hasher = Sha256::new();
    for entry in entries {
        let relative = normalized_relative_path(root, &entry)?;
        let metadata = std::fs::symlink_metadata(&entry)?;
        if metadata.is_dir() {
            hasher.update(b"directory\0");
            hasher.update(relative.as_bytes());
            hasher.update(b"\0");
            continue;
        }
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(format!("skill tree contains unsafe entry `{}`", entry.display()).into());
        }
        hasher.update(b"file\0");
        hasher.update(relative.as_bytes());
        hasher.update(b"\0");
        let mut file = File::open(&entry)?;
        let mut buffer = [0_u8; 32 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        hasher.update(b"\0");
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn collect_tree_entries(
    root: &Path,
    directory: &Path,
    entries: &mut Vec<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    for entry in read_directory_sorted(directory)? {
        let metadata = std::fs::symlink_metadata(&entry)?;
        if metadata.file_type().is_symlink() {
            return Err(format!("skill tree contains symlink `{}`", entry.display()).into());
        }
        entry.strip_prefix(root)?;
        entries.push(entry.clone());
        if metadata.is_dir() {
            collect_tree_entries(root, &entry, entries)?;
        } else if !metadata.is_file() {
            return Err(format!("skill tree contains special entry `{}`", entry.display()).into());
        }
    }
    Ok(())
}

fn select_canonical_copy(
    source: &SkillSource,
    name: &str,
    mut copies: Vec<CandidateSkill>,
) -> Result<CandidateSkill, Box<dyn std::error::Error>> {
    if copies.len() == 1 {
        return Ok(copies.remove(0));
    }
    for preferred in &source.preferred_paths {
        let mut matching = copies
            .iter()
            .filter(|candidate| path_is_within(&candidate.path, preferred));
        let Some(first) = matching.next() else {
            continue;
        };
        if matching.next().is_some() {
            return Err(format!(
                "skill source `{}` has multiple identical `{name}` copies under preferred path `{preferred}`",
                source.id
            )
            .into());
        }
        return Ok(first.clone());
    }
    let paths = copies
        .iter()
        .map(|candidate| candidate.path.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!(
        "skill source `{}` has identical `{name}` copies without a unique preferred path: {paths}",
        source.id
    )
    .into())
}

fn contextual_selector(path: &str) -> String {
    let components = path.split('/').collect::<Vec<_>>();
    let skills_index = components
        .iter()
        .rposition(|component| *component == "skills");
    let selected = match skills_index {
        Some(index) if index > 0 => components[index - 1..]
            .iter()
            .filter(|component| **component != "skills")
            .copied()
            .collect::<Vec<_>>(),
        _ => components
            .iter()
            .filter(|component| **component != "skills")
            .copied()
            .collect::<Vec<_>>(),
    };
    selected
        .into_iter()
        .map(normalize_selector_segment)
        .collect::<Vec<_>>()
        .join("/")
}

fn full_path_selector(path: &str) -> String {
    path.split('/')
        .filter(|component| *component != "skills")
        .map(normalize_selector_segment)
        .collect::<Vec<_>>()
        .join("/")
}

fn normalize_selector_segment(segment: &str) -> String {
    let mut normalized = String::new();
    let mut separator_pending = false;
    for character in segment.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_lowercase() || character.is_ascii_digit() {
            if separator_pending && !normalized.is_empty() {
                normalized.push('-');
            }
            normalized.push(character);
            separator_pending = false;
        } else {
            separator_pending = true;
        }
    }
    normalized.trim_matches('-').to_owned()
}

fn disambiguate_contextual_selectors(
    indexed: &mut [CatalogSkill],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut counts = BTreeMap::<String, usize>::new();
    for skill in indexed.iter() {
        *counts.entry(skill.selector.clone()).or_default() += 1;
    }
    for skill in indexed.iter_mut() {
        if counts.get(&skill.selector).copied().unwrap_or_default() > 1 {
            skill.selector = full_path_selector(&skill.path);
        }
    }
    let mut seen = BTreeSet::new();
    for skill in indexed.iter() {
        if skill.selector.is_empty() || !seen.insert(skill.selector.as_str()) {
            return Err(format!(
                "could not derive a unique selector for indexed path `{}`",
                skill.path
            )
            .into());
        }
    }
    Ok(())
}

fn read_directory_sorted(directory: &Path) -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    let mut entries = std::fs::read_dir(directory)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort();
    Ok(entries)
}

fn normalized_relative_path(
    root: &Path,
    path: &Path,
) -> Result<String, Box<dyn std::error::Error>> {
    let relative = path.strip_prefix(root)?;
    let mut components = Vec::new();
    for component in relative.components() {
        let Component::Normal(value) = component else {
            return Err(format!("unsafe relative source path `{}`", relative.display()).into());
        };
        let value = value
            .to_str()
            .ok_or_else(|| format!("non-UTF-8 source path `{}`", relative.display()))?;
        components.push(value);
    }
    Ok(components.join("/"))
}

fn path_is_within(path: &str, root: &str) -> bool {
    path == root || path.starts_with(&format!("{root}/"))
}

struct GithubClient {
    client: Client,
}

impl GithubClient {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let client = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .user_agent(concat!("acp-stack-skills-sync/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self { client })
    }

    fn commit(&self, repo: &str, branch: &str) -> Result<GithubCommit, Box<dyn std::error::Error>> {
        self.github_json(&format!(
            "https://api.github.com/repos/{repo}/commits/{}",
            branch.trim_matches('/')
        ))
    }

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
            Err(reqwest_error) => curl_json(url).map_err(|curl_error| {
                format!("request failed with reqwest ({reqwest_error}) and curl ({curl_error})")
                    .into()
            }),
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
            last_error = Some(format!("curl exited with status {}", output.status));
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

fn render_catalog(sources: &[SkillSource]) -> String {
    let mut output = String::from(
        "# Reviewed Agent Skills Catalog\n\
         # Generated indexes are maintained by `sync-skills-catalog`.\n",
    );
    for source in sources {
        output.push_str("\n[[sources]]\n");
        push_field(&mut output, "id", &source.id);
        push_field(&mut output, "alias", &source.alias);
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
        push_field(
            &mut output,
            "discovery",
            match source.discovery {
                SkillDiscovery::Direct => "direct",
                SkillDiscovery::Recursive => "recursive",
            },
        );
        push_array(&mut output, "preferred_paths", &source.preferred_paths);
        push_array(&mut output, "excluded_skills", &source.excluded_skills);
        push_array(&mut output, "essential_skills", &source.essential_skills);
        push_skills(&mut output, &source.indexed_skills);

        for directory in &source.directories {
            output.push_str("\n[[sources.directories]]\n");
            push_field(&mut output, "path", &directory.path);
            push_field(&mut output, "source_url", &directory.source_url);
            output.push_str(&format!("verified = {}\n", directory.verified));
            output.push_str(&format!("installable = {}\n", directory.installable));
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

fn push_skills(output: &mut String, skills: &[CatalogSkill]) {
    if skills.is_empty() {
        output.push_str("indexed_skills = []\n");
        return;
    }
    output.push_str("indexed_skills = [\n");
    for skill in skills {
        output.push_str(&format!(
            "  {{ selector = \"{}\", name = \"{}\", path = \"{}\" }},\n",
            toml_escape(&skill.selector),
            toml_escape(&skill.name),
            toml_escape(&skill.path)
        ));
    }
    output.push_str("]\n");
}

fn toml_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    use acp_stack::runtime::install::skill_registry::SkillDirectory;

    #[test]
    fn mode_defaults_to_check() {
        assert_eq!(
            Options::from_args(Vec::<String>::new()).expect("mode"),
            Options { mode: Mode::Check }
        );
    }

    #[test]
    fn direct_discovery_uses_frontmatter_name() {
        let repository = tempfile::tempdir().expect("repository");
        write_skill(
            repository.path(),
            "skills/folder-name",
            "frontmatter-name",
            "body",
        );
        let source = source(SkillDiscovery::Direct, "skills");

        let skills = discover_source_skills(repository.path(), &source).expect("discovery");

        assert_eq!(
            skills,
            [CatalogSkill {
                selector: "frontmatter-name".to_owned(),
                name: "frontmatter-name".to_owned(),
                path: "skills/folder-name".to_owned(),
            }]
        );
    }

    #[test]
    fn recursive_discovery_only_indexes_skills_subtrees_at_any_depth() {
        let repository = tempfile::tempdir().expect("repository");
        write_skill(
            repository.path(),
            "plugin/skills/contact-center/android",
            "contact-center/android",
            "one",
        );
        write_skill(
            repository.path(),
            "plugin/helpers/ignored",
            "ignored",
            "two",
        );
        let source = source(SkillDiscovery::Recursive, "");

        let skills = discover_source_skills(repository.path(), &source).expect("discovery");

        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].selector, "contact-center/android");
        assert_eq!(skills[0].name, "contact-center/android");
        assert_eq!(skills[0].path, "plugin/skills/contact-center/android");
    }

    #[test]
    fn exact_duplicates_collapse_to_unique_preferred_copy() {
        let repository = tempfile::tempdir().expect("repository");
        write_skill(
            repository.path(),
            "agents/skills/analysis",
            "analysis",
            "same",
        );
        write_skill(
            repository.path(),
            "plugins/vertical-plugins/equity/skills/analysis",
            "analysis",
            "same",
        );
        let mut source = source(SkillDiscovery::Recursive, "");
        source.preferred_paths = vec!["plugins/vertical-plugins".to_owned()];

        let skills = discover_source_skills(repository.path(), &source).expect("deduplicated");

        assert_eq!(skills.len(), 1);
        assert_eq!(
            skills[0].path,
            "plugins/vertical-plugins/equity/skills/analysis"
        );
        assert_eq!(skills[0].selector, "analysis");
    }

    #[test]
    fn exact_duplicates_without_preference_fail() {
        let repository = tempfile::tempdir().expect("repository");
        write_skill(repository.path(), "one/skills/analysis", "analysis", "same");
        write_skill(repository.path(), "two/skills/analysis", "analysis", "same");
        let source = source(SkillDiscovery::Recursive, "");

        let error = discover_source_skills(repository.path(), &source).expect_err("ambiguous");

        assert!(
            error
                .to_string()
                .contains("without a unique preferred path")
        );
    }

    #[test]
    fn content_distinct_collisions_receive_contextual_selectors() {
        let repository = tempfile::tempdir().expect("repository");
        write_skill(
            repository.path(),
            "commercial-legal/skills/customize",
            "customize",
            "one",
        );
        write_skill(
            repository.path(),
            "corporate-legal/skills/customize",
            "customize",
            "two",
        );
        let source = source(SkillDiscovery::Recursive, "");

        let skills = discover_source_skills(repository.path(), &source).expect("variants");

        assert_eq!(
            skills
                .iter()
                .map(|skill| skill.selector.as_str())
                .collect::<Vec<_>>(),
            ["commercial-legal/customize", "corporate-legal/customize"]
        );
    }

    #[test]
    fn excluded_paths_are_removed_and_stale_paths_fail() {
        let repository = tempfile::tempdir().expect("repository");
        write_skill(repository.path(), "plugin/skills/start", "start", "one");
        let mut source = source(SkillDiscovery::Recursive, "");
        source.excluded_skills = vec!["plugin/skills/start".to_owned()];
        assert!(
            discover_source_skills(repository.path(), &source)
                .expect("excluded")
                .is_empty()
        );

        source.excluded_skills = vec!["plugin/skills/missing".to_owned()];
        let error = discover_source_skills(repository.path(), &source).expect_err("stale");
        assert!(error.to_string().contains("stale excluded path"));
    }

    #[test]
    fn invalid_or_missing_frontmatter_fails() {
        let repository = tempfile::tempdir().expect("repository");
        let directory = repository.path().join("skills/invalid");
        std::fs::create_dir_all(&directory).expect("directory");
        std::fs::write(directory.join(SKILL_DESCRIPTOR), "# Missing\n").expect("descriptor");
        let source = source(SkillDiscovery::Direct, "skills");

        let error = discover_source_skills(repository.path(), &source).expect_err("frontmatter");

        assert!(error.to_string().contains("missing YAML frontmatter"));
    }

    #[cfg(unix)]
    #[test]
    fn source_symlinks_are_rejected() {
        use std::os::unix::fs::symlink;

        let repository = tempfile::tempdir().expect("repository");
        let skills = repository.path().join("skills");
        std::fs::create_dir_all(&skills).expect("skills");
        symlink(repository.path(), skills.join("linked")).expect("symlink");
        let source = source(SkillDiscovery::Direct, "skills");

        let error = discover_source_skills(repository.path(), &source).expect_err("symlink");

        assert!(error.to_string().contains("refusing symlink"));
    }

    #[test]
    fn rendering_preserves_human_curation_fields() {
        let mut source = source(SkillDiscovery::Recursive, "");
        source.excluded_skills = vec!["plugin/skills/start".to_owned()];
        source.preferred_paths = vec!["plugins/vertical-plugins".to_owned()];
        source.essential_skills = vec!["analysis".to_owned()];
        source.indexed_skills = vec![CatalogSkill {
            selector: "analysis".to_owned(),
            name: "analysis".to_owned(),
            path: "plugin/skills/analysis".to_owned(),
        }];

        let rendered = render_catalog(&[source]);

        assert!(rendered.contains("excluded_skills = ["));
        assert!(rendered.contains("preferred_paths = ["));
        assert!(rendered.contains("essential_skills = ["));
        assert!(rendered.contains("selector = \"analysis\""));
    }

    fn source(discovery: SkillDiscovery, directory: &str) -> SkillSource {
        SkillSource {
            id: "openai-skills".to_owned(),
            alias: "openai".to_owned(),
            name: "OpenAI Agent Skills".to_owned(),
            owner: "openai".to_owned(),
            repo: "skills".to_owned(),
            url: "https://github.com/openai/skills".to_owned(),
            docs: vec!["https://github.com/openai/skills".to_owned()],
            official: true,
            trusted: true,
            reviewed_at: "2026-07-13".to_owned(),
            branch: "main".to_owned(),
            verified_commit: None,
            indexed_commit: None,
            descriptor: SKILL_DESCRIPTOR.to_owned(),
            discovery,
            preferred_paths: Vec::new(),
            excluded_skills: Vec::new(),
            essential_skills: Vec::new(),
            indexed_skills: Vec::new(),
            directories: vec![SkillDirectory {
                path: directory.to_owned(),
                source_url: if directory.is_empty() {
                    "https://github.com/openai/skills/tree/main".to_owned()
                } else {
                    format!("https://github.com/openai/skills/tree/main/{directory}")
                },
                verified: true,
                installable: true,
            }],
        }
    }

    fn write_skill(root: &Path, path: &str, name: &str, body: &str) {
        let directory = root.join(path);
        std::fs::create_dir_all(&directory).expect("skill directory");
        let mut descriptor = File::create(directory.join(SKILL_DESCRIPTOR)).expect("descriptor");
        writeln!(
            descriptor,
            "---\nname: {name}\ndescription: test\n---\n{body}"
        )
        .expect("skill body");
    }
}
