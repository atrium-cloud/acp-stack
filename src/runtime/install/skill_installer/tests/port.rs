use super::super::*;
use super::support::*;

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
fn port_skill_directories_skips_target_skill_not_installed_by_acp_stack() {
    // A same-named target without the managed marker is the user's own content.
    let home = tempfile::tempdir().expect("home");
    let home = canonical_temp_home(&home);
    let source = home.join(".agents/skills");
    let target = home.join(".config/agents/skills");
    write_installed_skill(&source, "repo-map", "# Managed New\n");
    let user_skill = target.join("repo-map");
    std::fs::create_dir_all(&user_skill).expect("user skill dir");
    std::fs::write(user_skill.join(SKILL_DESCRIPTOR), "# User's Own\n").expect("descriptor");

    let report = port_skill_directories(&source, &target).expect("port");

    assert!(report.copied.is_empty());
    assert!(report.overwritten.is_empty());
    assert_eq!(report.kept_unmanaged.len(), 1);
    assert_eq!(report.kept_unmanaged[0].name, "repo-map");
    assert_eq!(
        std::fs::read_to_string(user_skill.join(SKILL_DESCRIPTOR)).expect("descriptor"),
        "# User's Own\n"
    );
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
