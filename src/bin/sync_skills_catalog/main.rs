//! Refresh or validate the checked-in Agent Skills catalog.

use std::collections::BTreeSet;
use std::process::ExitCode;
use std::time::Duration;

use acp_stack::runtime::install::skill_registry::{CatalogSkill, SkillCatalog, SkillSource};
use acp_stack::runtime::workspace_sources::safe_extract::{ExtractOpts, extract_archive};

mod discovery;
mod github;
mod render;

use self::discovery::discover_source_skills;
use self::github::{GithubClient, download_archive};
use self::render::render_catalog;

const SKILLS_TOML_PATH: &str = "data/skills.toml";
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const CURL_TIMEOUT_SECONDS: &str = "60";
pub(crate) const CURL_RETRY_COUNT: &str = "3";
pub(crate) const CURL_RETRY_DELAY_SECONDS: &str = "2";
pub(crate) const CURL_JSON_ATTEMPTS: usize = 3;
pub(crate) const GITHUB_ARCHIVE_MAX_BYTES: u64 = 200 * 1024 * 1024;
pub(crate) const SKILL_DESCRIPTOR: &str = "SKILL.md";

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use std::path::Path;

    use acp_stack::runtime::install::skill_registry::{SkillDirectory, SkillDiscovery};

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
