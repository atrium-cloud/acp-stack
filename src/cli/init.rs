use std::collections::HashMap;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use clap::{Args, ValueEnum};

use crate::agent_installer::{InstallerOutcome, install_resolved, run_installer};
use crate::agent_registry::{InstallSet, RegistryCatalog, RegistryEntry, RegistryKind};
use crate::auth::generate_api_key;
use crate::config::{
    self, AgentProviderConfig, CloudflareEdgeConfig, Config, DependencyEntry, EdgeConfig,
};
use crate::error::{Result, StackError};
use crate::fs_util::{
    atomic_write_owner_only, create_dir_owner_only, home_dir, parent_dir, pre_create_owner_only,
    set_owner_only_file, write_new_file_owner_only,
};
use crate::runtime::provider_keys::{
    env_refs_for_agent_id, env_var_for_agent_provider_id, provider_id_is_known,
    provider_id_supports_agent, required_env_refs_for_provider_id,
};
use crate::secrets::{SecretStore, age_key_path, secret_store_path};
use crate::state::{StateStore, default_state_path};

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Select the configured agent non-interactively from the registry.
    #[arg(long)]
    agent: Option<String>,
    /// Select the initial provider id for agents that support provider setup.
    #[arg(long)]
    provider: Option<String>,
    /// Secret ref to inject for the selected initial provider.
    #[arg(long, requires = "provider")]
    api_key_ref: Option<String>,
    /// Install the selected or already configured agent during init.
    #[arg(long, conflicts_with = "no_install_agent")]
    install_agent: bool,
    /// Skip the install prompt in interactive runs.
    #[arg(long)]
    no_install_agent: bool,
    /// Configure a public edge profile during init.
    #[arg(long, value_enum)]
    edge: Option<EdgeProviderArg>,
    /// Public exposure model for the selected edge provider.
    #[arg(long, value_enum, requires = "edge")]
    exposure: Option<EdgeExposureArg>,
    /// Public hostname for the edge profile, for example agent.example.com.
    #[arg(long, requires = "edge")]
    hostname: Option<String>,
    /// How cloudflared is expected to run for generated Cloudflare artifacts.
    #[arg(long, value_enum, default_value_t = CloudflaredDeploymentArg::Host)]
    cloudflared_deployment: CloudflaredDeploymentArg,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum EdgeProviderArg {
    Cloudflare,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum EdgeExposureArg {
    Tunnel,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CloudflaredDeploymentArg {
    Host,
    Docker,
    External,
}

impl CloudflaredDeploymentArg {
    fn as_config_value(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Docker => "docker",
            Self::External => "external",
        }
    }
}

pub(super) fn run_init(args: InitArgs) -> Result<()> {
    let home = home_dir()?;
    let config_path = config::default_config_path()?;
    let state_path = default_state_path(&home);
    let config_dir = parent_dir(&config_path)?;
    let state_dir = parent_dir(&state_path)?;

    create_dir_owner_only(config_dir)?;
    create_dir_owner_only(state_dir)?;

    let config_status = if config_path.exists() {
        // Repair perms before validation so a failure to parse the file does not
        // leave a permissive config on disk; matches the behavior of `acps status`.
        set_owner_only_file(&config_path)?;
        Config::load_from_path(&config_path)?;
        "validated existing config"
    } else {
        write_new_file_owner_only(&config_path, starter_config().as_bytes())?;
        Config::load_from_path(&config_path)?;
        "created starter config"
    };

    pre_create_owner_only(&state_path)?;
    let store = StateStore::open(&state_path)?;
    store.migrate()?;
    set_owner_only_file(&state_path)?;

    let registry = RegistryCatalog::load_with_override(&operator_registry_override(&home))?;
    let mut config = Config::load_from_path(&config_path)?;
    let selected_agent = select_agent_for_init(&args, &registry)?;
    if let Some(entry) = selected_agent {
        apply_registry_entry_to_config(&mut config, entry);
    }
    let edge_requested = apply_edge_profile_to_config(&args, &mut config)?;

    let session_ref = config.auth.session_key_ref.clone();
    let admin_ref = config.auth.admin_key_ref.clone();
    let store_existed = secret_store_path(&home).exists();
    let mut secret_store = SecretStore::open_or_create(&home)?;
    let session_present = secret_store.contains(&session_ref);
    let admin_present = secret_store.contains(&admin_ref);
    let auth_status = if store_existed {
        // Pre-existing store: both refs must be present. Half-initialized state
        // (e.g. one ref deleted, or unrelated secrets but no auth refs) is an
        // anomaly. Refuse to proceed — admin key is not regenerable in place;
        // the documented recovery path is `acps reset --yes`.
        if !admin_present {
            return Err(StackError::MissingAdminKey { name: admin_ref });
        }
        if !session_present {
            return Err(StackError::MissingSessionKey { name: session_ref });
        }
        "preserved existing API keys"
    } else {
        // Fresh store: generate both keys. Print the values BEFORE the durable
        // event write, so a downstream failure in `append_event` cannot leave
        // the persisted-but-never-revealed admin key unrecoverable.
        let session_value = generate_api_key();
        let admin_value = generate_api_key();
        println!("---");
        println!("session key ({session_ref}): {session_value}");
        println!("admin key ({admin_ref}): {admin_value}");
        println!(
            "save the admin key now; it is never regenerable. use `acps reset --yes` to rotate it."
        );
        println!("---");
        // Write both refs in one atomic persist so a mid-init failure cannot
        // leave the store with one key set and the other missing, which the
        // fail-fast logic would then treat as a corrupted state requiring
        // reset.
        secret_store.set_many([
            (session_ref.as_str(), session_value.as_str()),
            (admin_ref.as_str(), admin_value.as_str()),
        ])?;
        store.append_event_with_source(
            "info",
            "auth.keys_generated",
            crate::state::EVENT_SOURCE_CLI,
            "generated session and admin API keys",
            &serde_json::json!({
                "session_key_ref": session_ref,
                "admin_key_ref": admin_ref,
            })
            .to_string(),
        )?;
        "generated session and admin API keys"
    };

    let install_requested = should_install_agent(&args, selected_agent.is_some())?;
    let install_outcome = if install_requested {
        Some(install_configured_agent(&home, &config, &registry, &store)?)
    } else {
        None
    };

    let provider_configured = configure_provider_for_init(
        &args,
        &registry,
        &mut config,
        &config_path,
        &mut secret_store,
    )?;
    if selected_agent.is_some() || provider_configured || edge_requested {
        let canonical = config.to_canonical_toml()?;
        config = config::load_config_from_str(&canonical)?;
        atomic_write_owner_only(&config_path, canonical.as_bytes())?;
    }

    let provisioned_agent_configs =
        crate::runtime::agent_headless_config::provision_agent_headless_config(&config, &home)?;
    let provisioned_edge_artifacts = if edge_requested {
        let config_dir = parent_dir(&config_path)?;
        match config.edge.cloudflare.as_ref() {
            Some(cloudflare) if cloudflare.enabled => {
                let service_url = crate::edge::service_url_from_bind(&config.api.bind)?;
                crate::edge::write_cloudflare_artifacts(config_dir, cloudflare, &service_url)?
            }
            _ => Vec::new(),
        }
    } else {
        Vec::new()
    };

    // Record init.completed AFTER secret-store setup so a half-finished init
    // (e.g. failed key generation) does not leave a misleading
    // "initialized" event in the durable log.
    store.append_event_with_source(
        "info",
        "init.completed",
        crate::state::EVENT_SOURCE_CLI,
        "initialized",
        "{}",
    )?;

    println!("initialized acp-stack");
    println!("{config_status}: {}", config_path.display());
    println!("state: {}", state_path.display());
    println!("secrets: {}", secret_store.store_path().display());
    println!("age key: {}", age_key_path(&home).display());
    println!("auth: {auth_status}");
    println!("agent: {} ({})", config.agent.name, config.agent.id);
    if let Some(outcome) = install_outcome {
        println!("agent install: {}", outcome.label());
        println!("agent path: {}", outcome.path().display());
        println!("agent sha256: {}", outcome.sha256());
    }
    for provisioned in provisioned_agent_configs {
        println!("{}: {}", provisioned.label, provisioned.path.display());
    }
    for artifact in provisioned_edge_artifacts {
        println!("{}: {}", artifact.label, artifact.path.display());
    }

    Ok(())
}

fn apply_edge_profile_to_config(args: &InitArgs, config: &mut Config) -> Result<bool> {
    let Some(edge) = args.edge else {
        return Ok(false);
    };
    match edge {
        EdgeProviderArg::Cloudflare => {}
    }
    if !matches!(args.exposure, Some(EdgeExposureArg::Tunnel)) {
        return Err(StackError::MissingField {
            field: "--exposure tunnel",
        });
    }
    let hostname = args
        .hostname
        .as_ref()
        .ok_or(StackError::MissingField {
            field: "--hostname",
        })?
        .trim()
        .to_owned();
    let public_url = format!("https://{hostname}");
    config.api.bind = "127.0.0.1:7700".to_owned();
    config.api.public_url = Some(public_url.clone());
    config.security.http.allowed_origins = vec![public_url];
    config.security.http.trust_proxy_headers = true;
    config.security.http.trusted_proxies = vec!["127.0.0.1".to_owned(), "::1".to_owned()];
    config.edge = EdgeConfig {
        cloudflare: Some(CloudflareEdgeConfig {
            enabled: true,
            mode: "generated".to_owned(),
            exposure: "tunnel".to_owned(),
            hostname,
            tunnel_name: Some("acp-stack".to_owned()),
            tunnel_id: None,
            cloudflared_deployment: args.cloudflared_deployment.as_config_value().to_owned(),
        }),
    };
    if matches!(args.cloudflared_deployment, CloudflaredDeploymentArg::Host)
        && !config
            .dependencies
            .commands
            .iter()
            .any(|entry| entry.name == "cloudflared")
    {
        config.dependencies.commands.push(DependencyEntry {
            name: "cloudflared".to_owned(),
            required: true,
            feature: Some("cloudflare-tunnel".to_owned()),
        });
    }
    Ok(true)
}

fn select_agent_for_init<'a>(
    args: &InitArgs,
    registry: &'a RegistryCatalog,
) -> Result<Option<&'a RegistryEntry>> {
    if let Some(id) = &args.agent {
        return registry
            .lookup(id)
            .ok_or_else(|| StackError::AgentRegistryMissing { id: id.clone() })
            .map(Some);
    }
    if !io::stdin().is_terminal() {
        return Ok(None);
    }
    let entries = registry.entries();
    if entries.is_empty() {
        return Ok(None);
    }
    println!("Select an agent to configure:");
    for (index, entry) in entries.iter().enumerate() {
        println!("  {}. {} ({})", index + 1, entry.name, entry.id);
    }
    print!("agent [1-{}, blank to keep current]: ", entries.len());
    io::stdout()
        .flush()
        .map_err(|source| StackError::ConfigWrite {
            path: PathBuf::from("stdout"),
            source,
        })?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(|source| StackError::ConfigRead {
            path: PathBuf::from("stdin"),
            source,
        })?;
    let answer = answer.trim();
    if answer.is_empty() {
        return Ok(None);
    }
    let index = answer
        .parse::<usize>()
        .map_err(|_| StackError::InvalidParam {
            field: "agent",
            reason: format!("invalid selection `{answer}`"),
        })?;
    entries
        .get(index.saturating_sub(1))
        .ok_or_else(|| StackError::InvalidParam {
            field: "agent",
            reason: format!("selection `{answer}` is out of range"),
        })
        .map(Some)
}

