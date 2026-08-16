use std::fs;

use crate::common::cli::*;

#[test]
fn agent_install_registry_path_prepares_workspace_root_without_secret_store() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let workspace_root = tempdir.path().join("workspace");
    let binary_path = tempdir
        .path()
        .join(".local")
        .join("bin")
        .join("cli-registry-agent");
    let script = format!(
        "test \"$(pwd -P)\" = \"$(cd {workspace} && pwd -P)\" && mkdir -p {bin} && printf '#!/bin/sh\\n' > {binary} && chmod 755 {binary}",
        workspace = shell_quote_path(&workspace_root),
        bin = shell_quote_path(binary_path.parent().expect("binary has parent")),
        binary = shell_quote_path(&binary_path),
    );
    let config = VALID_CONFIG
        .replace(
            r#"command = "opencode""#,
            r#"command = "cli-registry-agent""#,
        )
        .replace(
            r#"root = "/workspace""#,
            &format!(r#"root = "{}""#, workspace_root.display()),
        )
        .replace(
            r#"uploads = "/workspace/uploads""#,
            &format!(r#"uploads = "{}/uploads""#, workspace_root.display()),
        )
        .replace(r#"args = ["acp"]"#, "args = []")
        .replace(
            r#"
[agent.install]
type = "shell"
shell = "curl -fsSL https://opencode.ai/install | bash"
creates = "opencode"
"#,
            "",
        );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");
    seed_auth_verifiers(tempdir.path(), SESSION_KEY, ADMIN_KEY);
    fs::write(
        config_dir.join("agents.toml"),
        format!(
            r#"
[[agents]]
id = "opencode"
name = "OpenCode Test"
kind = "native"
headless_compatible = true
support_doc = "docs/agents/opencode.md"

[agents.harness]
id = "opencode"

[agents.harness.install.shell]
script = {script:?}
creates = "cli-registry-agent"
"#
        ),
    )
    .expect("registry should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args(["agent", "install", "--yes", "--admin-key", ADMIN_KEY])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "progress: preparing agent install",
        ))
        .stdout(predicates::str::contains(
            "progress: resolving agent install plan",
        ))
        .stdout(predicates::str::contains(
            "progress: installing resolved agent artifacts",
        ))
        .stdout(predicates::str::contains("agent install: installed"))
        .stdout(predicates::str::contains(
            binary_path.to_string_lossy().as_ref(),
        ));

    assert!(workspace_root.is_dir());
    assert!(workspace_root.join("uploads").is_dir());
}
