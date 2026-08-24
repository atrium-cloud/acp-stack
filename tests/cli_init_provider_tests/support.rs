use std::fs;

use crate::common::cli::*;

// The fixture env var short-circuits the ACP spawn so these tests do not
// require a real opencode binary.
pub(crate) fn write_workspace_init_config(home: &std::path::Path) {
    let config_dir = home.join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir");
    let workspace = home.join("workspace");
    fs::create_dir_all(&workspace).expect("workspace dir");
    let config = VALID_CONFIG
        .replace(
            r#"root = "/workspace""#,
            &format!(r#"root = "{}""#, workspace.display()),
        )
        .replace(
            r#"uploads = "/workspace/uploads""#,
            &format!(r#"uploads = "{}/uploads""#, workspace.display()),
        )
        .replace(
            r#"cwd = "/workspace""#,
            &format!(r#"cwd = "{}""#, workspace.display()),
        )
        .replace(r#"command = "opencode""#, r#"command = "/bin/true""#);
    fs::write(config_dir.join("acps-config.toml"), config).expect("config");
}
