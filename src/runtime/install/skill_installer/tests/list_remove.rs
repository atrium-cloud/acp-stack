use super::super::*;
use super::support::*;

#[test]
fn list_installed_skills_empty_when_root_missing() {
    let home = tempfile::tempdir().expect("home");
    let catalog = RegistryCatalog::load_embedded().expect("registry");
    let home_path = canonical_temp_home(&home);

    let skills = list_installed_skills(&home_path, opencode_entry(&catalog)).expect("list");

    assert!(skills.is_empty());
}

#[test]
fn list_installed_skills_returns_sorted_flat_and_nested() {
    let home = tempfile::tempdir().expect("home");
    let catalog = RegistryCatalog::load_embedded().expect("registry");
    let home_path = canonical_temp_home(&home);
    let install_root = home_path.join(".agents/skills");
    write_installed_skill(&install_root, "repo-map", "# Repo Map\n");
    write_installed_skill(&install_root, "code-review", "# Code Review\n");
    write_installed_skill(&install_root, "contact-center/android", "# Android\n");

    let skills = list_installed_skills(&home_path, opencode_entry(&catalog)).expect("list");

    let names = skills.iter().map(|s| s.name.as_str()).collect::<Vec<_>>();
    assert_eq!(names, ["code-review", "contact-center/android", "repo-map"]);
    assert_eq!(skills[2].path, install_root.join("repo-map"));
    assert!(
        skills
            .iter()
            .all(|skill| skill.source.as_deref() == Some("test-source"))
    );
}

#[test]
fn list_installed_skills_source_absent_for_unmanaged_or_empty_marker() {
    let home = tempfile::tempdir().expect("home");
    let catalog = RegistryCatalog::load_embedded().expect("registry");
    let home_path = canonical_temp_home(&home);
    let install_root = home_path.join(".agents/skills");
    // Hand-placed skill: regular SKILL.md, no managed marker.
    let unmanaged = install_root.join("hand-made");
    std::fs::create_dir_all(&unmanaged).expect("skill dir");
    std::fs::write(unmanaged.join(SKILL_DESCRIPTOR), "# Mine\n").expect("descriptor");
    // Managed skill whose marker carries no source id.
    write_installed_skill(&install_root, "blank-marker", "# Blank\n");
    std::fs::write(
        install_root.join("blank-marker").join(MANAGED_SKILL_MARKER),
        "\n",
    )
    .expect("blank marker");
    write_installed_skill(&install_root, "managed", "# Managed\n");

    let skills = list_installed_skills(&home_path, opencode_entry(&catalog)).expect("list");

    let sources = skills
        .iter()
        .map(|skill| (skill.name.as_str(), skill.source.as_deref()))
        .collect::<Vec<_>>();
    assert_eq!(
        sources,
        [
            ("blank-marker", None),
            ("hand-made", None),
            ("managed", Some("test-source")),
        ]
    );
}

#[test]
#[cfg(unix)]
fn list_installed_skills_follows_symlinked_root() {
    let home = tempfile::tempdir().expect("home");
    let catalog = RegistryCatalog::load_embedded().expect("registry");
    let home_path = canonical_temp_home(&home);
    // Dotfiles-style setup: `~/.agents` is a symlink and listing must follow it.
    let real_agents = home_path.join("dotfiles/agents");
    write_installed_skill(&real_agents.join("skills"), "repo-map", "# Repo Map\n");
    std::os::unix::fs::symlink(&real_agents, home_path.join(".agents")).expect("symlink");

    let skills = list_installed_skills(&home_path, opencode_entry(&catalog)).expect("list");

    let names = skills.iter().map(|s| s.name.as_str()).collect::<Vec<_>>();
    assert_eq!(names, ["repo-map"]);
}

#[test]
fn remove_agent_skill_removes_flat_skill() {
    let home = tempfile::tempdir().expect("home");
    let catalog = RegistryCatalog::load_embedded().expect("registry");
    let home_path = canonical_temp_home(&home);
    let install_root = home_path.join(".agents/skills");
    write_installed_skill(&install_root, "repo-map", "# Repo Map\n");
    write_installed_skill(&install_root, "code-review", "# Code Review\n");

    let report =
        remove_agent_skill(&home_path, opencode_entry(&catalog), "repo-map").expect("remove");

    assert_eq!(report.removed.name, "repo-map");
    assert!(!install_root.join("repo-map").exists());
    assert!(
        install_root
            .join("code-review")
            .join(SKILL_DESCRIPTOR)
            .is_file()
    );
}

#[test]
fn remove_agent_skill_cleans_emptied_group_parent() {
    let home = tempfile::tempdir().expect("home");
    let catalog = RegistryCatalog::load_embedded().expect("registry");
    let home_path = canonical_temp_home(&home);
    let install_root = home_path.join(".agents/skills");
    write_installed_skill(&install_root, "contact-center/android", "# Android\n");

    remove_agent_skill(
        &home_path,
        opencode_entry(&catalog),
        "contact-center/android",
    )
    .expect("remove");

    assert!(!install_root.join("contact-center/android").exists());
    assert!(!install_root.join("contact-center").exists());
    assert!(install_root.is_dir());
}

