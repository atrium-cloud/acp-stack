use std::collections::HashMap;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use clap::{Args, ValueEnum};

use crate::agent_installer::{InstallerOutcome, install_resolved, run_installer};
use crate::agent_registry::{InstallSet, RegistryCatalog, RegistryEntry, RegistryKind};
use crate::auth::generate_api_key;
use crate::config::{
    self, AgentConfig, AgentCustomProviderConfig, AgentInstallConfig, AgentProviderConfig,
    ApiConfig, AuthConfig, CloudflareEdgeConfig, CodeSourceConfig, Config, CustomProviderApi,
    DEFAULT_CUSTOM_MODEL_CONTEXT, DEFAULT_CUSTOM_MODEL_OUTPUT_MAX_TOKENS, DataSourceConfig,
    DependencyEntry, EdgeConfig, LoggingConfig, SecurityConfig, SecurityHttpConfig,
    SupabaseLoggingConfig, WorkspaceConfig,
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
    /// Configure the selected provider as a custom provider.
    #[arg(long, requires = "provider")]
    custom_provider: bool,
    /// Display name for a custom provider.
    #[arg(long = "provider-name", requires = "custom_provider")]
    provider_name: Option<String>,
    /// Base URL for a custom provider.
    #[arg(long = "base-url", requires = "custom_provider")]
    base_url: Option<String>,
    /// API family for a custom provider: chat-completions or responses.
    #[arg(long = "provider-api", requires = "custom_provider")]
    provider_api: Option<String>,
    /// Initial custom model id.
    #[arg(long, requires = "custom_provider")]
    model: Option<String>,
    /// Display name for a custom model.
    #[arg(long = "model-name", requires = "custom_provider")]
    model_name: Option<String>,
    /// Context window in tokens for a custom model.
    #[arg(long, requires = "custom_provider")]
    context: Option<String>,
    /// Maximum output tokens for a custom model.
    #[arg(long = "output-max-tokens", requires = "custom_provider")]
    output_max_tokens: Option<String>,
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
    /// Workspace root to write into a newly-created starter config.
    #[arg(long)]
    workspace_root: Option<String>,
    /// Workspace uploads path to write into a newly-created starter config.
    #[arg(long)]
    workspace_uploads: Option<String>,
    /// Runtime user to write into a newly-created starter config.
    #[arg(long)]
    runtime_user: Option<String>,
    /// Pre-seed `[[workspace.code_sources]]` with one or more git
    /// repositories. Repeatable. Accepts an `https://...`, `git@host:repo`,
    /// or other supported repo URL. Only applied when the starter config is
    /// being created.
    #[arg(long = "code-from", value_name = "URL")]
    code_from: Vec<String>,
    /// Pre-seed `[[workspace.data_sources]]` with a local path or an
    /// `https://...` archive URL. Repeatable. Only applied when the starter
    /// config is being created.
    #[arg(long = "data-from", value_name = "PATH_OR_URL")]
    data_from: Vec<String>,
    /// Skip the workspace materializer; useful for tests and dev loops that
    /// do not need actual content fetched/cloned.
    #[arg(long)]
    skip_workspace_init: bool,
    /// Run the real-prompt agent testflight at the end of init. Warns about
    /// provider credit consumption. Mutually exclusive with `--skip-testflight`.
    #[arg(long, conflicts_with = "skip_testflight")]
    testflight: bool,
    /// Suppress the end-of-init testflight even in interactive runs.
    #[arg(long)]
    skip_testflight: bool,
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

const STARTER_MAX_REQUEST_BYTES: u64 = 104_857_600;
const STARTER_RATE_LIMIT_PER_MINUTE: u64 = 120;
const STARTER_RATE_LIMIT_BURST: u64 = 30;
const STARTER_AUTH_FAILURES_PER_MINUTE: u64 = 5;
const STARTER_AUTH_BLOCK_DURATION: &str = "15m";
const STARTER_SESSION_KEY_REF: &str = "ACP_STACK_SESSION_KEY";
const STARTER_ADMIN_KEY_REF: &str = "ACP_STACK_ADMIN_KEY";
const STARTER_DEFAULT_SHELL: &str = "/bin/bash";
const STARTER_WORKSPACE_MAX_FILE_BYTES: u64 = 8_388_608;
const STARTER_LOCAL_RETENTION_DAYS: u64 = 30;
const STARTER_LOG_LEVEL: &str = "info";
const STARTER_SUPABASE_URL: &str = "https://example.supabase.co";
const STARTER_SUPABASE_SERVICE_ROLE_KEY_REF: &str = "SUPABASE_SERVICE_ROLE_KEY";
const STARTER_SUPABASE_SCHEMA: &str = "acp_stack";
const STARTER_AGENT_ID: &str = "placeholder";
const STARTER_AGENT_NAME: &str = "Placeholder Agent";
const STARTER_AGENT_COMMAND: &str = "acp-agent";
const STARTER_AGENT_RESTART: &str = "never";
const STARTER_AGENT_INSTALL_CREATES: &str = "acp-agent";
const STARTER_AGENT_INSTALL_TYPE: &str = "shell";
const STARTER_AGENT_INSTALL_COMMAND: &str = "true";

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
        let existing_config = Config::load_from_path(&config_path)?;
        validate_deployment_overrides_match_existing(&args, &existing_config)?;
        "validated existing config"
    } else {
        let starter_config = starter_config(&args)?;
        write_new_file_owner_only(&config_path, starter_config.as_bytes())?;
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
        // Fail fast on agents the runtime cannot drive headlessly (browser
        // OAuth, terminal-only adapters, etc.). Without this check init would
        // happily install the binary and only fail at first session spawn,
        // wasting bandwidth and operator time.
        entry.ensure_supported()?;
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

    let materialize_report = if args.skip_workspace_init {
        None
    } else {
        Some(crate::runtime::workspace_init::materialize_workspace(
            &config.workspace,
            &secret_store,
        )?)
    };

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
    if let Some(materialize) = &materialize_report {
        for entry in &materialize.code {
            println!(
                "code source ({:?}): {}",
                entry.outcome,
                entry.destination.display()
            );
        }
        for entry in &materialize.data {
            println!(
                "data source ({:?}): {}",
                entry.outcome,
                entry.destination.display()
            );
        }
    }

    if let Some(decision) = resolve_testflight_decision(&args, &config, &registry)? {
        match decision {
            TestflightDecision::Run => {
                println!("---");
                println!("running real-prompt agent testflight");
                crate::cli::agent::run_init_testflight(&home, &config, &registry)?;
            }
            TestflightDecision::SkipExplicit => {
                println!("testflight: skipped (--skip-testflight)");
            }
            TestflightDecision::SkipNonInteractive => {
                println!("testflight: skipped (non-interactive run; pass --testflight to opt in)");
            }
            TestflightDecision::SkipDeclined => {
                println!("testflight: skipped (declined at prompt)");
            }
            TestflightDecision::SkipUnsupported => {
                println!("testflight: skipped (agent does not support headless testflight)");
            }
        }
    }

    Ok(())
}

