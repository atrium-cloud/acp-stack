use super::super::*;
use super::support::*;

#[test]
fn discover_source_skills_reads_frontmatter_for_user_source() {
    let catalog = SkillCatalog::load_embedded().expect("catalog");
    let sources = vec![user_source("my-org", "my-org/skills", "main")];
    let source = resolve_source_ref("my-org", &sources, &catalog).expect("resolve");
    let archive = tempfile::tempdir().expect("archive");
    write_catalog_skill(archive.path(), "skills/repo-map", "repo-map");
    write_catalog_skill(archive.path(), "skills/code-review", "code-review");

    let skills = discover_source_skills(&source, archive.path()).expect("discover");

    let selectors = skills
        .iter()
        .map(|s| s.selector.as_str())
        .collect::<Vec<_>>();
    assert_eq!(selectors, ["code-review", "repo-map"]);
    assert_eq!(skills[1].name, "repo-map");
    assert_eq!(skills[1].description.as_deref(), Some("test"));
    assert_eq!(skills[1].path, "skills/repo-map");
}

#[test]
fn discover_source_skills_degrades_on_malformed_descriptor() {
    // `add` (via `find_skill_dir`) installs a skill whose SKILL.md is any
    // regular file, so `source get` must still surface a sibling with malformed
    // frontmatter (degraded to the leaf name), not omit it or fail the listing.
    let catalog = SkillCatalog::load_embedded().expect("catalog");
    let sources = vec![user_source("my-org", "my-org/skills", "main")];
    let source = resolve_source_ref("my-org", &sources, &catalog).expect("resolve");
    let archive = tempfile::tempdir().expect("archive");
    write_catalog_skill(archive.path(), "skills/good", "good");
    // `write_skill` writes a descriptor with no YAML frontmatter.
    write_skill(archive.path(), "skills", "broken");

    let skills = discover_source_skills(&source, archive.path()).expect("discover");

    let selectors = skills
        .iter()
        .map(|skill| skill.selector.as_str())
        .collect::<Vec<_>>();
    assert_eq!(selectors, ["broken", "good"]);
    let broken = &skills[0];
    assert_eq!(broken.name, "broken");
    assert!(broken.description.is_none());
}

#[test]
fn discover_source_skills_reads_descriptions_for_catalog_source() {
    let archive = tempfile::tempdir().expect("archive");
    let path = "plugins/zoom/skills/general";
    write_catalog_skill(archive.path(), path, "zoom-general");
    let source = catalog_source(vec![CatalogSkill {
        selector: "zoom-general".to_owned(),
        name: "zoom-general".to_owned(),
        path: path.to_owned(),
    }]);

    let skills = discover_source_skills(&source, archive.path()).expect("discover");

    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].selector, "zoom-general");
    assert_eq!(skills[0].description.as_deref(), Some("test"));
    assert_eq!(skills[0].path, path);
}

#[test]
fn discover_source_skills_skips_catalog_skills_add_cannot_install() {
    // Catalog `add` (via `find_skill_dir`) requires a parseable descriptor
    // whose frontmatter name matches the index, so `source get` must skip
    // indexed skills that fail either check rather than listing them.
    let archive = tempfile::tempdir().expect("archive");
    write_catalog_skill(
        archive.path(),
        "plugins/zoom/skills/general",
        "zoom-general",
    );
    // `write_skill` writes a descriptor with no YAML frontmatter.
    write_skill(archive.path(), "plugins/zoom/skills", "broken");
    write_catalog_skill(archive.path(), "plugins/zoom/skills/renamed", "other-name");
    let source = catalog_source(vec![
        CatalogSkill {
            selector: "zoom-general".to_owned(),
            name: "zoom-general".to_owned(),
            path: "plugins/zoom/skills/general".to_owned(),
        },
        CatalogSkill {
            selector: "broken".to_owned(),
            name: "broken".to_owned(),
            path: "plugins/zoom/skills/broken".to_owned(),
        },
        CatalogSkill {
            selector: "renamed".to_owned(),
            name: "renamed".to_owned(),
            path: "plugins/zoom/skills/renamed".to_owned(),
        },
    ]);

    let skills = discover_source_skills(&source, archive.path()).expect("discover");

    let selectors = skills
        .iter()
        .map(|skill| skill.selector.as_str())
        .collect::<Vec<_>>();
    assert_eq!(selectors, ["zoom-general"]);
}