fn apply_registry_entry_to_config(config: &mut Config, entry: &RegistryEntry) {
    config.agent.id = entry.id.clone();
    config.agent.name = entry.name.clone();
    config.agent.cwd = Some(config.workspace.root.clone());
    config.agent.env = default_agent_env_refs(&entry.id);
    config.agent.expected_sha256 = None;
    config.agent.restart = "on-crash".to_owned();
    config.agent.mode = None;
    config.agent.model = None;
    config.agent.harness_version = None;
    config.agent.adapter = None;
    config.agent.provider = None;
    config.agent.install = None;

    match entry.kind {
        RegistryKind::Native => {
            let harness = entry.harness.as_ref().expect("validated registry harness");
            config.agent.command = harness.id.clone();
            config.agent.args = vec!["acp".to_owned()];
        }
        RegistryKind::Adapter => {
            let adapter = entry.adapter.as_ref().expect("validated registry adapter");
            config.agent.command = adapter.id.clone();
            config.agent.args = Vec::new();
        }
    }
}

fn configure_provider_for_init(
    args: &InitArgs,
    registry: &RegistryCatalog,
    config: &mut Config,
    config_path: &Path,
    secret_store: &mut SecretStore,
) -> Result<bool> {
    let Some(provider_id) = select_provider_for_init(args, registry, config)? else {
        return Ok(false);
    };
    let required_refs = apply_provider_to_config(
        registry,
        config,
        config_path,
        provider_id,
        args.api_key_ref.clone(),
    )?;
    collect_missing_provider_refs(secret_store, &required_refs)?;
    Ok(true)
}

