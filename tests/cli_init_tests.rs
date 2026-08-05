#![cfg(all(feature = "dev-tools", feature = "test-fixtures"))]

use acp_stack::config::{
    DependencyInstallScope, McpServerConfig, StackUpdatePolicy, load_config_from_str,
};
use acp_stack::dev_gates::TEST_SKIP_AGENT_INSTALL_ENV;
use acp_stack::state::{StateStore, default_state_path};
use predicates::prelude::PredicateBooleanExt as _;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

mod common;
use common::cli::*;

#[test]
fn init_creates_config_and_state() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    let mut command = acps_command();

    command
        .env("HOME", tempdir.path())
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success()
        .stdout(predicates::str::contains("progress: initializing auth"))
        .stdout(predicates::str::contains("initialized acp-stack"));

    let config_path = tempdir.path().join(".config/acp-stack/acps-config.toml");
    let state_path = tempdir.path().join(".local/share/acp-stack/state.sqlite");
    assert!(config_path.is_file());
    assert!(state_path.is_file());

    let config = fs::read_to_string(config_path).expect("starter config should be readable");
    assert!(
        !config.contains("[workspace.source]"),
        "starter config must not retain the legacy single-source block"
    );
    assert!(
        !config.contains("[[workspace.code_sources]]")
            && !config.contains("[[workspace.data_sources]]"),
        "starter config should declare no sources by default"
    );
}

#[test]
fn init_writes_mcp_declarations_to_starter_config() {
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
            "--mcp-preset",
            "linear",
            "--mcp-stdio",
            "local=local-mcp",
            "--mcp-stdio-env",
            "local=LOCAL_MCP_TOKEN",
            "--mcp-http",
            "remote=https://mcp.example/mcp",
            "--mcp-http-header",
            "remote=Authorization:REMOTE_MCP_TOKEN",
        ])
        .assert()
        .success();

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("starter config should be readable");
    let config = load_config_from_str(&written).expect("starter config should validate");
    assert_eq!(config.mcp.servers.len(), 3);
    let linear = config
        .mcp
        .servers
        .iter()
        .find(|server| server.name() == "linear")
        .expect("linear preset should be written");
    let McpServerConfig::Http(linear) = linear else {
        panic!("linear preset should be an HTTP MCP server");
    };
    assert_eq!(linear.url, "https://mcp.linear.app/mcp");
    assert_eq!(linear.headers.len(), 1);
    assert_eq!(linear.headers[0].name, "Authorization");
    assert_eq!(
        linear.headers[0].value_ref.as_deref(),
        Some("LINEAR_API_KEY")
    );

    let local = config
        .mcp
        .servers
        .iter()
        .find(|server| server.name() == "local")
        .expect("custom stdio server should be written");
    let McpServerConfig::Stdio(local) = local else {
        panic!("local MCP server should be stdio");
    };
    assert_eq!(local.command, "local-mcp");
    assert!(local.args.is_empty());
    assert_eq!(local.env, vec!["LOCAL_MCP_TOKEN"]);

    let remote = config
        .mcp
        .servers
        .iter()
        .find(|server| server.name() == "remote")
        .expect("custom HTTP server should be written");
    let McpServerConfig::Http(remote) = remote else {
        panic!("remote MCP server should be HTTP");
    };
    assert_eq!(remote.url, "https://mcp.example/mcp");
    assert_eq!(remote.headers.len(), 1);
    assert_eq!(remote.headers[0].name, "Authorization");
    assert_eq!(
        remote.headers[0].value_ref.as_deref(),
        Some("REMOTE_MCP_TOKEN")
    );
}

#[test]
fn init_rejects_removed_startup_script_flag() {
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
            "--startup-script",
            "bootstrap=echo ready",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--startup-script"));
}

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
fn init_dep_flag_writes_user_scope_dependency() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--dep",
            "ripgrep=apt-get install -y ripgrep",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    let config = load_config_from_str(&written).expect("config should validate");
    let entry = config
        .dependencies
        .commands
        .iter()
        .find(|entry| entry.name == "ripgrep")
        .expect("ripgrep dependency should be declared");
    let install = entry
        .install
        .as_ref()
        .expect("dep should have install action");
    assert_eq!(install.shell, "apt-get install -y ripgrep");
    assert_eq!(install.scope, DependencyInstallScope::User);
}