#[test]
fn remove_agent_skill_keeps_group_dir_with_siblings() {
    let home = tempfile::tempdir().expect("home");
    let catalog = RegistryCatalog::load_embedded().expect("registry");
    let home_path = canonical_temp_home(&home);
    let install_root = home_path.join(".agents/skills");
    write_installed_skill(&install_root, "zoom/android", "# Android\n");
    write_installed_skill(&install_root, "zoom/desktop", "# Desktop\n");

    remove_agent_skill(&home_path, opencode_entry(&catalog), "zoom/android").expect("remove");

    assert!(!install_root.join("zoom/android").exists());
    assert!(
        install_root
            .join("zoom/desktop")
            .join(SKILL_DESCRIPTOR)
            .is_file()
    );
}

#[test]
fn remove_agent_skill_missing_is_not_installed() {
    let home = tempfile::tempdir().expect("home");
    let catalog = RegistryCatalog::load_embedded().expect("registry");
    let home_path = canonical_temp_home(&home);
    std::fs::create_dir_all(home_path.join(".agents/skills")).expect("root");

    let err = remove_agent_skill(&home_path, opencode_entry(&catalog), "missing")
        .expect_err("missing skill");

    assert!(matches!(err, StackError::SkillNotInstalled { .. }));
}

#[test]
fn remove_agent_skill_conflicts_on_directory_without_descriptor() {
    // The runtime must never delete a directory it did not install.
    let home = tempfile::tempdir().expect("home");
    let catalog = RegistryCatalog::load_embedded().expect("registry");
    let home_path = canonical_temp_home(&home);
    let install_root = home_path.join(".agents/skills");
    std::fs::create_dir_all(install_root.join("scratch")).expect("scratch dir");

    let err =
        remove_agent_skill(&home_path, opencode_entry(&catalog), "scratch").expect_err("conflict");

    assert!(matches!(err, StackError::SkillInstallTargetConflict { .. }));
    assert!(install_root.join("scratch").is_dir());
}

#[test]
fn remove_agent_skill_refuses_skill_not_installed_by_acp_stack() {
    // A hand-placed folder looks like an installed skill but has no managed
    // marker; removal must refuse it and leave every byte in place.
    let home = tempfile::tempdir().expect("home");
    let catalog = RegistryCatalog::load_embedded().expect("registry");
    let home_path = canonical_temp_home(&home);
    let install_root = home_path.join(".agents/skills");
    let skill_dir = install_root.join("my-skill");
    std::fs::create_dir_all(&skill_dir).expect("skill dir");
    std::fs::write(skill_dir.join(SKILL_DESCRIPTOR), "# Mine\n").expect("descriptor");
    std::fs::write(skill_dir.join("notes.txt"), "user content\n").expect("notes");

    let err = remove_agent_skill(&home_path, opencode_entry(&catalog), "my-skill")
        .expect_err("unmanaged skill refused");

    assert!(matches!(err, StackError::SkillInstallTargetConflict { .. }));
    assert!(skill_dir.join(SKILL_DESCRIPTOR).is_file());
    assert!(skill_dir.join("notes.txt").is_file());
}

#[test]
fn remove_and_list_reject_agent_without_skills_support() {
    let home = tempfile::tempdir().expect("home");
    let catalog = RegistryCatalog::load_embedded().expect("registry");
    let home_path = canonical_temp_home(&home);
    let mut entry = opencode_entry(&catalog).clone();
    entry.supports_agent_skills = false;

    let skills = list_installed_skills(&home_path, &entry).expect("list");
    assert!(skills.is_empty());

    let err = remove_agent_skill(&home_path, &entry, "repo-map").expect_err("unsupported agent");
    assert!(matches!(err, StackError::SkillInstallFailed { .. }));
}

#[test]
#[cfg(unix)]
fn remove_agent_skill_rejects_symlinked_target() {
    let home = tempfile::tempdir().expect("home");
    let catalog = RegistryCatalog::load_embedded().expect("registry");
    let home_path = canonical_temp_home(&home);
    let install_root = home_path.join(".agents/skills");
    std::fs::create_dir_all(&install_root).expect("root");
    let external = tempfile::tempdir().expect("external");
    std::fs::write(external.path().join(SKILL_DESCRIPTOR), "# Skill\n").expect("descriptor");
    std::os::unix::fs::symlink(external.path(), install_root.join("repo-map")).expect("symlink");

    let err = remove_agent_skill(&home_path, opencode_entry(&catalog), "repo-map")
        .expect_err("symlinked target");

    assert!(matches!(err, StackError::SkillInstallTargetConflict { .. }));
    // The symlink target's descriptor must be intact: nothing was deleted.
    assert!(external.path().join(SKILL_DESCRIPTOR).is_file());
}
