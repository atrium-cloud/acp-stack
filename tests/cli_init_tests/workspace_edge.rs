use crate::common::cli::*;
use acp_stack::config::load_config_from_str;
use acp_stack::state::{StateStore, default_state_path};
use predicates::prelude::PredicateBooleanExt as _;
use std::fs;

#[test]
fn init_default_skips_testflight_under_non_interactive_runs() {
    // Non-interactive default with a registered agent: no --testflight, no
    // --skip-testflight, no stdin TTY. The runner should announce the skip
    // rather than silently continue — operators reading the log need to see
    // why testflight was not run.
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "testflight: skipped (non-interactive run; pass --testflight to opt in)",
        ));
}

#[test]
fn init_skip_testflight_flag_is_acknowledged_in_output() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "testflight: skipped (--skip-testflight)",
        ));
}

#[test]
fn init_creates_workspace_root_and_uploads_without_sources() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let workspace_root = tempdir.path().join("workspace");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "init",
            "--agent",
            "placebo",
            "--no-skills",
            "--skip-testflight",
            "--workspace-root",
            workspace_root.to_str().expect("workspace UTF-8"),
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "progress: materializing workspace sources",
        ))
        .stdout(predicates::str::contains("workspace root:"))
        .stdout(predicates::str::contains("workspace uploads:"));

    assert!(workspace_root.is_dir());
    assert!(workspace_root.join("uploads").is_dir());
}

#[test]
fn init_prepares_workspace_root_before_agent_install() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let workspace_root = tempdir.path().join("workspace");
    let managed_binary = tempdir.path().join(".local/bin/cwd-agent");
    let shell = format!(
        "test \"$(pwd -P)\" = \"$(cd {workspace} && pwd -P)\" && mkdir -p {bin} && printf '#!/bin/sh\\necho cwd-agent\\n' > {binary} && chmod 755 {binary}",
        workspace = shell_quote_path(&workspace_root),
        bin = shell_quote_path(managed_binary.parent().expect("binary has parent")),
        binary = shell_quote_path(&managed_binary),
    );
    fs::write(
        config_dir.join("agents.toml"),
        format!(
            r#"
[[agents]]
id = "cwd-agent"
name = "CWD Agent"
kind = "native"
headless_compatible = true
set_provider = false
set_model = false
allow_custom_provider = false
allow_custom_model = false
set_mode = false
support_doc = "docs/agents/cwd-agent.md"

[agents.harness]
id = "cwd-agent"

[agents.harness.install.shell]
script = {}
creates = "cwd-agent"
"#,
            toml_string(&shell),
        ),
    )
    .expect("agents override should be written");

    acps_command_without_placebo()
        .env("HOME", tempdir.path())
        .args([
            "init",
            "--agent",
            "cwd-agent",
            "--no-skills",
            "--skip-testflight",
            "--workspace-root",
            workspace_root.to_str().expect("workspace UTF-8"),
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "progress: materializing workspace sources",
        ));

    assert!(workspace_root.is_dir());
    assert!(workspace_root.join("uploads").is_dir());
    assert!(managed_binary.is_file());
    let store = StateStore::open(default_state_path(tempdir.path())).expect("state should open");
    let runs = store
        .query_installer_runs_filtered(Some("cwd-agent"), 10)
        .expect("installer history should query");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, "ran");
    assert_eq!(runs[0].step, "install");
}

#[test]
fn init_edge_profile_prints_edge_artifact_progress() {
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
            "--edge",
            "cloudflare",
            "--exposure",
            "tunnel",
            "--hostname",
            "agent.example.com",
            "--cloudflared-deployment",
            "external",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "progress: preparing Cloudflare edge artifacts",
        ))
        .stdout(predicates::str::contains(
            "workspace: skipped (--skip-workspace-init)",
        ))
        .stdout(predicates::str::contains("progress: materializing workspace sources").not());

    assert!(
        tempdir
            .path()
            .join(".config/acp-stack/cloudflared/config.yml")
            .is_file()
    );
    assert!(!tempdir.path().join("workspace").exists());
}

#[test]
fn init_skip_workspace_init_is_acknowledged_in_output() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let workspace_root = tempdir.path().join("workspace");

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
            "--workspace-root",
            workspace_root.to_str().expect("workspace UTF-8"),
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "workspace: skipped (--skip-workspace-init)",
        ))
        .stdout(predicates::str::contains("progress: materializing workspace sources").not());

    assert!(!workspace_root.exists());
}

#[test]
fn init_rejects_skip_workspace_init_outside_dev_mode() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["init", "--skip-workspace-init"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--skip-workspace-init"))
        .stderr(predicates::str::contains(
            "acps dev init --skip-workspace-init",
        ));
}

#[test]
fn init_noninteractive_without_agent_fails_before_writing_config() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["init", "--non-interactive"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "non-interactive init requires selecting a real agent",
        ));

    assert!(
        !tempdir
            .path()
            .join(".config/acp-stack/acps-config.toml")
            .exists(),
        "failed non-interactive init without --agent must not write starter config"
    );
}

#[test]
fn init_help_hides_dev_only_workspace_skip() {
    acps_command()
        .args(["init", "--help"])
        .assert()
        .success()
        .stdout(predicates::str::contains("--skip-workspace-init").not());
}

#[test]
fn dev_init_help_shows_workspace_skip() {
    acps_command()
        .args(["dev", "init", "--help"])
        .assert()
        .success()
        .stdout(predicates::str::contains("--skip-workspace-init"));
}

#[test]
fn serve_help_hides_allow_root_outside_dev_command() {
    acps_command()
        .args(["serve", "--help"])
        .assert()
        .success()
        .stdout(predicates::str::contains("--allow-root").not());
}

#[test]
fn dev_serve_help_shows_allow_root() {
    acps_command()
        .args(["dev", "serve", "--help"])
        .assert()
        .success()
        .stdout(predicates::str::contains("--allow-root"));
}

#[test]
fn serve_rejects_dev_only_root_overrides() {
    let tempdir = tempfile::tempdir().expect("tempdir");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["serve", "--allow-root"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "development-only flag; use `acps dev serve --allow-root`",
        ));

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_ALLOW_ROOT", "1")
        .args(["serve"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "development-only environment override; use `acps dev serve`",
        ));
}

#[test]
fn init_rejects_combining_testflight_and_skip_testflight() {
    // clap conflicts_with should fail at parse time, so init never starts.
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["init", "--testflight", "--skip-testflight"])
        .assert()
        .failure();
}

#[test]
fn init_explicit_testflight_prints_provider_credit_warning() {
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
            "--testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .stdout(predicates::str::contains(
            "this may consume provider credits.",
        ));
}

#[test]
fn init_writes_deployment_controlled_workspace_defaults() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--workspace-root",
            "/srv/acp",
            "--workspace-uploads",
            "/srv/acp/uploads",
            "--runtime-user",
            "svc-acp",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("starter config should be readable");
    let config = load_config_from_str(&written).expect("starter config should validate");
    assert_eq!(config.workspace.root, "/srv/acp");
    assert_eq!(config.workspace.uploads, "/srv/acp/uploads");
    assert_eq!(config.workspace.runtime_user, "svc-acp");
    assert_eq!(config.agent.cwd.as_deref(), Some("/srv/acp"));
}

#[test]
fn init_rejects_conflicting_deployment_overrides_for_existing_config() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success();

    acps_command()
        .env("HOME", tempdir.path())
        .args(["init", "--workspace-root", "/srv/acp"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "query parameter `--workspace-root` is invalid",
        ))
        .stderr(predicates::str::contains(
            "deployment override applies only when creating a starter config",
        ));
}
