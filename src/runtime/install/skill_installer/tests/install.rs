use super::super::*;
use super::support::*;

#[test]
fn rejects_invalid_skill_names() {
    for name in ["", "Upper", "two--dash", "-bad", "bad_", "bad//name"] {
        let err = parse_skill_names(&[name.to_owned()]).expect_err("invalid");
        assert!(matches!(err, StackError::SkillInstallInvalidName { .. }));
    }
}

#[test]
fn accepts_path_qualified_skill_selectors() {
    assert_eq!(
        parse_skill_names(&["zoom-plugin/contact-center/android".to_owned()])
            .expect("qualified selector"),
        ["zoom-plugin/contact-center/android"]
    );
}

#[test]
fn custom_sources_reject_qualified_selectors_during_preflight() {
    let catalog = SkillCatalog::load_embedded().expect("catalog");
    let source = resolve_source(
        &SkillSourceSelection::CustomGithubOwner {
            owner: "example-org".to_owned(),
        },
        &catalog,
    )
    .expect("custom source");

    let error = validate_requested_skills(&source, &["nested/skill".to_owned()])
        .expect_err("custom selector rejected");

    assert!(matches!(error, StackError::SkillInstallInvalidName { .. }));
}

#[test]
fn rejects_duplicate_skill_names() {
    let err = parse_skill_names(&["repo-map,repo-map".to_owned()]).expect_err("duplicate rejected");
    assert!(matches!(err, StackError::SkillInstallFailed { .. }));
}

#[test]
fn install_from_extracted_root_copies_multiple_skills() {
    let archive = tempfile::tempdir().expect("archive");
    let home = tempfile::tempdir().expect("home");
    write_skill(archive.path(), "skills/.curated", "repo-map");
    write_skill(archive.path(), "skills/.curated", "code-review");
    let destination = canonical_temp_home(&home).join(".agents/skills");

    let report = install_from_extracted_root(
        &source(),
        archive.path(),
        &destination,
        &["repo-map,code-review".to_owned()],
    )
    .expect("install");

    assert_eq!(report.installed.len(), 2);
    assert!(
        destination
            .join("repo-map")
            .join(SKILL_DESCRIPTOR)
            .is_file()
    );
    assert!(destination.join("code-review").join("script.sh").is_file());
    // Installed skills carry the managed marker recording the source id.
    assert_eq!(
        std::fs::read_to_string(destination.join("repo-map").join(MANAGED_SKILL_MARKER))
            .expect("marker"),
        "openai-skills\n"
    );
}

#[test]
fn catalog_install_uses_exact_path_and_frontmatter_install_name() {
    let archive = tempfile::tempdir().expect("archive");
    let home = tempfile::tempdir().expect("home");
    let path = "plugins/zoom/skills/contact-center/android";
    write_catalog_skill(archive.path(), path, "contact-center/android");
    let source = catalog_source(vec![CatalogSkill {
        selector: "zoom-plugin/contact-center/android".to_owned(),
        name: "contact-center/android".to_owned(),
        path: path.to_owned(),
    }]);
    let destination = canonical_temp_home(&home).join(".agents/skills");

    let report = install_from_extracted_root(
        &source,
        archive.path(),
        &destination,
        &["zoom-plugin/contact-center/android".to_owned()],
    )
    .expect("install");

    assert_eq!(report.installed[0].name, "contact-center/android");
    assert!(
        destination
            .join("contact-center/android")
            .join(SKILL_DESCRIPTOR)
            .is_file()
    );
}

#[test]
fn catalog_install_rejects_changed_frontmatter_name() {
    let archive = tempfile::tempdir().expect("archive");
    let home = tempfile::tempdir().expect("home");
    let path = "plugins/zoom/skills/general";
    write_catalog_skill(archive.path(), path, "changed-name");
    let source = catalog_source(vec![CatalogSkill {
        selector: "zoom-general".to_owned(),
        name: "zoom-general".to_owned(),
        path: path.to_owned(),
    }]);

    let error = install_from_extracted_root(
        &source,
        archive.path(),
        &canonical_temp_home(&home).join(".agents/skills"),
        &["zoom-general".to_owned()],
    )
    .expect_err("frontmatter mismatch");

    assert!(matches!(error, StackError::SkillInstallFailed { .. }));
}

