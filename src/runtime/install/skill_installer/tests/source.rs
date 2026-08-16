use super::super::*;
use super::support::*;

#[test]
fn parses_official_and_custom_sources() {
    let catalog = SkillCatalog::load_embedded().expect("catalog");
    assert_eq!(
        parse_skill_source("openai", &catalog).expect("openai"),
        SkillSourceSelection::Official {
            id: "openai-skills".to_owned()
        }
    );
    assert_eq!(
        parse_skill_source("anthropic", &catalog).expect("anthropic"),
        SkillSourceSelection::Official {
            id: "anthropic-skills".to_owned()
        }
    );
    assert_eq!(
        parse_skill_source("github:my-org", &catalog).expect("custom"),
        SkillSourceSelection::CustomGithubOwner {
            owner: "my-org".to_owned()
        }
    );
}

#[test]
fn resolve_source_ref_prefers_configured_user_source() {
    let catalog = SkillCatalog::load_embedded().expect("catalog");
    let sources = vec![user_source("my-org", "my-org/my-skills", "dev")];

    let resolved = resolve_source_ref("my-org", &sources, &catalog).expect("resolve");

    assert_eq!(resolved.owner, "my-org");
    assert_eq!(resolved.repo, "my-skills");
    assert_eq!(resolved.branch, "dev");
    assert!(!resolved.catalog_managed);
    assert_eq!(resolved.url, "https://github.com/my-org/my-skills");
    assert_eq!(resolved.directories[0].path, CUSTOM_SKILLS_DIRECTORY);
}

#[test]
fn resolve_source_ref_resolves_catalog_alias_and_ad_hoc_github() {
    let catalog = SkillCatalog::load_embedded().expect("catalog");

    let anthropic = resolve_source_ref("anthropic", &[], &catalog).expect("catalog alias");
    assert!(anthropic.catalog_managed);

    let repo = resolve_source_ref("github:acme/widgets", &[], &catalog).expect("ad-hoc repo");
    assert_eq!(
        (repo.owner.as_str(), repo.repo.as_str()),
        ("acme", "widgets")
    );
    assert!(!repo.catalog_managed);

    let owner_only = resolve_source_ref("github:acme", &[], &catalog).expect("owner-only");
    assert_eq!(owner_only.repo, "skills");
}

#[test]
fn resolve_source_ref_rejects_unknown_source() {
    let catalog = SkillCatalog::load_embedded().expect("catalog");
    let err = resolve_source_ref("nonsense", &[], &catalog).expect_err("unknown");
    assert!(matches!(err, StackError::SkillInstallInvalidSource { .. }));
}

#[test]
fn resolve_source_ref_catalog_alias_wins_over_shadowing_user_source() {
    // A hand-edited `[[skills.sources]]` entry whose alias collides with a
    // curated one must not hijack it: the catalog is resolved first.
    let catalog = SkillCatalog::load_embedded().expect("catalog");
    let sources = vec![user_source("anthropic", "evil/repo", "main")];

    let resolved = resolve_source_ref("anthropic", &sources, &catalog).expect("resolve");

    assert!(resolved.catalog_managed);
    assert_ne!(resolved.owner, "evil");
}

#[test]
fn resolve_source_ref_rejects_dot_segment_repo() {
    let catalog = SkillCatalog::load_embedded().expect("catalog");
    for reference in ["github:acme/..", "github:acme/."] {
        let err = resolve_source_ref(reference, &[], &catalog).expect_err("dot repo");
        assert!(matches!(err, StackError::SkillInstallInvalidSource { .. }));
    }
}

#[test]
fn resolves_custom_github_owner_to_skills_repo() {
    let catalog = SkillCatalog::load_embedded().expect("catalog");
    let selection = SkillSourceSelection::CustomGithubOwner {
        owner: "example-org".to_owned(),
    };

    let source = resolve_source(&selection, &catalog).expect("custom source");

    assert_eq!(source.owner, "example-org");
    assert_eq!(source.repo, CUSTOM_SKILLS_REPO);
    assert_eq!(source.branch, DEFAULT_SKILL_SOURCE_BRANCH);
    assert_eq!(source.url, "https://github.com/example-org/skills");
    assert_eq!(source.directories[0].path, CUSTOM_SKILLS_DIRECTORY);
    assert!(source.directories[0].installable);
}
