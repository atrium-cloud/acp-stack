use crate::common::cli::*;
use acp_stack::config::load_config_from_str;
use acp_stack::dev_gates::TEST_SKIP_AGENT_INSTALL_ENV;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[test]
fn init_custom_agent_writes_install_config() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    // Completing init at all proves the registry-only gates were bypassed:
    // `should_install_agent` would otherwise fail `lookup_required` on a
    // non-registry id even when agent install is fixture-skipped.
    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--custom-agent-id",
            "my-agent",
            "--custom-agent-name",
            "My Agent",
            "--custom-agent-command",
            "my-agent-bin",
            "--custom-agent-arg",
            "acp",
            "--custom-agent-install",
            "echo install my-agent",
            "--custom-agent-creates",
            "my-agent-bin",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("custom agent config should be readable");
    let config = load_config_from_str(&written).expect("custom agent config should validate");
    assert_eq!(config.agent.id, "my-agent");
    assert_eq!(config.agent.name, "My Agent");
    assert_eq!(config.agent.command, "my-agent-bin");
    assert_eq!(config.agent.args, vec!["acp".to_owned()]);
    let install = config
        .agent
        .install
        .as_ref()
        .expect("custom agent must write an [agent.install] escape hatch");
    assert_eq!(install.install_type, "shell");
    assert_eq!(install.creates, "my-agent-bin");
    assert_eq!(install.shell.as_deref(), Some("echo install my-agent"));
    // The custom agent block must round-trip canonical TOML.
    let canonical = config
        .to_canonical_toml()
        .expect("custom agent config should round-trip canonical TOML");
    assert!(canonical.contains("[array.targets.agent.install]"));
}

#[test]
fn init_custom_agent_rejects_placeholder_id() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--custom-agent-id",
            "placeholder",
            "--custom-agent-command",
            "x",
            "--custom-agent-install",
            "echo x",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("placeholder"));

    assert!(
        !tempdir
            .path()
            .join(".config/acp-stack/acps-config.toml")
            .exists(),
        "a rejected custom agent must not leave a config on disk"
    );
}

#[test]
fn init_custom_agent_rejects_registry_id() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--custom-agent-id",
            "placebo",
            "--custom-agent-command",
            "x",
            "--custom-agent-install",
            "echo x",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--agent placebo"));

    assert!(
        !tempdir
            .path()
            .join(".config/acp-stack/acps-config.toml")
            .exists(),
        "a rejected custom registry id must not leave a config on disk"
    );
}

#[test]
fn init_custom_agent_requires_command() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--custom-agent-id",
            "my-agent",
            "--custom-agent-install",
            "echo x",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--custom-agent-command"));
}

#[test]
fn init_custom_agent_rejects_blank_command() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--custom-agent-id",
            "my-agent",
            "--custom-agent-command",
            "   ",
            "--custom-agent-install",
            "echo x",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--custom-agent-command"));
}

#[test]
fn init_custom_agent_rejects_explicit_model_flag_on_rerun() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--custom-agent-id",
            "my-agent",
            "--custom-agent-command",
            "my-agent-bin",
            "--custom-agent-install",
            "echo install",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--model",
            "some-model",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--model"));
}

#[test]
fn init_custom_agent_rejects_explicit_mode_flag_on_rerun() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--custom-agent-id",
            "my-agent",
            "--custom-agent-command",
            "my-agent-bin",
            "--custom-agent-install",
            "echo install",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--mode",
            "review",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--mode"));
}

#[test]
fn init_custom_agent_allows_explicit_registry_agent_switch_on_rerun() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--custom-agent-id",
            "my-agent",
            "--custom-agent-command",
            "my-agent-bin",
            "--custom-agent-install",
            "echo install",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

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
        .success();

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    let config = load_config_from_str(&written).expect("config should validate");
    assert_eq!(config.agent.id, "placebo");
    assert!(
        config.agent.install.is_none(),
        "switching to a registry agent should clear custom install config"
    );
}

