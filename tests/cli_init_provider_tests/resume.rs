use std::fs;

use predicates::prelude::PredicateBooleanExt as _;

use crate::common::cli::*;

#[test]
fn init_resume_restores_recorded_edge_request_before_edge_step_exists() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    let output = acps_command(tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "opencode",
            "--provider",
            "openai",
            "--edge",
            "cloudflare",
            "--exposure",
            "tunnel",
            "--hostname",
            "agent.example.com",
            "--cloudflared-deployment",
            "external",
            "--no-skills",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(output).expect("stderr should be utf8");
    let run_id = stderr
        .lines()
        .find_map(|line| line.strip_prefix("init failed in run "))
        .expect("stderr should include failed init run id");

    seed_init_secrets(tempdir.path(), &[("OPENAI_API_KEY", "test-openai-key")]);

    acps_command(tempdir.path())
        .args(["init", "--resume", "--run-id", run_id])
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
fn init_resume_with_nothing_to_resume_writes_no_placeholder_config() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command(tempdir.path())
        .args(["init", "--resume"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("no resumable init run found"));

    assert!(
        !tempdir
            .path()
            .join(".config/acp-stack/acps-config.toml")
            .exists(),
        "a failed --resume must not leave a starter config on disk"
    );
}

#[test]
fn init_resume_restores_recorded_provider_args_before_provider_step_exists() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir");
    let workspace = tempdir.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace");
    let local_bin = tempdir.path().join(".local/bin");
    let managed_opencode = local_bin.join("opencode");
    fs::write(
        config_dir.join("agents.toml"),
        format!(
            r#"
[[agents]]
id = "opencode"
name = "OpenCode"
kind = "native"
headless_compatible = true
set_provider = true
set_model = true
allow_custom_provider = true
allow_custom_model = true
set_mode = true
support_doc = "docs/agents/opencode.md"

[agents.harness]
id = "opencode"

[agents.harness.install.shell]
script = "exit 9"
creates = {}
"#,
            toml_string(&managed_opencode.to_string_lossy()),
        ),
    )
    .expect("agents override");

    let output = acps_command(tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "opencode",
            "--provider",
            "myprovider",
            "--custom-provider",
            "--provider-name",
            "My Provider",
            "--base-url",
            "https://api.myprovider.example/v1",
            "--api-key-ref",
            "MY_PROVIDER_API_KEY",
            "--model",
            "my-model",
            "--model-name",
            "My Model",
            "--workspace-root",
            workspace.to_str().expect("workspace UTF-8"),
            "--no-skills",
            "--skip-workspace-init",
            "--skip-testflight",
        ])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(output).expect("stderr should be utf8");
    let run_id = stderr
        .lines()
        .find_map(|line| line.strip_prefix("init failed in run "))
        .expect("stderr should include failed init run id");
    let config_before =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    assert!(!config_before.contains("[array.targets.agent.provider]"));

    fs::write(
        config_dir.join("agents.toml"),
        format!(
            r#"
[[agents]]
id = "opencode"
name = "OpenCode"
kind = "native"
headless_compatible = true
set_provider = true
set_model = true
allow_custom_provider = true
allow_custom_model = true
set_mode = true
support_doc = "docs/agents/opencode.md"

[agents.harness]
id = {}

[agents.harness.install.shell]
script = "true"
creates = "opencode"
"#,
            toml_string(env!("CARGO_BIN_EXE_placebo-agent")),
        ),
    )
    .expect("agents override");
    seed_init_secrets(
        tempdir.path(),
        &[("MY_PROVIDER_API_KEY", "test-provider-key")],
    );

    acps_command(tempdir.path())
        .args(["init", "--resume", "--run-id", run_id])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "workspace: skipped (--skip-workspace-init)",
        ));

    let config_after =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    assert!(config_after.contains("[array.targets.agent.provider]"));
    assert!(config_after.contains(r#"id = "myprovider""#));
    assert!(config_after.contains("[array.targets.agent.provider.custom]"));
    assert!(config_after.contains(r#"name = "My Provider""#));
    assert!(config_after.contains(r#"api_key_ref = "MY_PROVIDER_API_KEY""#));
    assert!(config_after.contains(r#"base_url = "https://api.myprovider.example/v1""#));
    assert!(config_after.contains(r#"model_name = "My Model""#));
}

#[test]
fn init_resume_restores_recorded_skip_testflight_before_testflight_step_exists() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    let output = acps_command(tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "opencode",
            "--provider",
            "openai",
            "--no-skills",
            "--skip-workspace-init",
            "--skip-testflight",
        ])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(output).expect("stderr should be utf8");
    let run_id = stderr
        .lines()
        .find_map(|line| line.strip_prefix("init failed in run "))
        .expect("stderr should include failed init run id");

    seed_init_secrets(tempdir.path(), &[("OPENAI_API_KEY", "test-openai-key")]);

    acps_command(tempdir.path())
        .args(["init", "--resume", "--run-id", run_id])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "testflight: skipped (--skip-testflight)",
        ))
        .stdout(
            predicates::str::contains(
                "testflight: skipped (non-interactive run; pass --testflight to opt in)",
            )
            .not(),
        );
}

#[test]
fn init_resume_restores_recorded_testflight_before_testflight_step_exists() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    let output = acps_command(tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "opencode",
            "--provider",
            "openai",
            "--no-skills",
            "--skip-workspace-init",
            "--testflight",
        ])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(output).expect("stderr should be utf8");
    let run_id = stderr
        .lines()
        .find_map(|line| line.strip_prefix("init failed in run "))
        .expect("stderr should include failed init run id");

    seed_init_secrets(tempdir.path(), &[("OPENAI_API_KEY", "test-openai-key")]);

    let output = acps_command(tempdir.path())
        .args(["init", "--resume", "--run-id", run_id])
        .assert()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("stdout should be utf8");
    assert!(
        stdout.contains("this may consume provider credits."),
        "{stdout}"
    );
    assert!(
        !stdout.contains("testflight: skipped (non-interactive run; pass --testflight to opt in)"),
        "{stdout}"
    );
}
