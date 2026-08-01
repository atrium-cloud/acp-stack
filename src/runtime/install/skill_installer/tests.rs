use super::*;

fn source() -> ResolvedSkillSource {
    ResolvedSkillSource {
        id: "openai-skills".to_owned(),
        name: "OpenAI Agent Skills".to_owned(),
        owner: "openai".to_owned(),
        repo: "skills".to_owned(),
        url: "https://github.com/openai/skills".to_owned(),
        branch: "main".to_owned(),
        verified_commit: None,
        indexed_commit: None,
        descriptor: SKILL_DESCRIPTOR.to_owned(),
        catalog_managed: false,
        directories: vec![
            ResolvedSkillDirectory {
                path: "skills/.system".to_owned(),
                installable: false,
            },
            ResolvedSkillDirectory {
                path: "skills/.curated".to_owned(),
                installable: true,
            },
        ],
        indexed_skills: Vec::new(),
    }
}

fn catalog_source(skills: Vec<CatalogSkill>) -> ResolvedSkillSource {
    ResolvedSkillSource {
        id: "openai-plugins".to_owned(),
        name: "OpenAI Plugin Skills".to_owned(),
        owner: "openai".to_owned(),
        repo: "plugins".to_owned(),
        url: "https://github.com/openai/plugins".to_owned(),
        branch: "main".to_owned(),
        verified_commit: None,
        indexed_commit: None,
        descriptor: SKILL_DESCRIPTOR.to_owned(),
        catalog_managed: true,
        directories: Vec::new(),
        indexed_skills: skills,
    }
}

fn write_skill(root: &Path, directory: &str, name: &str) {
    let skill_dir = root.join(directory).join(name);
    std::fs::create_dir_all(&skill_dir).expect("skill dir");
    std::fs::write(skill_dir.join(SKILL_DESCRIPTOR), "# Skill\n").expect("descriptor");
    std::fs::write(skill_dir.join("script.sh"), "true\n").expect("script");
}

fn write_installed_skill(root: &Path, name: &str, descriptor: &str) {
    let skill_dir = root.join(name);
    std::fs::create_dir_all(&skill_dir).expect("skill dir");
    std::fs::write(skill_dir.join(SKILL_DESCRIPTOR), descriptor).expect("descriptor");
    std::fs::write(skill_dir.join("script.sh"), "true\n").expect("script");
}

fn write_catalog_skill(root: &Path, path: &str, name: &str) {
    let skill_dir = root.join(path);
    std::fs::create_dir_all(&skill_dir).expect("skill dir");
    std::fs::write(
        skill_dir.join(SKILL_DESCRIPTOR),
        format!("---\nname: {name}\ndescription: test\n---\n# Skill\n"),
    )
    .expect("descriptor");
    std::fs::write(skill_dir.join("script.sh"), "true\n").expect("script");
}