#[test]
fn init_dep_system_flag_writes_system_scope() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--dep-system",
            "nginx=apt-get install -y nginx",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    let config = load_config_from_str(&written).expect("config should validate");
    let install = config
        .dependencies
        .commands
        .iter()
        .find(|entry| entry.name == "nginx")
        .and_then(|entry| entry.install.as_ref())
        .expect("nginx dependency should be declared with an install action");
    assert_eq!(install.scope, DependencyInstallScope::System);
}

#[test]
fn init_deps_apply_requires_yes_noninteractive() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--dep",
            "acpstack-absent-tool=true",
            "--deps-apply",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--deps-apply-yes"));
}

#[test]
fn init_deps_apply_runs_pending_action_and_surfaces_failure() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    // The tool is not on PATH (pending), so the apply step runs its shell,
    // which exits non-zero — proving the step executes and surfaces failure.
    let output = acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--dep",
            "acpstack-failtool=exit 3",
            "--deps-apply",
            "--deps-apply-yes",
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
        stderr.contains("dependency apply produced failing actions"),
        "{stderr}"
    );
    assert!(
        stderr.contains("acpstack-failtool failed (exit=3)"),
        "{stderr}"
    );
}

#[test]
fn init_deps_apply_skips_system_scope_without_sudo_and_continues() {
    // SAFETY: `geteuid()` is always safe — no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        // As root the escalation probe short-circuits to "run directly"
        // and the skip path under test is unreachable.
        return;
    }
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    // Deterministic "no passwordless sudo": prepend a fake sudo that always
    // exits 1 (as if a password were required), so the escalation probe
    // resolves it and collapses to Unavailable regardless of the host's
    // real sudoers state. The rest of PATH stays intact for the init run.
    let fake_bin = tempdir.path().join("fake-bin");
    fs::create_dir_all(&fake_bin).expect("fake bin dir");
    let fake_sudo = fake_bin.join("sudo");
    fs::write(&fake_sudo, "#!/bin/sh\nexit 1\n").expect("fake sudo");
    #[cfg(unix)]
    fs::set_permissions(&fake_sudo, fs::Permissions::from_mode(0o755)).expect("chmod fake sudo");
    let host_path = std::env::var("PATH").expect("PATH should be set");
    let path_with_fake_sudo = format!("{}:{host_path}", fake_bin.to_string_lossy());

    // The system-scope action would succeed if it ran; the point is that it
    // must NOT run — it is skipped on privilege and init still completes.
    acps_command()
        .env("HOME", tempdir.path())
        .env("PATH", path_with_fake_sudo)
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--dep-system",
            "acpstack-absent-system-tool=exit 0",
            "--deps-apply",
            "--deps-apply-yes",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "no passwordless sudo; they will be skipped and recorded as privilege_required",
        ))
        .stdout(predicates::str::contains("sudo /bin/bash -c 'exit 0'"))
        .stdout(predicates::str::contains(
            "need root and were skipped (uid=",
        ))
        .stdout(predicates::str::contains(
            "resume with: acps init --resume --deps-apply --deps-apply-yes",
        ))
        .stdout(predicates::str::contains("initialized acp-stack"));

    // The skip is still visible in the audit log as privilege_required.
    let store =
        StateStore::open(default_state_path(tempdir.path())).expect("state store should open");
    let rows = store
        .query_installer_runs_filtered(Some("deps_apply"), 10)
        .expect("installer history should query");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].status, "privilege_required");
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

#[test]
fn init_stack_update_off_sets_manual_policy() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--stack-update",
            "off",
            "--stack-update-frequency",
            "6m",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    let config = load_config_from_str(&written).expect("config should validate");
    assert_eq!(config.updates.acp_stack.policy, StackUpdatePolicy::Manual);
    assert_eq!(config.updates.acp_stack.frequency, "1d");
}

#[test]
fn init_stack_update_on_writes_compatible_policy_and_frequency() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--stack-update",
            "on",
            "--stack-update-frequency",
            "3w",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .success();

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    let config = load_config_from_str(&written).expect("config should validate");
    assert_eq!(
        config.updates.acp_stack.policy,
        StackUpdatePolicy::Compatible
    );
    assert_eq!(config.updates.acp_stack.frequency, "3w");
}

#[test]
fn init_stack_update_rejects_sub_day_frequency() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--stack-update",
            "security",
            "--stack-update-frequency",
            "6m",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("day (d) or week (w)"));

    assert!(
        !tempdir
            .path()
            .join(".config/acp-stack/acps-config.toml")
            .exists(),
        "invalid stack-update frequency must fail before config creation"
    );
}