#[cfg(unix)]
#[test]
fn init_custom_agent_fails_when_installed_command_is_absent() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let workspace = tempdir.path().join("ws");
    fs::create_dir_all(&workspace).expect("workspace dir should be created");
    let creates = tempdir.path().join("custom-agent-marker");
    fs::write(&creates, "#!/bin/sh\nexit 0\n").expect("creates marker should be written");
    let mut permissions = fs::metadata(&creates)
        .expect("creates marker metadata should be readable")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&creates, permissions).expect("creates marker should be executable");

    let output = acps_command()
        .env("HOME", tempdir.path())
        .env_remove(TEST_SKIP_AGENT_INSTALL_ENV)
        .args([
            "dev",
            "init",
            "--custom-agent-id",
            "my-agent",
            "--custom-agent-command",
            "acpstack-missing-custom-command",
            "--custom-agent-install",
            "true",
            "--custom-agent-creates",
            creates.to_str().expect("creates path should be utf8"),
            "--workspace-root",
            workspace.to_str().expect("workspace path should be utf8"),
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(output).expect("stderr should be utf8");
    assert!(
        stderr.contains("did not resolve after custom agent install"),
        "{stderr}"
    );
}

#[cfg(unix)]
#[test]
fn init_custom_agent_acp_gate_skips_when_spawn_cwd_absent() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let workspace = tempdir.path().join("missing-workspace");
    let creates = tempdir.path().join("custom-agent-marker");
    fs::write(&creates, "#!/bin/sh\nexit 0\n").expect("creates marker should be written");
    let mut permissions = fs::metadata(&creates)
        .expect("creates marker metadata should be readable")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&creates, permissions).expect("creates marker should be executable");

    acps_command()
        .env("HOME", tempdir.path())
        .env_remove(TEST_SKIP_AGENT_INSTALL_ENV)
        .args([
            "dev",
            "init",
            "--custom-agent-id",
            "my-agent",
            "--custom-agent-command",
            "bin/my-agent",
            "--custom-agent-install",
            "true",
            "--custom-agent-creates",
            creates.to_str().expect("creates path should be utf8"),
            "--workspace-root",
            workspace.to_str().expect("workspace path should be utf8"),
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("spawn cwd"));
}

#[test]
fn init_agent_env_ref_appends_to_config() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    // Pre-seed the referenced secret; `--agent-env-ref` references an existing
    // secret and fails fast otherwise.
    seed_init_secrets(tempdir.path(), &[("MY_AGENT_TOKEN", "token-value")]);

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--agent-env-ref",
            "MY_AGENT_TOKEN",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    let config = load_config_from_str(&written).expect("config should validate");
    assert!(
        config.agent.env.contains(&"MY_AGENT_TOKEN".to_owned()),
        "agent.env should contain the operator env ref, got {:?}",
        config.agent.env
    );
}

#[test]
fn init_agent_env_ref_missing_secret_fails_fast() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    let output = acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--agent-env-ref",
            "MISSING_TOKEN",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(output).expect("stderr should be utf8");
    assert!(
        stderr.contains("secret `MISSING_TOKEN` was not found in the secret store"),
        "{stderr}"
    );
    // The ref must NOT be persisted to agent.env when verification fails, or a
    // later `--resume` would complete with an unresolved env ref.
    let config_path = tempdir.path().join(".config/acp-stack/acps-config.toml");
    if config_path.is_file() {
        let written = fs::read_to_string(&config_path).expect("config should be readable");
        let config = load_config_from_str(&written).expect("config should validate");
        assert!(
            !config.agent.env.contains(&"MISSING_TOKEN".to_owned()),
            "a failed env-ref verification must not persist the ref: {:?}",
            config.agent.env
        );
    }
}

#[test]
fn init_agent_env_ref_rejected_for_existing_config() {
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
        .success();

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--agent-env-ref",
            "MY_AGENT_TOKEN",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--agent-env-ref"));
}

#[test]
fn init_custom_agent_acp_gate_skips_when_binary_absent() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    // The custom binary is not on PATH (install is fixture-skipped), so the
    // connection gate skips cleanly and init still completes.
    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--custom-agent-id",
            "ghost",
            "--custom-agent-command",
            "acpstack-ghost-binary",
            "--custom-agent-install",
            "echo install",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("acp connection check skipped"));
}

#[test]
fn init_custom_agent_acp_gate_fails_for_non_acp_binary() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let workspace = tempdir.path().join("ws");
    std::fs::create_dir_all(&workspace).expect("workspace dir should be created");

    // `true` is a real binary on PATH but does not speak ACP, so the gate runs
    // and surfaces a connection failure instead of completing init.
    let output = acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--custom-agent-id",
            "t",
            "--custom-agent-command",
            "true",
            "--custom-agent-install",
            "echo install",
            "--workspace-root",
            workspace.to_str().expect("workspace path should be utf8"),
            "--skip-workspace-init",
            "--skip-testflight",
        ])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(output).expect("stderr should be utf8");
    assert!(
        stderr.contains("failed to complete an ACP session"),
        "{stderr}"
    );
}
