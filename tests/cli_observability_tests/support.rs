use axum::{Json, Router, routing::get};
use http::StatusCode;
use serde_json::Value;
use std::fs;
use tempfile::TempDir;
use tokio::net::UnixListener;
use tokio::task::JoinHandle;

use crate::common::cli::*;

pub(crate) struct HealthProbeHarness {
    pub(crate) socket_path: std::path::PathBuf,
    pub(crate) join: JoinHandle<std::io::Result<()>>,
    pub(crate) _tempdir: TempDir,
}

impl HealthProbeHarness {
    pub(crate) async fn spawn(status: StatusCode, body: Value) -> Self {
        let tempdir = TempDir::new().expect("tempdir");
        let socket_path = tempdir.path().join("probe.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind local probe");
        let app = Router::new().route(
            "/v1/health/ready",
            get(move || {
                let body = body.clone();
                async move { (status, Json(body)) }
            }),
        );
        let join = tokio::spawn(async move { axum::serve(listener, app).await });
        Self {
            socket_path,
            join,
            _tempdir: tempdir,
        }
    }
}

impl Drop for HealthProbeHarness {
    fn drop(&mut self) {
        self.join.abort();
    }
}

pub(crate) fn write_fake_agent_home(home: &std::path::Path, fake_args: &[&str]) {
    let config_dir = home.join(".config/acp-stack");
    let workspace = home.join("workspace");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::create_dir_all(&workspace).expect("workspace should be created");
    let mut args = vec!["acp"];
    args.extend_from_slice(fake_args);
    let args_toml = args
        .iter()
        .map(|arg| toml_string(arg))
        .collect::<Vec<_>>()
        .join(", ");
    let config = VALID_PLACEBO_CONFIG
        .replace(
            r#"root = "/workspace""#,
            &format!(r#"root = "{}""#, workspace.display()),
        )
        .replace(
            r#"uploads = "/workspace/uploads""#,
            &format!(r#"uploads = "{}/uploads""#, workspace.display()),
        )
        .replace(
            r#"command = "placebo-agent""#,
            &format!(
                "command = {}",
                toml_string(env!("CARGO_BIN_EXE_placebo-agent"))
            ),
        )
        .replace(r#"args = ["acp"]"#, &format!("args = [{args_toml}]"))
        .replace(
            r#"cwd = "/workspace""#,
            &format!("cwd = {}", toml_string(&workspace.to_string_lossy())),
        );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");
}

pub(crate) fn amp_config() -> String {
    VALID_CONFIG
        .replace(r#"id = "opencode""#, r#"id = "amp""#)
        .replace(r#"name = "OpenCode""#, r#"name = "Amp Code""#)
        .replace(r#"command = "opencode""#, r#"command = "amp-acp""#)
        .replace(r#"args = ["acp"]"#, r#"args = []"#)
        .replace(r#"env = ["OPENCODE_API_KEY"]"#, r#"env = ["AMP_API_KEY"]"#)
        .replace(
            r#"
[agent.install]
type = "shell"
shell = "curl -fsSL https://opencode.ai/install | bash"
creates = "opencode"
"#,
            "",
        )
}