#[test]
fn init_stack_update_rejects_invalid_policy_before_config_creation() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--stack-update",
            "securty",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("on|security|off"));

    assert!(
        !tempdir
            .path()
            .join(".config/acp-stack/acps-config.toml")
            .exists(),
        "invalid stack-update policy must fail before config creation"
    );
}

#[test]
fn init_stack_update_existing_config_preserves_policy_without_flags() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--stack-update",
            "on",
            "--stack-update-frequency",
            "3w",
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
    assert_eq!(
        config.updates.acp_stack.policy,
        StackUpdatePolicy::Compatible
    );
    assert_eq!(config.updates.acp_stack.frequency, "3w");
}

#[test]
fn init_stack_update_default_preserved_non_interactive() {
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

    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config should be readable");
    let config = load_config_from_str(&written).expect("config should validate");
    // No --stack-update flag and non-interactive: the schema defaults are untouched.
    assert_eq!(
        config.updates.acp_stack.policy,
        StackUpdatePolicy::SecurityCritical
    );
    assert_eq!(config.updates.acp_stack.frequency, "1d");
}

#[test]
fn init_rejects_invalid_mcp_declarations() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    for (extra_args, expected) in [
        (
            &["--mcp-http", "remote=http://mcp.example/mcp"][..],
            "mcp-http",
        ),
        (&["--mcp-http", "remote=https://"], "mcp-http"),
        (
            &["--mcp-http", "remote=https://token@mcp.example/mcp"],
            "credentials",
        ),
        (&["--mcp-preset", "unknown"], "mcp-preset"),
        (&["--mcp-stdio", "local"], "mcp-stdio"),
        (&["--mcp-stdio", "=local-mcp"], "mcp-stdio"),
        (&["--mcp-http", "remote="], "mcp-http"),
        (
            &[
                "--mcp-preset",
                "linear",
                "--mcp-http",
                "linear=https://mcp.example/mcp",
            ],
            "duplicate name",
        ),
        (
            &[
                "--mcp-stdio",
                "local=local-a",
                "--mcp-stdio",
                "local=local-b",
            ],
            "duplicate name",
        ),
        (
            &[
                "--mcp-http",
                "remote=https://mcp-a.example/mcp",
                "--mcp-http",
                "remote=https://mcp-b.example/mcp",
            ],
            "duplicate name",
        ),
        (
            &[
                "--mcp-stdio",
                "shared=local-mcp",
                "--mcp-http",
                "shared=https://mcp.example/mcp",
            ],
            "duplicate name",
        ),
        (
            &["--mcp-http-header", "remote=Authorization"],
            "mcp-http-header",
        ),
        (
            &[
                "--mcp-http",
                "remote=https://mcp.example/mcp",
                "--mcp-http-header",
                "remote=:REMOTE_MCP_TOKEN",
            ],
            "non-empty header",
        ),
        (
            &[
                "--mcp-http",
                "remote=https://mcp.example/mcp",
                "--mcp-http-header",
                "remote=Authorization:",
            ],
            "non-empty header",
        ),
        (
            &[
                "--mcp-http",
                "remote=https://mcp.example/mcp",
                "--mcp-http-header",
                "remote=Bad Header:REMOTE_MCP_TOKEN",
            ],
            "valid HTTP header name",
        ),
        (
            &[
                "--mcp-http-header",
                "missing=Authorization:REMOTE_MCP_TOKEN",
            ],
            "mcp-http-header",
        ),
        (
            &[
                "--mcp-stdio",
                "local=local-mcp",
                "--mcp-http-header",
                "local=Authorization:REMOTE_MCP_TOKEN",
            ],
            "not an HTTP server",
        ),
        (
            &[
                "--mcp-http",
                "remote=https://mcp.example/mcp",
                "--mcp-stdio-env",
                "remote=LOCAL_MCP_TOKEN",
            ],
            "not a stdio server",
        ),
        (
            &[
                "--mcp-stdio",
                "local=local-mcp",
                "--mcp-stdio-env",
                "local=BAD REF",
            ],
            "secret ref name",
        ),
        (
            &[
                "--mcp-http",
                "remote=https://mcp.example/mcp",
                "--mcp-http-header",
                "remote=Authorization:BAD REF",
            ],
            "secret ref name",
        ),
        (
            &[
                "--mcp-stdio",
                "local=local-mcp",
                "--mcp-stdio-env",
                "local=SHARED_MCP_TOKEN",
                "--mcp-http",
                "remote=https://mcp.example/mcp",
                "--mcp-http-header",
                "remote=Authorization:SHARED_MCP_TOKEN",
            ],
            "declared more than once",
        ),
        (
            &[
                "--mcp-http",
                "remote=https://mcp.example/mcp",
                "--mcp-http-header",
                "remote=Authorization:FIRST_TOKEN",
                "--mcp-http-header",
                "remote=authorization:SECOND_TOKEN",
            ],
            "already has header",
        ),
        (
            &["--mcp-stdio-env", "missing=LOCAL_MCP_TOKEN"],
            "mcp-stdio-env",
        ),
    ] {
        assert_init_mcp_failure(tempdir.path(), extra_args, expected);
    }
}