#[test]
fn catalog_install_rejects_two_variants_with_same_target() {
    let archive = tempfile::tempdir().expect("archive");
    let home = tempfile::tempdir().expect("home");
    for path in ["one/skills/customize", "two/skills/customize"] {
        write_catalog_skill(archive.path(), path, "customize");
    }
    let source = catalog_source(vec![
        CatalogSkill {
            selector: "one/customize".to_owned(),
            name: "customize".to_owned(),
            path: "one/skills/customize".to_owned(),
        },
        CatalogSkill {
            selector: "two/customize".to_owned(),
            name: "customize".to_owned(),
            path: "two/skills/customize".to_owned(),
        },
    ]);

    let error = install_from_extracted_root(
        &source,
        archive.path(),
        &canonical_temp_home(&home).join(".agents/skills"),
        &["one/customize,two/customize".to_owned()],
    )
    .expect_err("duplicate target");

    assert!(matches!(error, StackError::SkillInstallFailed { .. }));
}

#[test]
fn catalog_install_rejects_parent_and_nested_install_targets() {
    let source = catalog_source(vec![
        CatalogSkill {
            selector: "zoom-mcp".to_owned(),
            name: "zoom-mcp".to_owned(),
            path: "zoom/skills/zoom-mcp".to_owned(),
        },
        CatalogSkill {
            selector: "zoom-mcp/whiteboard".to_owned(),
            name: "zoom-mcp/whiteboard".to_owned(),
            path: "zoom/skills/zoom-mcp/whiteboard".to_owned(),
        },
    ]);

    let error = validate_requested_skills(&source, &["zoom-mcp,zoom-mcp/whiteboard".to_owned()])
        .expect_err("overlapping targets");

    assert!(matches!(error, StackError::SkillInstallFailed { .. }));
}

#[test]
fn catalog_install_rejects_nested_target_inside_installed_skill() {
    let archive = tempfile::tempdir().expect("archive");
    let home = tempfile::tempdir().expect("home");
    let path = "plugins/example/skills/web";
    write_catalog_skill(archive.path(), path, "ui-toolkit/web");
    let source = catalog_source(vec![CatalogSkill {
        selector: "ui-toolkit/web".to_owned(),
        name: "ui-toolkit/web".to_owned(),
        path: path.to_owned(),
    }]);
    let destination = canonical_temp_home(&home).join(".agents/skills");
    std::fs::create_dir_all(destination.join("ui-toolkit")).expect("installed parent");
    std::fs::write(
        destination.join("ui-toolkit").join(SKILL_DESCRIPTOR),
        "# Installed parent\n",
    )
    .expect("parent descriptor");

    let error = install_from_extracted_root(
        &source,
        archive.path(),
        &destination,
        &["ui-toolkit/web".to_owned()],
    )
    .expect_err("installed ancestor rejected");

    assert!(matches!(
        error,
        StackError::SkillInstallTargetConflict { path, .. }
            if path == destination.join("ui-toolkit")
    ));
}

#[test]
fn install_from_extracted_root_ignores_noninstallable_system_directory() {
    let archive = tempfile::tempdir().expect("archive");
    let home = tempfile::tempdir().expect("home");
    write_skill(archive.path(), "skills/.system", "internal-only");
    let destination = canonical_temp_home(&home).join(".agents/skills");

    let err = install_from_extracted_root(
        &source(),
        archive.path(),
        &destination,
        &["internal-only".to_owned()],
    )
    .expect_err("system skill not installable");

    assert!(matches!(err, StackError::SkillInstallSkillMissing { .. }));
}

#[test]
fn install_from_extracted_root_rejects_missing_skill() {
    let archive = tempfile::tempdir().expect("archive");
    let home = tempfile::tempdir().expect("home");

    let err = install_from_extracted_root(
        &source(),
        archive.path(),
        &canonical_temp_home(&home).join(".agents/skills"),
        &["missing-skill".to_owned()],
    )
    .expect_err("missing skill");

    assert!(matches!(err, StackError::SkillInstallSkillMissing { .. }));
}