fn canonical_temp_home(tempdir: &tempfile::TempDir) -> PathBuf {
    tempdir.path().canonicalize().expect("canonical temp home")
}

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
fn resolves_custom_github_owner_to_skills_repo() {
    let catalog = SkillCatalog::load_embedded().expect("catalog");
    let selection = SkillSourceSelection::CustomGithubOwner {
        owner: "example-org".to_owned(),
    };

    let source = resolve_source(&selection, &catalog).expect("custom source");

    assert_eq!(source.owner, "example-org");
    assert_eq!(source.repo, CUSTOM_SKILLS_REPO);
    assert_eq!(source.branch, CUSTOM_SKILLS_BRANCH);
    assert_eq!(source.url, "https://github.com/example-org/skills");
    assert_eq!(source.directories[0].path, CUSTOM_SKILLS_DIRECTORY);
    assert!(source.directories[0].installable);
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

#[test]
fn port_skill_directories_shared_path_is_noop() {
    let home = tempfile::tempdir().expect("home");
    let source = canonical_temp_home(&home).join(".agents/skills");

    let report = port_skill_directories(&source, &source).expect("port");

    assert_eq!(report.status, SkillPortStatus::Shared);
    assert!(report.copied.is_empty());
    assert!(report.overwritten.is_empty());
}

#[test]
fn port_skill_directories_copies_valid_skills() {
    let home = tempfile::tempdir().expect("home");
    let home = canonical_temp_home(&home);
    let source = home.join(".agents/skills");
    let target = home.join(".config/agents/skills");
    write_installed_skill(&source, "repo-map", "# Repo Map\n");
    write_installed_skill(&source, "code-review", "# Code Review\n");

    let report = port_skill_directories(&source, &target).expect("port");

    assert_eq!(report.status, SkillPortStatus::Copied);
    assert_eq!(report.copied.len(), 2);
    assert!(target.join("repo-map").join(SKILL_DESCRIPTOR).is_file());
    assert!(target.join("code-review").join("script.sh").is_file());
}

#[test]
fn port_skill_directories_preserves_namespaced_skill_paths() {
    let home = tempfile::tempdir().expect("home");
    let home = canonical_temp_home(&home);
    let source = home.join(".agents/skills");
    let target = home.join(".config/agents/skills");
    write_installed_skill(
        &source,
        "contact-center/android",
        "---\nname: contact-center/android\n---\n",
    );

    let report = port_skill_directories(&source, &target).expect("port");

    assert_eq!(report.copied[0].name, "contact-center/android");
    assert!(
        target
            .join("contact-center/android")
            .join(SKILL_DESCRIPTOR)
            .is_file()
    );
}

#[test]
fn port_skill_directories_overwrites_valid_target_skill() {
    let home = tempfile::tempdir().expect("home");
    let home = canonical_temp_home(&home);
    let source = home.join(".agents/skills");
    let target = home.join(".config/agents/skills");
    write_installed_skill(&source, "repo-map", "# New\n");
    write_installed_skill(&target, "repo-map", "# Old\n");
    std::fs::write(target.join("repo-map").join("old.txt"), "old\n").expect("old file");

    let report = port_skill_directories(&source, &target).expect("port");

    assert_eq!(report.status, SkillPortStatus::Copied);
    assert!(report.copied.is_empty());
    assert_eq!(report.overwritten.len(), 1);
    assert_eq!(
        std::fs::read_to_string(target.join("repo-map").join(SKILL_DESCRIPTOR))
            .expect("descriptor"),
        "# New\n"
    );
    assert!(!target.join("repo-map").join("old.txt").exists());
}

#[test]
#[cfg(unix)]
fn port_skill_directories_preflight_rejects_nested_symlink_before_target_mutation() {
    let home = tempfile::tempdir().expect("home");
    let home = canonical_temp_home(&home);
    let source = home.join(".agents/skills");
    let target = home.join(".config/agents/skills");
    write_installed_skill(&source, "a-skill", "# New\n");
    write_installed_skill(&target, "a-skill", "# Old\n");
    write_installed_skill(&source, "b-skill", "# B\n");
    let external = tempfile::tempdir().expect("external");
    std::fs::create_dir_all(source.join("b-skill/nested")).expect("nested");
    std::os::unix::fs::symlink(external.path(), source.join("b-skill/nested/symlinked-dir"))
        .expect("symlink");

    let err = port_skill_directories(&source, &target).expect_err("nested symlink");

    assert!(matches!(err, StackError::SkillInstallFailed { .. }));
    assert_eq!(
        std::fs::read_to_string(target.join("a-skill").join(SKILL_DESCRIPTOR)).expect("descriptor"),
        "# Old\n"
    );
}

#[test]
fn port_skill_directories_rejects_target_conflict() {
    let home = tempfile::tempdir().expect("home");
    let home = canonical_temp_home(&home);
    let source = home.join(".agents/skills");
    let target = home.join(".config/agents/skills");
    write_installed_skill(&source, "repo-map", "# Repo Map\n");
    std::fs::create_dir_all(target.join("repo-map")).expect("target");

    let err = port_skill_directories(&source, &target).expect_err("conflict");

    assert!(matches!(err, StackError::SkillInstallTargetConflict { .. }));
}

#[test]
#[cfg(unix)]
fn port_skill_directories_rejects_source_symlink() {
    let home = tempfile::tempdir().expect("home");
    let home = canonical_temp_home(&home);
    let source = home.join(".agents/skills");
    let target = home.join(".config/agents/skills");
    let external = tempfile::tempdir().expect("external");
    std::fs::create_dir_all(&source).expect("source root");
    std::fs::write(external.path().join(SKILL_DESCRIPTOR), "# Skill\n").expect("descriptor");
    std::os::unix::fs::symlink(external.path(), source.join("repo-map")).expect("symlink");

    let err = port_skill_directories(&source, &target).expect_err("symlink");

    assert!(matches!(err, StackError::SkillInstallFailed { .. }));
}

#[test]
fn port_skill_directories_skips_non_skill_directories() {
    let home = tempfile::tempdir().expect("home");
    let home = canonical_temp_home(&home);
    let source = home.join(".agents/skills");
    let target = home.join(".config/agents/skills");
    std::fs::create_dir_all(source.join("notes")).expect("notes");
    std::fs::create_dir_all(source.join("BadName")).expect("bad name");
    std::fs::write(source.join("README.md"), "readme\n").expect("readme");

    let report = port_skill_directories(&source, &target).expect("port");

    assert_eq!(report.status, SkillPortStatus::NoneFound);
    assert!(!target.exists());
}

#[test]
fn port_skill_directories_rejects_root_skill_descriptor() {
    let home = tempfile::tempdir().expect("home");
    let home = canonical_temp_home(&home);
    let source = home.join(".agents/skills");
    let target = home.join(".config/agents/skills");
    std::fs::create_dir_all(&source).expect("source root");
    std::fs::write(source.join(SKILL_DESCRIPTOR), "# Root\n").expect("descriptor");

    let err = port_skill_directories(&source, &target).expect_err("root descriptor");

    assert!(matches!(err, StackError::SkillInstallFailed { .. }));
    assert!(!target.exists());
}

#[test]
fn port_skill_directories_rejects_unportable_skill_name() {
    let home = tempfile::tempdir().expect("home");
    let home = canonical_temp_home(&home);
    let source = home.join(".agents/skills");
    let target = home.join(".config/agents/skills");
    write_installed_skill(&source, "_bad", "# Bad\n");

    let err = port_skill_directories(&source, &target).expect_err("unportable name");

    assert!(matches!(err, StackError::SkillInstallFailed { .. }));
    assert!(!target.exists());
}

#[test]
fn port_skill_directories_missing_source_is_none_found() {
    let home = tempfile::tempdir().expect("home");
    let home = canonical_temp_home(&home);

    let report = port_skill_directories(
        &home.join(".agents/skills"),
        &home.join(".config/agents/skills"),
    )
    .expect("port");

    assert_eq!(report.status, SkillPortStatus::NoneFound);
    assert!(report.copied.is_empty());
    assert!(report.overwritten.is_empty());
}

#[test]
fn port_agent_skills_treats_unknown_source_agent_as_noop() {
    let home = tempfile::tempdir().expect("home");
    let catalog = RegistryCatalog::load_embedded().expect("registry");

    let report =
        port_agent_skills(home.path(), &catalog, "removed-agent", "opencode").expect("port");

    assert_eq!(report, None);
}

fn claude_code_entry(
    catalog: &RegistryCatalog,
) -> &crate::runtime::install::agent_registry::RegistryEntry {
    catalog.lookup("claude-code").expect("claude-code entry")
}

#[test]
fn link_agent_skills_is_none_without_link_dir() {
    let home = tempfile::tempdir().expect("home");
    let catalog = RegistryCatalog::load_embedded().expect("registry");
    let opencode = catalog.lookup("opencode").expect("opencode entry");

    let report = link_agent_skills(home.path(), opencode).expect("link");

    assert_eq!(report, None);
}

#[test]
fn link_agent_skills_links_installed_skills() {
    let home = tempfile::tempdir().expect("home");
    let catalog = RegistryCatalog::load_embedded().expect("registry");
    let home_path = canonical_temp_home(&home);
    let install_root = home_path.join(".agents/skills");
    write_installed_skill(&install_root, "repo-map", "# Repo Map\n");
    write_installed_skill(&install_root, "contact-center/android", "# Android\n");

    let report = link_agent_skills(home.path(), claude_code_entry(&catalog))
        .expect("link")
        .expect("report");

    assert_eq!(report.linked.len(), 2);
    assert!(report.conflicts.is_empty());
    let link = home_path.join(".claude/skills/repo-map");
    assert!(
        std::fs::symlink_metadata(&link)
            .expect("link metadata")
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        std::fs::read_link(&link).expect("link target"),
        install_root.join("repo-map")
    );
    let nested = home_path.join(".claude/skills/contact-center/android");
    assert_eq!(
        std::fs::read_link(&nested).expect("nested link target"),
        install_root.join("contact-center/android")
    );
    assert!(nested.join(SKILL_DESCRIPTOR).is_file());
}

#[test]
fn link_agent_skills_is_idempotent_and_repoints_stale_links() {
    let home = tempfile::tempdir().expect("home");
    let catalog = RegistryCatalog::load_embedded().expect("registry");
    let home_path = canonical_temp_home(&home);
    let install_root = home_path.join(".agents/skills");
    write_installed_skill(&install_root, "repo-map", "# Repo Map\n");
    let link_root = home_path.join(".claude/skills");
    std::fs::create_dir_all(&link_root).expect("link root");
    // The stale link points into the install root at a since-removed skill,
    // so repointing must win over pruning: once refreshed the link targets
    // the live skill and prune keeps it.
    std::os::unix::fs::symlink(install_root.join("old-name"), link_root.join("repo-map"))
        .expect("stale");

    let entry = claude_code_entry(&catalog);
    let first = link_agent_skills(home.path(), entry)
        .expect("link")
        .expect("report");
    let second = link_agent_skills(home.path(), entry)
        .expect("relink")
        .expect("report");

    // The first refresh repoints the stale link; the second finds it
    // already correct and reports it as unchanged instead of linked.
    assert_eq!(first.linked.len(), 1);
    assert!(first.unchanged.is_empty());
    assert!(first.pruned.is_empty());
    assert!(first.conflicts.is_empty());
    assert!(second.linked.is_empty());
    assert_eq!(second.unchanged.len(), 1);
    assert!(second.pruned.is_empty());
    assert!(second.conflicts.is_empty());
    assert_eq!(
        std::fs::read_link(link_root.join("repo-map")).expect("target"),
        install_root.join("repo-map")
    );
}

#[test]
fn link_agent_skills_keeps_existing_real_directory_as_conflict() {
    let home = tempfile::tempdir().expect("home");
    let catalog = RegistryCatalog::load_embedded().expect("registry");
    let home_path = canonical_temp_home(&home);
    let install_root = home_path.join(".agents/skills");
    write_installed_skill(&install_root, "repo-map", "# Managed\n");
    let link_root = home_path.join(".claude/skills");
    write_installed_skill(&link_root, "repo-map", "# User owned\n");

    let report = link_agent_skills(home.path(), claude_code_entry(&catalog))
        .expect("link")
        .expect("report");

    assert!(report.linked.is_empty());
    assert_eq!(report.conflicts.len(), 1);
    assert!(
        !std::fs::symlink_metadata(link_root.join("repo-map"))
            .expect("metadata")
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        std::fs::read_to_string(link_root.join("repo-map").join(SKILL_DESCRIPTOR))
            .expect("descriptor"),
        "# User owned\n"
    );
}

#[test]
fn link_agent_skills_keeps_existing_regular_file_as_conflict() {
    let home = tempfile::tempdir().expect("home");
    let catalog = RegistryCatalog::load_embedded().expect("registry");
    let home_path = canonical_temp_home(&home);
    write_installed_skill(&home_path.join(".agents/skills"), "repo-map", "# Managed\n");
    let link_root = home_path.join(".claude/skills");
    std::fs::create_dir_all(&link_root).expect("link root");
    std::fs::write(link_root.join("repo-map"), "not a directory\n").expect("file");

    let report = link_agent_skills(home.path(), claude_code_entry(&catalog))
        .expect("link")
        .expect("report");

    assert!(report.linked.is_empty());
    assert_eq!(report.conflicts.len(), 1);
    assert_eq!(
        std::fs::read_to_string(link_root.join("repo-map")).expect("file"),
        "not a directory\n"
    );
}

#[test]
fn link_agent_skills_resolves_symlinked_link_root_ancestor() {
    let home = tempfile::tempdir().expect("home");
    let catalog = RegistryCatalog::load_embedded().expect("registry");
    let home_path = canonical_temp_home(&home);
    let install_root = home_path.join(".agents/skills");
    write_installed_skill(&install_root, "repo-map", "# Repo Map\n");
    // Dotfiles-style setup: ~/.claude is a symlink to a real directory.
    let dotfiles_claude = home_path.join("dotfiles/claude");
    std::fs::create_dir_all(&dotfiles_claude).expect("dotfiles dir");
    std::os::unix::fs::symlink(&dotfiles_claude, home_path.join(".claude")).expect("symlink");

    let report = link_agent_skills(home.path(), claude_code_entry(&catalog))
        .expect("link")
        .expect("report");

    assert_eq!(report.link_root, dotfiles_claude.join("skills"));
    assert_eq!(report.linked.len(), 1);
    assert_eq!(
        std::fs::read_link(dotfiles_claude.join("skills/repo-map")).expect("target"),
        install_root.join("repo-map")
    );
    // The harness path traverses the ~/.claude symlink to the same skill.
    assert!(
        home_path
            .join(".claude/skills/repo-map")
            .join(SKILL_DESCRIPTOR)
            .is_file()
    );
}

#[test]
fn link_agent_skills_prunes_dangling_links_but_keeps_foreign_ones() {
    let home = tempfile::tempdir().expect("home");
    let catalog = RegistryCatalog::load_embedded().expect("registry");
    let home_path = canonical_temp_home(&home);
    let install_root = home_path.join(".agents/skills");
    write_installed_skill(&install_root, "repo-map", "# Repo Map\n");
    let link_root = home_path.join(".claude/skills");
    std::fs::create_dir_all(&link_root).expect("link root");
    // Dangling link into the install root: skill was removed after linking.
    std::os::unix::fs::symlink(
        install_root.join("removed-skill"),
        link_root.join("removed-skill"),
    )
    .expect("dangling link");
    // Dangling link elsewhere: user-owned, must not be touched.
    std::os::unix::fs::symlink(
        home_path.join("no-such-target"),
        link_root.join("user-link"),
    )
    .expect("foreign link");

    let report = link_agent_skills(home.path(), claude_code_entry(&catalog))
        .expect("link")
        .expect("report");

    assert_eq!(report.linked.len(), 1);
    assert_eq!(report.pruned.len(), 1);
    assert_eq!(report.pruned[0].name, "removed-skill");
    assert!(std::fs::symlink_metadata(link_root.join("removed-skill")).is_err());
    assert!(
        std::fs::symlink_metadata(link_root.join("user-link"))
            .expect("foreign link kept")
            .file_type()
            .is_symlink()
    );
}

#[test]
fn link_agent_skills_missing_install_root_returns_none() {
    let home = tempfile::tempdir().expect("home");
    let catalog = RegistryCatalog::load_embedded().expect("registry");
    let home_path = canonical_temp_home(&home);

    let report = link_agent_skills(home.path(), claude_code_entry(&catalog)).expect("link");

    assert_eq!(report, None);
    assert!(!home_path.join(".claude/skills").exists());
}

#[test]
fn link_agent_skills_skips_stray_symlinks_in_install_root() {
    let home = tempfile::tempdir().expect("home");
    let catalog = RegistryCatalog::load_embedded().expect("registry");
    let home_path = canonical_temp_home(&home);
    let install_root = home_path.join(".agents/skills");
    write_installed_skill(&install_root, "repo-map", "# Repo Map\n");
    // A symlink inside a skill dir would fail the copy-time port checks;
    // linking copies nothing, so the skill still links.
    std::os::unix::fs::symlink(
        home_path.join("elsewhere"),
        install_root.join("repo-map/linked-file"),
    )
    .expect("symlink inside skill");
    // A stray symlink at the install root is skipped, not an error.
    std::os::unix::fs::symlink(home_path.join("elsewhere"), install_root.join("stray"))
        .expect("stray symlink");

    let report = link_agent_skills(home.path(), claude_code_entry(&catalog))
        .expect("link")
        .expect("report");

    assert_eq!(report.linked.len(), 1);
    assert_eq!(report.linked[0].name, "repo-map");
    assert!(
        home_path
            .join(".claude/skills/repo-map")
            .join(SKILL_DESCRIPTOR)
            .is_file()
    );
}

#[test]
fn link_agent_skills_resolves_symlinked_install_root_ancestor() {
    let home = tempfile::tempdir().expect("home");
    let catalog = RegistryCatalog::load_embedded().expect("registry");
    let home_path = canonical_temp_home(&home);
    // Dotfiles-style setup: ~/.agents is a symlink to a real directory.
    let real_agents = home_path.join("dotfiles/agents");
    write_installed_skill(&real_agents.join("skills"), "repo-map", "# Repo Map\n");
    std::os::unix::fs::symlink(&real_agents, home_path.join(".agents")).expect("symlink");

    let report = link_agent_skills(home.path(), claude_code_entry(&catalog))
        .expect("link")
        .expect("report");

    assert_eq!(report.install_root, real_agents.join("skills"));
    assert_eq!(report.linked.len(), 1);
    assert_eq!(
        std::fs::read_link(home_path.join(".claude/skills/repo-map")).expect("target"),
        real_agents.join("skills/repo-map")
    );
}

#[test]
fn link_agent_skills_leaves_user_directories_untouched() {
    let home = tempfile::tempdir().expect("home");
    let catalog = RegistryCatalog::load_embedded().expect("registry");
    let home_path = canonical_temp_home(&home);
    let install_root = home_path.join(".agents/skills");
    write_installed_skill(&install_root, "repo-map", "# Repo Map\n");
    // A user-owned directory in the link root holding a dangling symlink
    // pointing outside the install root plus a real file: recursion reaches
    // the directory, but nothing user-owned is modified or pruned.
    let user_dir = home_path.join(".claude/skills/user-skill");
    std::fs::create_dir_all(&user_dir).expect("user dir");
    std::os::unix::fs::symlink(home_path.join("no-such-target"), user_dir.join("ref"))
        .expect("user symlink");
    std::fs::write(user_dir.join("notes.md"), "user notes\n").expect("user file");

    let report = link_agent_skills(home.path(), claude_code_entry(&catalog))
        .expect("link")
        .expect("report");

    assert_eq!(report.linked.len(), 1);
    assert!(report.pruned.is_empty());
    assert!(
        std::fs::symlink_metadata(user_dir.join("ref"))
            .expect("user symlink kept")
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        std::fs::read_to_string(user_dir.join("notes.md")).expect("user file kept"),
        "user notes\n"
    );
}

#[test]
fn link_agent_skills_best_effort_reports_error_without_failing() {
    let home = tempfile::tempdir().expect("home");
    let catalog = RegistryCatalog::load_embedded().expect("registry");
    let home_path = canonical_temp_home(&home);
    write_installed_skill(
        &home_path.join(".agents/skills"),
        "repo-map",
        "# Repo Map\n",
    );
    // A regular file where the harness config dir should be makes the link
    // root unusable; the failure is reported, not propagated.
    std::fs::write(home_path.join(".claude"), "not a directory\n").expect("file");

    let outcome = link_agent_skills_best_effort(home.path(), claude_code_entry(&catalog));

    assert_eq!(outcome.report, None);
    let error = outcome.error.expect("error reported");
    assert!(error.contains(".claude"), "error: {error}");
}

#[test]
fn link_agent_skills_prunes_nested_links_and_emptied_group_dirs() {
    let home = tempfile::tempdir().expect("home");
    let catalog = RegistryCatalog::load_embedded().expect("registry");
    let home_path = canonical_temp_home(&home);
    let install_root = home_path.join(".agents/skills");
    write_installed_skill(&install_root, "contact-center/android", "# Android\n");
    write_installed_skill(&install_root, "tools/jq", "# Jq\n");
    let link_root = home_path.join(".claude/skills");

    let entry = claude_code_entry(&catalog);
    let first = link_agent_skills(home.path(), entry)
        .expect("link")
        .expect("report");
    assert_eq!(first.linked.len(), 2);
    // User content inside a managed group dir keeps the dir alive.
    std::fs::write(link_root.join("contact-center/notes.md"), "user notes\n").expect("user file");

    // Remove both skills from the install root: the nested links dangle.
    std::fs::remove_dir_all(install_root.join("contact-center")).expect("remove skill");
    std::fs::remove_dir_all(install_root.join("tools")).expect("remove skill");
    let second = link_agent_skills(home.path(), entry)
        .expect("relink")
        .expect("report");

    assert_eq!(second.pruned.len(), 2);
    assert_eq!(second.pruned[0].name, "contact-center/android");
    assert_eq!(second.pruned[1].name, "tools/jq");
    // The emptied `tools` group dir is removed with its pruned link...
    assert!(!link_root.join("tools").exists());
    // ...but `contact-center` still holds user content, so it stays,
    // with the user file untouched.
    assert_eq!(
        std::fs::read_to_string(link_root.join("contact-center/notes.md")).expect("user file kept"),
        "user notes\n"
    );
}

#[test]
fn link_agent_skills_collects_per_skill_errors_and_continues() {
    let home = tempfile::tempdir().expect("home");
    let catalog = RegistryCatalog::load_embedded().expect("registry");
    let home_path = canonical_temp_home(&home);
    let install_root = home_path.join(".agents/skills");
    write_installed_skill(&install_root, "alpha", "# Alpha\n");
    write_installed_skill(&install_root, "contact-center/android", "# Android\n");
    let link_root = home_path.join(".claude/skills");
    std::fs::create_dir_all(&link_root).expect("link root");
    // A user symlink where the nested skill's group dir should be blocks
    // that one skill; the other must still link, and prune must still run.
    std::os::unix::fs::symlink(
        home_path.join("elsewhere"),
        link_root.join("contact-center"),
    )
    .expect("group symlink");
    std::os::unix::fs::symlink(
        install_root.join("removed-skill"),
        link_root.join("removed-skill"),
    )
    .expect("dangling link");

    let report = link_agent_skills(home.path(), claude_code_entry(&catalog))
        .expect("link")
        .expect("report");

    assert_eq!(report.linked.len(), 1);
    assert_eq!(report.linked[0].name, "alpha");
    assert_eq!(report.errors.len(), 1);
    assert!(
        report.errors[0].contains("contact-center/android"),
        "error: {}",
        report.errors[0]
    );
    assert_eq!(report.pruned.len(), 1);
    assert_eq!(report.pruned[0].name, "removed-skill");
    assert!(link_root.join("alpha").join(SKILL_DESCRIPTOR).is_file());
}