/// What `acps init` should do with the post-init testflight phase. Resolved
/// from the operator's flags + TTY state + agent registry support so the
/// outer flow can render a clear log line for every path, and the test suite
/// can assert each case without exercising the real ACP bridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TestflightDecision {
    /// All preconditions met and the operator opted in (explicit flag or
    /// interactive yes).
    Run,
    /// Operator passed `--skip-testflight`.
    SkipExplicit,
    /// Non-interactive run and `--testflight` was not passed.
    SkipNonInteractive,
    /// Interactive run and the operator answered no at the credit-warning
    /// prompt.
    SkipDeclined,
    /// Selected agent isn't headless-compatible; the testflight would fail
    /// at spawn. Surface the skip so the operator isn't surprised.
    SkipUnsupported,
}

fn resolve_testflight_decision(
    args: &InitArgs,
    config: &Config,
    registry: &RegistryCatalog,
) -> Result<Option<TestflightDecision>> {
    if args.skip_testflight {
        return Ok(Some(TestflightDecision::SkipExplicit));
    }
    let Some(entry) = registry.lookup(&config.agent.id) else {
        // Operator's `[agent].id` doesn't match the registry (e.g., escape
        // hatch). No registry entry means we don't know the testflight
        // capabilities, so don't auto-run. Surface as a separate state only
        // if the operator explicitly asked.
        if args.testflight {
            return Err(StackError::AgentRegistryMissing {
                id: config.agent.id.clone(),
            });
        }
        return Ok(None);
    };
    if !entry.headless_compatible {
        if args.testflight {
            return Err(StackError::AgentUnsupported {
                name: entry.name.clone(),
            });
        }
        return Ok(Some(TestflightDecision::SkipUnsupported));
    }
    if args.testflight {
        print_testflight_credit_warning(entry);
        return Ok(Some(TestflightDecision::Run));
    }
    if !io::stdin().is_terminal() {
        return Ok(Some(TestflightDecision::SkipNonInteractive));
    }
    if confirm_testflight_credit_warning(entry)? {
        Ok(Some(TestflightDecision::Run))
    } else {
        Ok(Some(TestflightDecision::SkipDeclined))
    }
}

