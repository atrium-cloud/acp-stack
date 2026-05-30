use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::error::{Result, StackError};
use crate::fs_util::{
    create_dir_owner_only, home_dir, parent_dir, pre_create_owner_only, set_owner_only_file,
};
use crate::runtime::install::agent_installer::{install_resolved, run_installer};
use crate::runtime::install::agent_registry::RegistryCatalog;
use crate::secrets::SecretStore;
use crate::state::{StateStore, default_state_path};

use super::AgentInstallArgs;
use crate::cli::core::{OutputFormat, daemon_base_url, print_json};

pub(super) fn run_agent_daemon_post(
    path: &'static str,
    label: &'static str,
    output: OutputFormat,
) -> Result<()> {
    let home = home_dir()?;
    let config = Config::load_from_default_path()?;
    let store = SecretStore::open(&home)?;
    let admin_key = store.get(&config.auth.admin_key_ref)?.to_owned();
    let base_url = daemon_base_url(config.api.public_url.as_deref(), &config.api.bind)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|source| StackError::ServeIo { source })?;
    let body = runtime.block_on(post_agent_daemon(&base_url, path, &admin_key))?;
    if output.is_json() {
        print_json(body.get("data").unwrap_or(&body))?;
        return Ok(());
    }
    if label == "start" || label == "restart" {
        let pid = body["data"]["pid"]
            .as_u64()
            .map(|pid| pid.to_string())
            .unwrap_or_else(|| "unknown".to_owned());
        println!("agent {label}: running");
        println!("pid: {pid}");
    } else {
        let exit_status = body["data"]["exit_status"]
            .as_i64()
            .map(|status| status.to_string())
            .unwrap_or_else(|| "unknown".to_owned());
        println!("agent stop: stopped");
        println!("exit_status: {exit_status}");
    }
    Ok(())
}

async fn post_agent_daemon(
    base_url: &str,
    path: &'static str,
    admin_key: &str,
) -> Result<serde_json::Value> {
    let url = format!("{}{}", base_url.trim_end_matches('/'), path);
    let response = reqwest::Client::new()
        .post(url)
        .bearer_auth(admin_key)
        .send()
        .await
        .map_err(|source| StackError::AgentApiRequest { path, source })?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|source| StackError::AgentApiRequest { path, source })?;
    if !status.is_success() {
        return Err(StackError::AgentApiStatus { path, status, body });
    }
    serde_json::from_str(&body).map_err(|err| StackError::AgentInitializeFailed {
        reason: format!("agent API response was not JSON: {err}"),
    })
}

pub(super) fn run_agent_install(_args: AgentInstallArgs, output: OutputFormat) -> Result<()> {
    let home = home_dir()?;
    let config = Config::load_from_default_path()?;

    let state_path = default_state_path(&home);
    let state_dir = parent_dir(&state_path)?;
    create_dir_owner_only(state_dir)?;
    pre_create_owner_only(&state_path)?;
    let store = StateStore::open(&state_path)?;
    store.migrate()?;
    set_owner_only_file(&state_path)?;

    let workspace_root = PathBuf::from(config.workspace.root.clone());
    let log_base = crate::state::default_installer_log_base(&home);

    let outcome = if let Some(install) = config.agent.install.as_ref() {
        // Operator escape-hatch shell recipe takes precedence over the
        // embedded registry. Useful for private forks of an agent whose id
        // happens to clash with a curated entry.
        let env = resolve_agent_env_for_cli(&home, &config)?;
        let expected_sha256 = config.agent.expected_sha256.clone();
        run_installer(
            &config.agent.id,
            install,
            expected_sha256.as_deref(),
            env,
            &workspace_root,
            &store,
            Some(&log_base),
        )?
    } else {
        let registry = RegistryCatalog::load_with_override(&operator_registry_override(&home))?;
        let entry = registry.lookup_required(&config.agent.id)?;
        let dest = local_bin_dir(&home);
        install_resolved(
            &config.agent,
            entry,
            Default::default(),
            &workspace_root,
            &dest,
            &store,
            Some(&log_base),
        )?
    };

    if output.is_json() {
        print_json(&serde_json::json!({
            "status": outcome.label(),
            "path": outcome.path().display().to_string(),
            "sha256": outcome.sha256(),
        }))?;
    } else {
        println!("agent install: {}", outcome.label());
        println!("path: {}", outcome.path().display());
        println!("sha256: {}", outcome.sha256());
    }
    Ok(())
}

pub(in crate::cli) fn operator_registry_override(home: &Path) -> PathBuf {
    home.join(".config").join("acp-stack").join("agents.toml")
}

pub(super) fn local_bin_dir(home: &Path) -> PathBuf {
    home.join(".local").join("bin")
}

pub(super) fn resolve_agent_env_for_cli(
    home: &Path,
    config: &Config,
) -> Result<HashMap<String, String>> {
    if config.agent.env.is_empty() {
        return Ok(HashMap::new());
    }
    let store = SecretStore::open(home)?;
    let mut env = HashMap::with_capacity(config.agent.env.len());
    for name in &config.agent.env {
        let value = store.get(name)?;
        env.insert(name.clone(), value.to_owned());
    }
    Ok(env)
}