fn select_provider_for_init(
    args: &InitArgs,
    registry: &RegistryCatalog,
    config: &Config,
) -> Result<Option<String>> {
    if let Some(provider_id) = &args.provider {
        return Ok(Some(provider_id.clone()));
    }
    if !io::stdin().is_terminal() {
        return Ok(None);
    }
    let Some(entry) = registry.lookup(&config.agent.id) else {
        return Ok(None);
    };
    if !entry.set_provider {
        return Ok(None);
    }

    print!("provider id [blank to skip]: ");
    io::stdout()
        .flush()
        .map_err(|source| StackError::ConfigWrite {
            path: PathBuf::from("stdout"),
            source,
        })?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(|source| StackError::ConfigRead {
            path: PathBuf::from("stdin"),
            source,
        })?;
    let answer = answer.trim();
    if answer.is_empty() {
        Ok(None)
    } else {
        Ok(Some(answer.to_owned()))
    }
}

fn apply_provider_to_config(
    registry: &RegistryCatalog,
    config: &mut Config,
    config_path: &Path,
    provider_id: String,
    api_key_ref: Option<String>,
) -> Result<Vec<String>> {
    let entry =
        registry
            .lookup(&config.agent.id)
            .ok_or_else(|| StackError::AgentRegistryMissing {
                id: config.agent.id.clone(),
            })?;
    if !entry.set_provider {
        return Err(StackError::AgentConfigProvision {
            path: config_path.to_path_buf(),
            reason: format!(
                "{} does not support provider configuration during init",
                config.agent.name
            ),
        });
    }
    if !provider_id_is_known(&provider_id) {
        return Err(StackError::InvalidParam {
            field: "provider",
            reason: format!("provider `{provider_id}` is not listed in data/mapping.toml"),
        });
    }
    if !provider_id_supports_agent(&provider_id, &config.agent.id) {
        return Err(StackError::InvalidParam {
            field: "provider",
            reason: format!(
                "provider `{provider_id}` is not supported for agent `{}`",
                config.agent.id
            ),
        });
    }
    let default_api_key_ref = env_var_for_agent_provider_id(&config.agent.id, &provider_id);
    if default_api_key_ref.is_none() {
        return Err(StackError::AgentConfigProvision {
            path: config_path.to_path_buf(),
            reason: format!(
                "provider `{provider_id}` has no API-key env mapping for agent `{}`",
                config.agent.id
            ),
        });
    }
    let api_key_ref = api_key_ref
        .or_else(|| default_api_key_ref.map(str::to_owned))
        .expect("default API-key ref checked");

    let required_refs = required_env_refs_for_provider_id(&provider_id, &api_key_ref);
    for env_ref in &required_refs {
        if !config.agent.env.iter().any(|name| name == env_ref) {
            config.agent.env.push(env_ref.clone());
        }
    }
    config.agent.provider = Some(AgentProviderConfig {
        id: provider_id,
        model: None,
        api_key_ref: Some(api_key_ref),
    });
    Ok(required_refs)
}