fn confirm_testflight_credit_warning(entry: &RegistryEntry) -> Result<bool> {
    print_testflight_credit_warning(entry);
    print!("run testflight now? [y/N]: ");
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

fn print_testflight_credit_warning(entry: &RegistryEntry) {
    println!("---");
    println!(
        "init testflight will start `{}` and send a real prompt to the configured provider.",
        entry.name
    );
    println!("this may consume provider credits.");
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
    let required_refs = apply_provider_to_config(args, registry, config, config_path, provider_id)?;
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
    args: &InitArgs,
    registry: &RegistryCatalog,
    config: &mut Config,
    config_path: &Path,
    provider_id: String,
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
    if args.custom_provider {
        if !entry.allow_custom_provider {
            return Err(StackError::AgentConfigProvision {
                path: config_path.to_path_buf(),
                reason: format!(
                    "{} does not support custom provider setup",
                    config.agent.name
                ),
            });
        }
        if !entry.allow_custom_model {
            return Err(StackError::AgentConfigProvision {
                path: config_path.to_path_buf(),
                reason: format!("{} does not support custom model setup", config.agent.name),
            });
        }
        return apply_custom_provider_to_config(args, config, config_path, provider_id);
    }
    if !provider_id_is_known(&provider_id) {
        return Err(StackError::InvalidParam {
            field: "provider",
            reason: format!("provider `{provider_id}` is not listed in provider/env mapping"),
        });
    }
    if provider_id_is_known(&provider_id)
        && !provider_id_supports_agent(&provider_id, &config.agent.id)
    {
        return Err(StackError::InvalidParam {
            field: "provider",
            reason: format!(
                "provider `{provider_id}` is not supported for agent `{}`",
                config.agent.id
            ),
        });
    }
    if config.agent.id == "codex" && provider_id == "openai" {
        if args.api_key_ref.is_some() {
            return Err(StackError::AgentConfigProvision {
                path: config_path.to_path_buf(),
                reason: "Codex OpenAI uses Codex-native auth; do not pass --api-key-ref".to_owned(),
            });
        }
        config.agent.provider = Some(AgentProviderConfig {
            id: provider_id,
            model: None,
            api_key_ref: None,
            custom: None,
        });
        return Ok(Vec::new());
    }
    let default_api_key_ref = env_var_for_agent_provider_id(&config.agent.id, &provider_id);
    if default_api_key_ref.is_none() {
        if !entry.allow_custom_provider {
            return Err(StackError::AgentConfigProvision {
                path: config_path.to_path_buf(),
                reason: format!(
                    "{} does not support custom provider setup",
                    config.agent.name
                ),
            });
        }
        if !entry.allow_custom_model {
            return Err(StackError::AgentConfigProvision {
                path: config_path.to_path_buf(),
                reason: format!("{} does not support custom model setup", config.agent.name),
            });
        }
        if !args.custom_provider && !confirm_custom_provider_setup(&provider_id)? {
            return Err(StackError::AgentConfigProvision {
                path: config_path.to_path_buf(),
                reason: format!(
                    "provider `{provider_id}` has no API-key env mapping for agent `{}`",
                    config.agent.id
                ),
            });
        }
        return apply_custom_provider_to_config(args, config, config_path, provider_id);
    }
    let api_key_ref = args
        .api_key_ref
        .clone()
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
        custom: None,
    });
    Ok(required_refs)
}