#[test]
fn install_from_extracted_root_rejects_descriptor_symlink() {
    let archive = tempfile::tempdir().expect("archive");
    let home = tempfile::tempdir().expect("home");
    let skill_dir = archive.path().join("skills/.curated/linked-skill");
    std::fs::create_dir_all(&skill_dir).expect("skill dir");
    std::fs::write(archive.path().join("target.md"), "# Skill\n").expect("target");
    #[cfg(unix)]
    std::os::unix::fs::symlink("../../target.md", skill_dir.join(SKILL_DESCRIPTOR))
        .expect("symlink");

    #[cfg(unix)]
    {
        let err = install_from_extracted_root(
            &source(),
            archive.path(),
            &canonical_temp_home(&home).join(".agents/skills"),
            &["linked-skill".to_owned()],
        )
        .expect_err("symlink descriptor rejected");
        assert!(matches!(err, StackError::SkillInstallFailed { .. }));
    }
}

#[test]
fn install_from_extracted_root_rejects_target_conflict() {
    let archive = tempfile::tempdir().expect("archive");
    let home = tempfile::tempdir().expect("home");
    write_skill(archive.path(), "skills/.curated", "repo-map");
    let destination = canonical_temp_home(&home).join(".agents/skills");
    std::fs::create_dir_all(destination.join("repo-map")).expect("target");

    let err = install_from_extracted_root(
        &source(),
        archive.path(),
        &destination,
        &["repo-map".to_owned()],
    )
    .expect_err("target conflict");

    assert!(matches!(err, StackError::SkillInstallTargetConflict { .. }));
}

#[test]
fn install_from_extracted_root_skips_existing_skill() {
    let archive = tempfile::tempdir().expect("archive");
    let home = tempfile::tempdir().expect("home");
    write_skill(archive.path(), "skills/.curated", "repo-map");
    let destination = canonical_temp_home(&home).join(".agents/skills");
    std::fs::create_dir_all(destination.join("repo-map")).expect("target");
    std::fs::write(
        destination.join("repo-map").join(SKILL_DESCRIPTOR),
        "# Old\n",
    )
    .expect("descriptor");

    let report = install_from_extracted_root(
        &source(),
        archive.path(),
        &destination,
        &["repo-map".to_owned()],
    )
    .expect("idempotent skip");

    assert!(report.installed.is_empty());
    assert_eq!(report.skipped.len(), 1);
}

#[test]
#[cfg(unix)]
fn all_skills_installed_rejects_symlinked_target() {
    let home = tempfile::tempdir().expect("home");
    let destination = canonical_temp_home(&home).join(".agents/skills");
    let external = tempfile::tempdir().expect("external");
    std::fs::create_dir_all(&destination).expect("destination");
    std::fs::write(external.path().join(SKILL_DESCRIPTOR), "# Skill\n").expect("descriptor");
    std::os::unix::fs::symlink(external.path(), destination.join("repo-map")).expect("symlink");

    assert!(!all_skills_installed(
        &source(),
        &destination,
        &["repo-map".to_owned()]
    ));
}

#[test]
#[cfg(unix)]
fn install_from_extracted_root_rejects_symlinked_destination_ancestor() {
    let archive = tempfile::tempdir().expect("archive");
    let home = tempfile::tempdir().expect("home");
    let external = tempfile::tempdir().expect("external");
    write_skill(archive.path(), "skills/.curated", "repo-map");
    let home_path = canonical_temp_home(&home);
    std::os::unix::fs::symlink(external.path(), home_path.join(".agents")).expect("symlink");
    let destination = home_path.join(".agents/skills");

    let err = install_from_extracted_root(
        &source(),
        archive.path(),
        &destination,
        &["repo-map".to_owned()],
    )
    .expect_err("symlinked ancestor rejected");

    assert!(matches!(err, StackError::SkillInstallTargetConflict { .. }));
}

#[test]
fn expands_home_relative_install_dir() {
    let home = Path::new("/tmp/test-home");
    assert_eq!(
        expand_agent_skills_install_dir(home, "~/.agents/skills").expect("expand"),
        Path::new("/tmp/test-home/.agents/skills")
    );
}