fn collect_missing_provider_refs(
    secret_store: &mut SecretStore,
    required_refs: &[String],
) -> Result<()> {
    if !io::stdin().is_terminal() {
        return Ok(());
    }
    let mut collected = Vec::new();
    for env_ref in required_refs {
        if secret_store.contains(env_ref) {
            continue;
        }
        print!("{env_ref}: ");
        io::stdout()
            .flush()
            .map_err(|source| StackError::ConfigWrite {
                path: PathBuf::from("stdout"),
                source,
            })?;
        let mut value = String::new();
        io::stdin()
            .read_line(&mut value)
            .map_err(|source| StackError::StdinRead { source })?;
        let value = value.trim_end_matches(['\n', '\r']).to_owned();
        if !value.is_empty() {
            collected.push((env_ref.as_str(), value));
        }
    }
    secret_store.set_many(
        collected
            .iter()
            .map(|(name, value)| (*name, value.as_str())),
    )
}

fn default_agent_env_refs(agent_id: &str) -> Vec<String> {
    env_refs_for_agent_id(agent_id)
        .into_iter()
        .map(str::to_owned)
        .collect()
}

fn should_install_agent(args: &InitArgs, selected_agent: bool) -> Result<bool> {
    if args.install_agent {
        return Ok(true);
    }
    if args.no_install_agent || !selected_agent || !io::stdin().is_terminal() {
        return Ok(false);
    }
    print!("Install the selected agent now? [y/N]: ");
    io::stdout()
        .flush()
        .map_err(|source| StackError::ConfigWrite {
            path: PathBuf::from("stdout"),
            source,
        })?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(|source| StackError::ConfigRead {
            path: PathBuf::from("stdin"),
            source,
        })?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "YES" | "Yes"))
}

