use super::super::*;
use super::support::*;

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