#[test]
fn init_rejects_mcp_declarations_when_config_exists() {
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

    for (extra_args, expected) in [
        (&["--mcp-preset", "linear"][..], "--mcp-preset"),
        (&["--mcp-stdio", "local=local-mcp"], "--mcp-stdio"),
        (
            &["--mcp-stdio-env", "local=LOCAL_MCP_TOKEN"],
            "--mcp-stdio-env",
        ),
        (
            &["--mcp-http", "remote=https://mcp.example/mcp"],
            "--mcp-http",
        ),
        (
            &["--mcp-http-header", "remote=Authorization:REMOTE_MCP_TOKEN"],
            "--mcp-http-header",
        ),
    ] {
        assert_init_mcp_failure(tempdir.path(), extra_args, expected);
    }
}

fn assert_init_mcp_failure(home: &std::path::Path, extra_args: &[&str], expected: &str) {
    acps_command()
        .env("HOME", home)
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--skip-testflight",
            "--skip-workspace-init",
        ])
        .args(extra_args)
        .assert()
        .failure()
        .stderr(predicates::str::contains(expected));
}

#[test]
fn init_rejects_mcp_secret_ref_duplicates_after_registry_defaults() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "amp",
            "--skip-testflight",
            "--skip-workspace-init",
            "--mcp-stdio",
            "local=local-mcp",
            "--mcp-stdio-env",
            "local=AMP_API_KEY",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("declared more than once"));
    assert!(
        !tempdir
            .path()
            .join(".config/acp-stack/acps-config.toml")
            .exists(),
        "invalid post-registry config must not be written"
    );
}

#[test]
fn init_rejects_private_drive_file_viewer_url_as_data_source() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "init",
            "--agent",
            "placebo",
            "--skip-testflight",
            "--data-from",
            "https://drive.google.com/file/d/abc123/view",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("private Drive file viewer link"));
}

#[test]
fn init_accepts_drive_uc_export_download_url_as_data_source() {
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
            "--data-from",
            "https://drive.google.com/uc?export=download&id=abc123",
        ])
        .assert()
        .success();
}

#[test]
fn init_rejects_drive_folder_url_as_data_source() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "init",
            "--agent",
            "placebo",
            "--skip-testflight",
            "--data-from",
            "https://drive.google.com/drive/folders/abc123",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("Drive folder"));
}

#[test]
fn init_rejects_dropbox_preview_url_without_dl_flag() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "init",
            "--agent",
            "placebo",
            "--skip-testflight",
            "--data-from",
            "https://www.dropbox.com/scl/fi/abc123/file.zip?dl=0",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("Dropbox preview link"));
}

#[test]
fn init_accepts_dropbox_url_with_dl_one() {
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
            "--data-from",
            "https://www.dropbox.com/scl/fi/abc123/file.zip?dl=1",
        ])
        .assert()
        .success();
}

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

fn write_capabilities_fixture(
    dir: &std::path::Path,
    mcp_capabilities: serde_json::Value,
) -> std::path::PathBuf {
    let path = dir.join("agent-capabilities.json");
    let body = serde_json::json!({
        "protocol_version": 1,
        "capabilities": { "mcpCapabilities": mcp_capabilities },
        "agent_name": "placebo",
        "agent_title": null,
        "agent_version": null,
    });
    fs::write(&path, body.to_string()).expect("capabilities fixture written");
    path
}

fn init_step_payload(home: &std::path::Path, kind: &str) -> (String, String) {
    let store = StateStore::open(default_state_path(home)).expect("state store");
    let run = store
        .latest_init_run()
        .expect("latest init run")
        .expect("init run exists");
    let steps = store.query_init_steps(&run.id).expect("init steps");
    let step = steps
        .iter()
        .find(|step| step.kind == kind)
        .unwrap_or_else(|| panic!("step `{kind}` recorded: {steps:?}"));
    (step.status.clone(), step.payload_json.clone())
}