fn install_configured_agent(
    home: &Path,
    config: &Config,
    registry: &RegistryCatalog,
    store: &StateStore,
) -> Result<InstallerOutcome> {
    let workspace_root = PathBuf::from(config.workspace.root.clone());
    if let Some(install) = config.agent.install.as_ref() {
        let env = resolve_agent_env(home, config)?;
        return run_installer(
            install,
            config.agent.expected_sha256.as_deref(),
            env,
            &workspace_root,
            store,
        );
    }
    let entry =
        registry
            .lookup(&config.agent.id)
            .ok_or_else(|| StackError::AgentRegistryMissing {
                id: config.agent.id.clone(),
            })?;
    match install_resolved(
        &config.agent,
        entry,
        Default::default(),
        &workspace_root,
        &local_bin_dir(home),
        store,
    ) {
        Ok(outcome) => Ok(outcome),
        Err(first_error) if io::stdin().is_terminal() => {
            if let Some(retry_entry) = prompt_retry_install_path(
                entry,
                config.agent.harness_version.as_deref(),
                &first_error,
            )? {
                install_resolved(
                    &config.agent,
                    &retry_entry,
                    Default::default(),
                    &workspace_root,
                    &local_bin_dir(home),
                    store,
                )
            } else {
                Err(first_error)
            }
        }
        Err(err) => Err(err),
    }
}

fn prompt_retry_install_path(
    entry: &RegistryEntry,
    version_pin: Option<&str>,
    first_error: &StackError,
) -> Result<Option<RegistryEntry>> {
    let mut retry = entry.clone();
    let mut options = Vec::new();
    if let Some(harness) = retry.harness.as_mut()
        && let Some(label) = remove_selected_path(&mut harness.install, version_pin)
    {
        options.push(format!("harness via {label}"));
    }
    if let Some(adapter) = retry.adapter.as_mut()
        && let Some(label) = remove_selected_path(&mut adapter.install, None)
    {
        options.push(format!("adapter via {label}"));
    }
    if options.is_empty() {
        return Ok(None);
    }
    println!("agent install failed: {first_error}");
    println!("available retry path: {}", options.join(", "));
    print!("Try the next install path now? [y/N]: ");
    io::stdout()
        .flush()
        .map_err(|source| StackError::ConfigWrite {
            path: PathBuf::from("stdout"),
            source,
        })?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(|source| StackError::ConfigRead {
            path: PathBuf::from("stdin"),
            source,
        })?;
    if matches!(answer.trim(), "y" | "Y" | "yes" | "YES" | "Yes") {
        Ok(Some(retry))
    } else {
        Ok(None)
    }
}

fn remove_selected_path(
    install: &mut InstallSet,
    version_pin: Option<&str>,
) -> Option<&'static str> {
    if version_pin.is_some() {
        if install.github.take().is_some() {
            return next_path_label(install);
        }
        if install.npm.take().is_some() {
            return next_path_label(install);
        }
        return None;
    }
    if install.shell.take().is_some() {
        return next_path_label(install);
    }
    if install.npm.take().is_some() {
        return next_path_label(install);
    }
    if install.github.take().is_some() {
        return next_path_label(install);
    }
    None
}

fn next_path_label(install: &InstallSet) -> Option<&'static str> {
    if install.shell.is_some() {
        Some("shell")
    } else if install.npm.is_some() {
        Some("npm")
    } else if install.github.is_some() {
        Some("github")
    } else {
        None
    }
}

fn resolve_agent_env(home: &Path, config: &Config) -> Result<HashMap<String, String>> {
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

fn operator_registry_override(home: &Path) -> PathBuf {
    home.join(".config").join("acp-stack").join("agents.toml")
}

fn local_bin_dir(home: &Path) -> PathBuf {
    home.join(".local").join("bin")
}

fn starter_config() -> &'static str {
    r#"[api]
bind = "127.0.0.1:7700"
public_url = "http://127.0.0.1:7700"
max_request_bytes = 104857600

[auth]
session_key_ref = "ACP_STACK_SESSION_KEY"
admin_key_ref = "ACP_STACK_ADMIN_KEY"

[security.http]
max_request_bytes = 104857600
rate_limit_per_minute = 120
burst = 30
auth_failures_per_minute = 5
auth_block_duration = "15m"
allowed_origins = []
trust_proxy_headers = false

[workspace]
root = "/workspace"
uploads = "/workspace/uploads"
default_shell = "/bin/bash"
runtime_user = "acp"
max_file_bytes = 8388608

[workspace.source]
type = "none"

[logging]
level = "info"
local_retention_days = 30

[logging.supabase]
enabled = false
url = "https://example.supabase.co"
api_key_ref = "SUPABASE_SECRET_KEY"
schema = "acp_stack"

[agent]
id = "placeholder"
name = "Placeholder Agent"
command = "acp-agent"
args = []
cwd = "/workspace"
env = []
restart = "never"

[agent.install]
type = "shell"
shell = "true"
creates = "acp-agent"
"#
}
