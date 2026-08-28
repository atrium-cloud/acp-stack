use crate::common::cli::*;
use acp_stack::config::{McpServerConfig, load_config_from_str};
use std::fs;

#[test]
fn init_creates_config_and_state() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");

    let mut command = acps_command(tempdir.path());

    command
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

    acps_command(tempdir.path())
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

    acps_command(tempdir.path())
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