fn apply_custom_provider_to_config(
    args: &InitArgs,
    config: &mut Config,
    config_path: &Path,
    provider_id: String,
) -> Result<Vec<String>> {
    let provider_name = required_init_custom_value("provider-name", args.provider_name.clone())?;
    let base_url = required_init_custom_value("base-url", args.base_url.clone())?;
    let api_key_ref = required_init_custom_value("api-key-ref", args.api_key_ref.clone())?;
    let model = required_init_custom_value("model", args.model.clone())?;
    let model_name = args.model_name.clone().unwrap_or_else(|| model.clone());
    let api = parse_init_custom_provider_api(
        args.provider_api.as_deref(),
        default_init_custom_provider_api(&config.agent.id),
    )?;
    if config.agent.id == "codex" && api != CustomProviderApi::Responses {
        return Err(StackError::InvalidParam {
            field: "provider-api",
            reason: "Codex custom providers only support responses".to_owned(),
        });
    }
    let context = parse_init_custom_token_limit(
        "context",
        args.context.as_deref(),
        DEFAULT_CUSTOM_MODEL_CONTEXT,
    )?;
    let output_max_tokens = parse_init_custom_token_limit(
        "output-max-tokens",
        args.output_max_tokens.as_deref(),
        DEFAULT_CUSTOM_MODEL_OUTPUT_MAX_TOKENS,
    )?;
    if !config.agent.env.iter().any(|name| name == &api_key_ref) {
        config.agent.env.push(api_key_ref.clone());
    }
    config.agent.provider = Some(AgentProviderConfig {
        id: provider_id,
        model: Some(model),
        api_key_ref: Some(api_key_ref.clone()),
        custom: Some(AgentCustomProviderConfig {
            name: provider_name,
            base_url,
            api,
            model_name: Some(model_name),
            context,
            output_max_tokens,
        }),
    });
    let canonical = config.to_canonical_toml()?;
    let validated = config::load_config_from_str(&canonical)?;
    *config = validated;
    if config
        .agent
        .provider
        .as_ref()
        .and_then(|provider| provider.custom.as_ref())
        .is_none()
    {
        return Err(StackError::AgentConfigProvision {
            path: config_path.to_path_buf(),
            reason: "custom provider config was not retained".to_owned(),
        });
    }
    Ok(vec![api_key_ref])
}

