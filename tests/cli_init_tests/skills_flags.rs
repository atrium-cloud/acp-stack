use crate::common::cli::*;

#[test]
fn init_no_skills_flag_skips_skill_install_prompt() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--no-skills",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("initialized acp-stack"));
}

#[test]
fn init_rejects_skills_without_source() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["init", "--skills", "repo-map"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--skills-source"));
}

#[test]
fn init_rejects_source_without_skills() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["init", "--skills-source", "openai"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--skills"));
}

#[test]
fn init_rejects_removed_plugins_flag() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["init", "--plugins", "cloudflare"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("unexpected argument '--plugins'"));
}

#[test]
fn init_rejects_removed_plugins_source_flag() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["init", "--plugins-source", "openai"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "unexpected argument '--plugins-source'",
        ));
}

#[test]
fn init_validates_skill_names_before_download() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    seed_init_secrets(tempdir.path(), &[("OPENAI_API_KEY", "test-openai-key")]);

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "opencode",
            "--provider",
            "openai",
            "--skip-testflight",
            "--skip-workspace-init",
            "--skills-source",
            "openai",
            "--skills",
            "BadSkill",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("invalid skill name"));
}