#[test]
fn init_reports_unsupported_mcp_transport_as_ignored() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let fixture = write_capabilities_fixture(tempdir.path(), serde_json::json!({}));

    let output = acps_command()
        .env("HOME", tempdir.path())
        .env(
            acp_stack::dev_gates::FIXTURE_AGENT_CAPABILITIES_ENV,
            &fixture,
        )
        .args([
            "dev",
            "init",
            "--handoff-json",
            "--agent",
            "placebo",
            "--skip-workspace-init",
            "--skip-testflight",
            "--mcp-http",
            "remote=https://mcp.example/mcp",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let body: serde_json::Value = serde_json::from_slice(&output).expect("handoff json parses");
    assert_eq!(body["status"], "initialized");
    let ignored = body["ignored_features"]
        .as_array()
        .expect("ignored_features present");
    assert_eq!(ignored.len(), 1, "{body}");
    assert_eq!(ignored[0]["feature"], "mcp.server");
    assert_eq!(ignored[0]["target"], "remote");
    assert_eq!(ignored[0]["capability"], "mcpCapabilities.http");

    // Keep-in-config contract: the declaration is a faithful record and stays.
    let written = fs::read_to_string(tempdir.path().join(".config/acp-stack/acps-config.toml"))
        .expect("config readable");
    let config = load_config_from_str(&written).expect("config validates");
    assert_eq!(config.mcp.servers.len(), 1);

    let (status, payload) = init_step_payload(tempdir.path(), "capability_probe");
    assert_eq!(status, "succeeded");
    assert!(payload.contains(r#""probe_status":"ok""#), "{payload}");
    assert!(payload.contains("mcpCapabilities.http"), "{payload}");

    // Non-interactive runs never record the interactive MCP step: MCP arrives
    // through flags and the ignored-features report, not prompts.
    {
        let store = StateStore::open(default_state_path(tempdir.path())).expect("state store");
        let run = store
            .latest_init_run()
            .expect("latest init run")
            .expect("init run exists");
        let steps = store.query_init_steps(&run.id).expect("init steps");
        assert!(
            steps.iter().all(|step| step.kind != "mcp_configure"),
            "{steps:?}"
        );
    }

    // The probe persists the advertisement so capability routes answer
    // without the agent ever having been started.
    let store = StateStore::open(default_state_path(tempdir.path())).expect("state store");
    let capabilities = store
        .latest_agent_capabilities("placebo")
        .expect("capabilities query");
    assert!(capabilities.is_some(), "agent_capabilities row missing");
}

#[test]
fn init_reports_no_ignores_when_transport_is_advertised() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let fixture = write_capabilities_fixture(tempdir.path(), serde_json::json!({ "http": true }));

    let output = acps_command()
        .env("HOME", tempdir.path())
        .env(
            acp_stack::dev_gates::FIXTURE_AGENT_CAPABILITIES_ENV,
            &fixture,
        )
        .args([
            "dev",
            "init",
            "--handoff-json",
            "--agent",
            "placebo",
            "--skip-workspace-init",
            "--skip-testflight",
            "--mcp-http",
            "remote=https://mcp.example/mcp",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let body: serde_json::Value = serde_json::from_slice(&output).expect("handoff json parses");
    assert_eq!(body["status"], "initialized");
    assert!(
        body.get("ignored_features").is_none(),
        "ignored_features must be omitted when empty: {body}"
    );
}

#[test]
fn init_probe_unavailable_never_fails_init() {
    let tempdir = tempfile::tempdir().expect("tempdir");

    // No capabilities fixture and `--skip-workspace-init` leaves the spawn cwd
    // unprovisioned, so the probe cannot run. Init must succeed regardless and
    // record why the probe made no claims.
    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "dev",
            "init",
            "--agent",
            "placebo",
            "--skip-workspace-init",
            "--skip-testflight",
            "--mcp-http",
            "remote=https://mcp.example/mcp",
        ])
        .assert()
        .success();

    let (status, payload) = init_step_payload(tempdir.path(), "capability_probe");
    assert_eq!(status, "succeeded");
    assert!(
        payload.contains(r#""probe_status":"unavailable""#),
        "{payload}"
    );
}