fn confirm_custom_provider_setup(provider_id: &str) -> Result<bool> {
    if !io::stdin().is_terminal() {
        return Ok(false);
    }
    print!(
        "provider `{provider_id}` has no default API-key env mapping; configure it as a custom provider? [y/N]: "
    );
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

fn required_init_custom_value(field: &'static str, value: Option<String>) -> Result<String> {
    if let Some(value) = value
        && !value.trim().is_empty()
        && value.trim().len() == value.len()
    {
        return Ok(value);
    }
    if !io::stdin().is_terminal() {
        return Err(StackError::InvalidParam {
            field,
            reason: format!("--{field} is required for custom provider init"),
        });
    }
    print!("{field}: ");
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
    let answer = answer.trim().to_owned();
    if answer.is_empty() {
        return Err(StackError::InvalidParam {
            field,
            reason: format!("--{field} is required for custom provider init"),
        });
    }
    Ok(answer)
}

fn default_init_custom_provider_api(agent_id: &str) -> CustomProviderApi {
    if agent_id == "codex" {
        CustomProviderApi::Responses
    } else {
        CustomProviderApi::ChatCompletions
    }
}

fn parse_init_custom_provider_api(
    value: Option<&str>,
    default: CustomProviderApi,
) -> Result<CustomProviderApi> {
    match value {
        None => Ok(default),
        Some("chat-completions") => Ok(CustomProviderApi::ChatCompletions),
        Some("responses") => Ok(CustomProviderApi::Responses),
        Some(_) => Err(StackError::InvalidParam {
            field: "provider-api",
            reason: "must be `chat-completions` or `responses`".to_owned(),
        }),
    }
}

fn parse_init_custom_token_limit(
    field: &'static str,
    value: Option<&str>,
    default: u64,
) -> Result<u64> {
    let Some(value) = value else {
        return Ok(default);
    };
    if value.contains(',') {
        return Err(StackError::InvalidParam {
            field,
            reason: "must be a plain integer without commas".to_owned(),
        });
    }
    let parsed = value.parse::<u64>().map_err(|_| StackError::InvalidParam {
        field,
        reason: "must be a positive integer".to_owned(),
    })?;
    if parsed == 0 {
        return Err(StackError::InvalidParam {
            field,
            reason: "must be greater than 0".to_owned(),
        });
    }
    Ok(parsed)
}

fn collect_missing_provider_refs(
    secret_store: &mut SecretStore,
    required_refs: &[String],
) -> Result<()> {
    if io::stdin().is_terminal() {
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
        )?;
    }
    for env_ref in required_refs {
        if !secret_store.contains(env_ref) {
            return Err(StackError::SecretNotFound {
                name: env_ref.clone(),
            });
        }
    }
    Ok(())
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
    let log_base = crate::state::default_installer_log_base(home);
    if let Some(install) = config.agent.install.as_ref() {
        let env = resolve_agent_env(home, config)?;
        return run_installer(
            &config.agent.id,
            install,
            config.agent.expected_sha256.as_deref(),
            env,
            &workspace_root,
            store,
            Some(&log_base),
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
        Some(&log_base),
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
                    Some(&log_base),
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

fn validate_deployment_overrides_match_existing(args: &InitArgs, config: &Config) -> Result<()> {
    reject_conflicting_deployment_override(
        "--workspace-root",
        args.workspace_root.as_deref(),
        &config.workspace.root,
    )?;
    reject_conflicting_deployment_override(
        "--workspace-uploads",
        args.workspace_uploads.as_deref(),
        &config.workspace.uploads,
    )?;
    reject_conflicting_deployment_override(
        "--runtime-user",
        args.runtime_user.as_deref(),
        &config.workspace.runtime_user,
    )
}

fn reject_conflicting_deployment_override(
    field: &'static str,
    requested: Option<&str>,
    existing: &str,
) -> Result<()> {
    let Some(requested) = requested else {
        return Ok(());
    };
    if requested == existing {
        return Ok(());
    }
    Err(StackError::InvalidParam {
        field,
        reason: format!(
            "deployment override applies only when creating a starter config; existing config has `{existing}`. Edit the config first or re-run with the existing value."
        ),
    })
}

fn starter_config(args: &InitArgs) -> Result<String> {
    let workspace_root = args
        .workspace_root
        .clone()
        .unwrap_or_else(|| config::DEFAULT_WORKSPACE_ROOT.to_owned());
    let workspace_uploads = args.workspace_uploads.clone().unwrap_or_else(|| {
        if args.workspace_root.is_some() {
            Path::new(&workspace_root)
                .join("uploads")
                .display()
                .to_string()
        } else {
            config::DEFAULT_WORKSPACE_UPLOADS.to_owned()
        }
    });
    let runtime_user = args
        .runtime_user
        .clone()
        .unwrap_or_else(|| config::DEFAULT_RUNTIME_USER.to_owned());

    let starter = Config {
        config_version: config::SUPPORTED_CONFIG_VERSION,
        api: ApiConfig {
            bind: config::DEFAULT_API_BIND.to_owned(),
            public_url: Some(format!("http://{}", config::DEFAULT_API_BIND)),
            max_request_bytes: STARTER_MAX_REQUEST_BYTES,
        },
        auth: AuthConfig {
            session_key_ref: STARTER_SESSION_KEY_REF.to_owned(),
            admin_key_ref: STARTER_ADMIN_KEY_REF.to_owned(),
        },
        security: SecurityConfig {
            http: SecurityHttpConfig {
                max_request_bytes: STARTER_MAX_REQUEST_BYTES,
                rate_limit_per_minute: STARTER_RATE_LIMIT_PER_MINUTE,
                burst: STARTER_RATE_LIMIT_BURST,
                auth_failures_per_minute: STARTER_AUTH_FAILURES_PER_MINUTE,
                auth_block_duration: STARTER_AUTH_BLOCK_DURATION.to_owned(),
                allowed_origins: Vec::new(),
                trust_proxy_headers: false,
                trusted_proxies: Vec::new(),
            },
        },
        edge: EdgeConfig::default(),
        workspace: WorkspaceConfig {
            root: workspace_root.clone(),
            uploads: workspace_uploads,
            default_shell: STARTER_DEFAULT_SHELL.to_owned(),
            runtime_user,
            max_file_bytes: STARTER_WORKSPACE_MAX_FILE_BYTES,
            code_sources: code_sources_from_args(args),
            data_sources: data_sources_from_args(args)?,
        },
        logging: LoggingConfig {
            level: STARTER_LOG_LEVEL.to_owned(),
            local_retention_days: STARTER_LOCAL_RETENTION_DAYS,
            supabase: Some(SupabaseLoggingConfig {
                enabled: false,
                url: STARTER_SUPABASE_URL.to_owned(),
                service_role_key_ref: STARTER_SUPABASE_SERVICE_ROLE_KEY_REF.to_owned(),
                schema: STARTER_SUPABASE_SCHEMA.to_owned(),
            }),
        },
        agent: AgentConfig {
            id: STARTER_AGENT_ID.to_owned(),
            name: STARTER_AGENT_NAME.to_owned(),
            command: STARTER_AGENT_COMMAND.to_owned(),
            args: Vec::new(),
            cwd: Some(workspace_root),
            env: Vec::new(),
            expected_sha256: None,
            restart: STARTER_AGENT_RESTART.to_owned(),
            mode: None,
            model: None,
            harness_version: None,
            adapter: None,
            provider: None,
            install: Some(AgentInstallConfig {
                install_type: STARTER_AGENT_INSTALL_TYPE.to_owned(),
                creates: STARTER_AGENT_INSTALL_CREATES.to_owned(),
                shell: Some(STARTER_AGENT_INSTALL_COMMAND.to_owned()),
            }),
        },
        permissions: Default::default(),
        commands: Default::default(),
        dependencies: Default::default(),
        mcp: Default::default(),
        acpctl: Default::default(),
    };

    let canonical = starter.to_canonical_toml()?;
    config::load_config_from_str(&canonical)?;
    Ok(canonical)
}

fn code_sources_from_args(args: &InitArgs) -> Vec<CodeSourceConfig> {
    args.code_from
        .iter()
        .map(|repo| CodeSourceConfig {
            source_type: "git".to_owned(),
            repo: Some(repo.clone()),
            branch: None,
            credential_ref: None,
            name: None,
        })
        .collect()
}

fn data_sources_from_args(args: &InitArgs) -> Result<Vec<DataSourceConfig>> {
    args.data_from
        .iter()
        .map(|value| classify_data_from(value))
        .collect()
}

fn classify_data_from(value: &str) -> Result<DataSourceConfig> {
    if value.strip_prefix("https://").is_some() {
        reject_unsupported_https_data_source(value)?;
        return Ok(DataSourceConfig {
            source_type: "https".to_owned(),
            name: None,
            path: None,
            url: Some(value.to_owned()),
            expected_sha256: None,
            max_download_bytes: None,
            max_extracted_bytes: None,
            bucket: None,
            prefix: None,
            region: None,
            access_key_ref: None,
            secret_key_ref: None,
        });
    }
    if value.starts_with("http://") {
        return Err(StackError::InvalidParam {
            field: "data-from",
            reason: format!("`{value}` must use https:// (http is not allowed)"),
        });
    }
    if !value.starts_with('/') {
        return Err(StackError::InvalidParam {
            field: "data-from",
            reason: format!("`{value}` must be an absolute path or an https:// URL"),
        });
    }
    Ok(DataSourceConfig {
        source_type: "local".to_owned(),
        name: None,
        path: Some(value.to_owned()),
        url: None,
        expected_sha256: None,
        max_download_bytes: None,
        max_extracted_bytes: None,
        bucket: None,
        prefix: None,
        region: None,
        access_key_ref: None,
        secret_key_ref: None,
    })
}

/// Reject HTTPS data sources that the materializer cannot satisfy headlessly.
/// Catches three known failure modes BEFORE init writes any state, so the
/// operator gets a clear error pointing at the actual URL rather than a vague
/// download/extract failure halfway through materialization.
///
/// Patterns rejected:
/// - `drive.google.com/file/d/.../view` (private file view link; needs the
///   `uc?export=download&id=` form to expose a usable HTTPS download)
/// - `drive.google.com/drive/folders/...` (folder, not an archive; the
///   materializer downloads single files)
/// - `dropbox.com/.../?dl=0` or no `dl` param (preview link; needs `?dl=1`)
fn reject_unsupported_https_data_source(value: &str) -> Result<()> {
    let lower = value.to_ascii_lowercase();
    if lower.contains("drive.google.com/file/d/")
        && !lower.contains("uc?export=download")
        && !lower.contains("uc?id=")
    {
        return Err(StackError::InvalidParam {
            field: "data-from",
            reason: format!(
                "`{value}` is a private Drive file viewer link; pass the `https://drive.google.com/uc?export=download&id=<ID>` form instead"
            ),
        });
    }
    if lower.contains("drive.google.com/drive/folders/") {
        return Err(StackError::InvalidParam {
            field: "data-from",
            reason: format!(
                "`{value}` is a Drive folder; init only supports single-archive downloads. Export the folder as an archive and link to the archive."
            ),
        });
    }
    if lower.contains("dropbox.com/") && !lower.contains("dl=1") && !lower.contains("raw=1") {
        return Err(StackError::InvalidParam {
            field: "data-from",
            reason: format!(
                "`{value}` is a Dropbox preview link; append `?dl=1` so the materializer receives the file bytes"
            ),
        });
    }
    Ok(())
}
