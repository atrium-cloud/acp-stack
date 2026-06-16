use std::io::IsTerminal;
use std::path::Path;

use http::header::HeaderName;

use crate::config::{
    self, AgentConfig, AgentInstallConfig, ApiConfig, CodeSourceConfig, Config, DataSourceConfig,
    DependencyEntry, DependencyInstallAction, DependencyInstallScope, EdgeConfig, HttpHeaderRef,
    LoggingConfig, McpConfig, McpHttpServer, McpServerConfig, McpStdioServer, SecurityConfig,
    SecurityHttpConfig, StackUpdatePolicy, SupabaseLoggingConfig, WorkspaceConfig,
    is_valid_secret_ref_name, normalize_day_or_week_duration,
};
use crate::error::{Result, StackError};
use crate::runtime::dependencies::deps_apply::{
    DepApplyCandidate, candidate_summary_line, summarize_candidates,
};
use crate::secrets::SecretStore;

use super::super::logging::{
    SUPABASE_DEFAULT_API_KEY_REF, SUPABASE_DEFAULT_SCHEMA, disabled_supabase_config,
    enabled_supabase_config,
};
use super::{
    InitArgs, STARTER_AGENT_COMMAND, STARTER_AGENT_ID, STARTER_AGENT_INSTALL_COMMAND,
    STARTER_AGENT_INSTALL_CREATES, STARTER_AGENT_INSTALL_TYPE, STARTER_AGENT_NAME,
    STARTER_AGENT_RESTART, STARTER_AUTH_BLOCK_DURATION, STARTER_AUTH_FAILURES_PER_MINUTE,
    STARTER_DEFAULT_SHELL, STARTER_LOCAL_RETENTION_DAYS, STARTER_LOG_LEVEL,
    STARTER_MAX_REQUEST_BYTES, STARTER_RATE_LIMIT_BURST, STARTER_RATE_LIMIT_PER_MINUTE,
    STARTER_WORKSPACE_MAX_FILE_BYTES, prompt, prompts_enabled,
};

pub(super) fn validate_deployment_overrides_match_existing(
    args: &InitArgs,
    config: &Config,
) -> Result<()> {
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

pub(super) fn reject_starter_only_mcp_args_for_existing_config(args: &InitArgs) -> Result<()> {
    reject_starter_only_mcp_arg("--mcp-preset", &args.mcp_preset)?;
    reject_starter_only_mcp_arg("--mcp-stdio", &args.mcp_stdio)?;
    reject_starter_only_mcp_arg("--mcp-stdio-env", &args.mcp_stdio_env)?;
    reject_starter_only_mcp_arg("--mcp-http", &args.mcp_http)?;
    reject_starter_only_mcp_arg("--mcp-http-header", &args.mcp_http_header)
}

/// Operator-supplied agent environment variable references collected during
/// init. `flag_refs` (from `--agent-env-ref`) must already exist in the secret
/// store; `fresh` holds interactively-entered name+value pairs to write.
/// Values are `Zeroizing` and never echoed or recorded in the init run args.
#[derive(Default)]
pub(super) struct AgentEnvCollection {
    flag_refs: Vec<String>,
    fresh: Vec<(String, zeroize::Zeroizing<String>)>,
}

impl AgentEnvCollection {
    /// All declared ref names, flag-provided first then interactive.
    fn ref_names(&self) -> Vec<String> {
        self.flag_refs
            .iter()
            .cloned()
            .chain(self.fresh.iter().map(|(name, _)| name.clone()))
            .collect()
    }
}

/// Reject `--agent-env-ref` when a config already exists; like the other
/// starter-only flags it only applies to a fresh config.
pub(super) fn reject_agent_env_refs_for_existing_config(args: &InitArgs) -> Result<()> {
    if !args.agent_env_ref.is_empty() {
        return Err(StackError::InvalidParam {
            field: "--agent-env-ref",
            reason: "agent env refs can only be set while creating a new config".to_owned(),
        });
    }
    Ok(())
}

/// Collect operator agent environment variable refs from `--agent-env-ref` and,
/// in interactive runs, an add-loop that prompts for each name and a masked
/// value. Flag refs reference secrets that must already exist; interactive
/// entries carry their value for the store write after the secret store opens.
pub(super) fn collect_agent_env_refs_for_init(
    args: &InitArgs,
    interactive: bool,
) -> Result<AgentEnvCollection> {
    let mut flag_refs: Vec<String> = Vec::new();
    for raw in &args.agent_env_ref {
        let name = raw.trim().to_owned();
        if name.is_empty() {
            return Err(StackError::InvalidParam {
                field: "agent-env-ref",
                reason: "secret ref name must not be empty".to_owned(),
            });
        }
        if !is_valid_secret_ref_name(&name) {
            return Err(StackError::InvalidParam {
                field: "agent-env-ref",
                reason: format!(
                    "`{name}` is not a valid secret ref name (letters, digits, and underscore; must not start with a digit)"
                ),
            });
        }
        if !flag_refs.contains(&name) {
            flag_refs.push(name);
        }
    }
    let mut fresh: Vec<(String, zeroize::Zeroizing<String>)> = Vec::new();
    if interactive {
        while prompt::confirm(interactive, "Add an agent environment variable?", false)? {
            let Some(name) =
                prompt::text(interactive, "secret ref name (e.g. GITHUB_TOKEN)", true)?
            else {
                break;
            };
            let name = name.trim().to_owned();
            if name.is_empty() {
                continue;
            }
            if !is_valid_secret_ref_name(&name) {
                println!(
                    "`{name}` is not a valid secret ref name (letters, digits, and underscore; must not start with a digit); skipping."
                );
                continue;
            }
            let Some(value) = prompt::password(interactive, &format!("value for {name}"))? else {
                break;
            };
            if value.is_empty() {
                // Don't store an empty secret for the ref; skip it.
                continue;
            }
            fresh.push((name, zeroize::Zeroizing::new(value)));
        }
    }
    Ok(AgentEnvCollection { flag_refs, fresh })
}

/// Append the collected ref names to `config.agent.env`, de-duplicating against
/// refs already present (e.g. the provider key ref). Returns whether anything
/// was added. Called only after the refs are verified/stored so a run that fails
/// verification never persists an unresolved `agent.env` ref.
pub(super) fn append_agent_env_refs(config: &mut Config, collection: &AgentEnvCollection) -> bool {
    let mut changed = false;
    for name in collection.ref_names() {
        if !config.agent.env.contains(&name) {
            config.agent.env.push(name);
            changed = true;
        }
    }
    changed
}

/// Write interactively-collected env values to the store and verify that every
/// flag-provided ref already resolves. Runs after the secret store is open and
/// before the agent is installed/launched, so `resolve_agent_env` finds them.
pub(super) fn apply_agent_env_collection(
    secret_store: &mut SecretStore,
    collection: &AgentEnvCollection,
) -> Result<()> {
    // Guard the store before writing. `set_many` upserts, so a fresh name that
    // collides would silently overwrite an existing provider/MCP secret.
    for (name, _) in &collection.fresh {
        if !is_valid_secret_ref_name(name) {
            return Err(StackError::InvalidParam {
                field: "agent-env-ref",
                reason: format!(
                    "`{name}` is not a valid secret ref name (letters, digits, and underscore; must not start with a digit)"
                ),
            });
        }
        if secret_store.contains(name) {
            return Err(StackError::InvalidParam {
                field: "agent-env-ref",
                reason: format!(
                    "secret `{name}` already exists in the store; refusing to overwrite it. Choose a new ref name, or update the value with `acps secrets set`."
                ),
            });
        }
    }
    // Only write when there is something to store: `set_many` re-encrypts the
    // whole store (age ciphertext is non-deterministic), so an empty write on a
    // no-change re-run would needlessly rewrite the secret file.
    if !collection.fresh.is_empty() {
        secret_store.set_many(
            collection
                .fresh
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str())),
        )?;
    }
    for name in &collection.flag_refs {
        if !secret_store.contains(name) {
            return Err(StackError::SecretNotFound { name: name.clone() });
        }
    }
    Ok(())
}

fn parse_dep_entry(
    raw: &str,
    scope: DependencyInstallScope,
    flag: &'static str,
) -> Result<DependencyEntry> {
    let Some((name, shell)) = raw.split_once('=') else {
        return Err(StackError::InvalidParam {
            field: flag,
            reason: format!("expected NAME=SHELL, got `{raw}`"),
        });
    };
    let name = name.trim().to_owned();
    let shell = shell.trim().to_owned();
    if name.is_empty() || shell.is_empty() {
        return Err(StackError::InvalidParam {
            field: flag,
            reason: format!("both a name and a shell command are required in `{raw}`"),
        });
    }
    Ok(DependencyEntry {
        name,
        required: true,
        feature: None,
        install: Some(DependencyInstallAction {
            shell,
            creates: None,
            scope,
            timeout_secs: None,
        }),
    })
}

/// Build dependency entries from `--dep` (user scope) and `--dep-system`
/// (system scope) flags. Each is `NAME=SHELL` with an install action.
pub(super) fn deps_from_args(args: &InitArgs) -> Result<Vec<DependencyEntry>> {
    let mut entries = Vec::new();
    for raw in &args.dep {
        entries.push(parse_dep_entry(raw, DependencyInstallScope::User, "--dep")?);
    }
    for raw in &args.dep_system {
        entries.push(parse_dep_entry(
            raw,
            DependencyInstallScope::System,
            "--dep-system",
        )?);
    }
    Ok(entries)
}

/// Append flag-declared dependencies to `config.dependencies.commands`,
/// rejecting a name that is already declared (e.g. an auto-added `cloudflared`).
pub(super) fn push_args_deps_to_config(config: &mut Config, args: &InitArgs) -> Result<()> {
    for entry in deps_from_args(args)? {
        if config
            .dependencies
            .commands
            .iter()
            .any(|existing| existing.name == entry.name)
        {
            return Err(StackError::InvalidParam {
                field: "--dep",
                reason: format!("dependency `{}` is already declared", entry.name),
            });
        }
        config.dependencies.commands.push(entry);
    }
    Ok(())
}

/// `--dep`/`--dep-system` declare into a fresh starter config only; reject them
/// when a config already exists (the operator edits config or uses `acps deps`).
pub(super) fn reject_deps_args_for_existing_config(args: &InitArgs) -> Result<()> {
    for (flag, values) in [("--dep", &args.dep), ("--dep-system", &args.dep_system)] {
        if !values.is_empty() {
            return Err(StackError::InvalidParam {
                field: flag,
                reason: "dependency declarations apply only when creating a starter config"
                    .to_owned(),
            });
        }
    }
    Ok(())
}

/// Decide whether to run the dependency-apply init step. Non-interactive runs
/// require `--deps-apply --deps-apply-yes`; interactive runs summarize the
/// pending actions and confirm (default no). Returns false when there is
/// nothing actionable.
pub(super) fn should_apply_deps_for_init(
    args: &InitArgs,
    candidates: &[DepApplyCandidate],
    interactive: bool,
) -> Result<bool> {
    if candidates.is_empty() {
        return Ok(false);
    }
    if !interactive {
        if args.deps_apply && !args.deps_apply_yes {
            return Err(StackError::InvalidParam {
                field: "--deps-apply",
                reason: "non-interactive dependency apply requires --deps-apply-yes".to_owned(),
            });
        }
        return Ok(args.deps_apply && args.deps_apply_yes);
    }
    if args.deps_apply && args.deps_apply_yes {
        return Ok(true);
    }
    let (count, any_system) = summarize_candidates(candidates);
    println!("dependencies with install actions ({count}):");
    for candidate in candidates {
        println!("  - {}", candidate_summary_line(candidate));
    }
    if any_system {
        println!("note: one or more actions declare scope=system and require root privilege.");
    }
    prompt::confirm(interactive, "Apply these dependencies now?", false)
}

/// Validate an acp-stack auto-update frequency. The minimum granularity is a
/// day, so only `d` (day) and `w` (week) units are accepted — the shared
/// duration parser treats `m` as minutes, so allowing it here would silently
/// schedule sub-day updates. For longer cadences use weeks (e.g. `26w` ≈ 6
/// months).
fn validate_update_frequency(raw: &str) -> Result<String> {
    normalize_day_or_week_duration("stack-update-frequency", raw)
}

fn parse_stack_update_policy(raw: &str) -> Result<StackUpdatePolicy> {
    match raw {
        "on" => Ok(StackUpdatePolicy::Compatible),
        "security" => Ok(StackUpdatePolicy::SecurityCritical),
        "off" => Ok(StackUpdatePolicy::Manual),
        other => Err(StackError::InvalidParam {
            field: "--stack-update",
            reason: format!("expected on|security|off, got `{other}`"),
        }),
    }
}

pub(super) fn validate_stack_update_args(args: &InitArgs) -> Result<()> {
    let policy = args
        .stack_update
        .as_deref()
        .map(parse_stack_update_policy)
        .transpose()?;
    if policy != Some(StackUpdatePolicy::Manual)
        && let Some(raw) = args.stack_update_frequency.as_deref()
    {
        validate_update_frequency(raw)?;
    }
    Ok(())
}

fn prompt_stack_update_policy() -> Result<StackUpdatePolicy> {
    let items = vec![
        (
            StackUpdatePolicy::SecurityCritical,
            "Security updates only".to_owned(),
            "recommended".to_owned(),
        ),
        (
            StackUpdatePolicy::Compatible,
            "On — all compatible updates".to_owned(),
            String::new(),
        ),
        (
            StackUpdatePolicy::Manual,
            "Off — manual updates only".to_owned(),
            String::new(),
        ),
    ];
    Ok(prompt::select(true, "acp-stack auto-update", &items)?
        .unwrap_or(StackUpdatePolicy::SecurityCritical))
}

fn prompt_stack_update_frequency() -> Result<String> {
    #[derive(Clone, PartialEq, Eq)]
    enum FrequencyChoice {
        Daily,
        Weekly,
        Custom,
    }
    let items = vec![
        (
            FrequencyChoice::Daily,
            "Daily (1d)".to_owned(),
            String::new(),
        ),
        (
            FrequencyChoice::Weekly,
            "Weekly (1w)".to_owned(),
            String::new(),
        ),
        (
            FrequencyChoice::Custom,
            "Custom".to_owned(),
            "day/week units, e.g. 3w".to_owned(),
        ),
    ];
    match prompt::select(true, "update frequency", &items)? {
        Some(FrequencyChoice::Weekly) => Ok("1w".to_owned()),
        Some(FrequencyChoice::Custom) => {
            let raw = prompt::text(true, "frequency (e.g. 3w; minimum 1 day)", true)?
                .unwrap_or_else(|| "1d".to_owned());
            validate_update_frequency(&raw)
        }
        // Daily, or a non-interactive/empty select, defaults to daily.
        _ => Ok("1d".to_owned()),
    }
}

/// Configure `[updates.acp_stack]` from `--stack-update`/`--stack-update-frequency`
/// or, interactively, a policy + frequency prompt placed after mode selection.
/// `on` → Compatible, `security` → SecurityCritical, `off` → Manual. A frequency
/// is only collected for non-Manual policies. Returns whether config changed; a
/// non-interactive run with no flags leaves the schema defaults intact.
pub(super) fn configure_stack_update_for_init(
    args: &InitArgs,
    config: &mut Config,
    interactive: bool,
) -> Result<bool> {
    let policy = match args.stack_update.as_deref() {
        Some(raw) => Some(parse_stack_update_policy(raw)?),
        None if interactive => Some(prompt_stack_update_policy()?),
        None => None,
    };
    let Some(policy) = policy else {
        return Ok(false);
    };
    let frequency = if policy == StackUpdatePolicy::Manual {
        None
    } else {
        match args.stack_update_frequency.as_deref() {
            Some(raw) => Some(validate_update_frequency(raw)?),
            None if interactive => Some(prompt_stack_update_frequency()?),
            None => None,
        }
    };

    let mut changed = false;
    if config.updates.acp_stack.policy != policy {
        config.updates.acp_stack.policy = policy;
        changed = true;
    }
    if let Some(frequency) = frequency
        && config.updates.acp_stack.frequency != frequency
    {
        config.updates.acp_stack.frequency = frequency;
        changed = true;
    }
    Ok(changed)
}

pub(super) fn prompt_starter_config_selections_if_needed(args: &mut InitArgs) -> Result<()> {
    let interactive = prompts_enabled(args);
    if !interactive {
        return Ok(());
    }
    prompt_repeated_values(
        interactive,
        "code source git URL",
        "add a code source?",
        "code source git URL",
        &mut args.code_from,
    )?;
    prompt_repeated_values(
        interactive,
        "data source path or HTTPS archive URL",
        "add a data source?",
        "data source path or HTTPS archive URL",
        &mut args.data_from,
    )?;
    prompt_mcp_preset(interactive, args)?;
    prompt_repeated_values(
        interactive,
        "custom stdio MCP server",
        "add a custom stdio MCP server?",
        "stdio MCP server (name=command)",
        &mut args.mcp_stdio,
    )?;
    prompt_repeated_values(
        interactive,
        "custom HTTP MCP server",
        "add a custom HTTP MCP server?",
        "HTTP MCP server (name=https://...)",
        &mut args.mcp_http,
    )?;
    prompt_repeated_values(
        interactive,
        "stdio MCP secret ref",
        "add a stdio MCP secret ref?",
        "stdio MCP secret ref (server=SECRET_REF)",
        &mut args.mcp_stdio_env,
    )?;
    prompt_repeated_values(
        interactive,
        "HTTP MCP header secret ref",
        "add an HTTP MCP header secret ref?",
        "HTTP MCP header secret ref (server=Header:SECRET_REF)",
        &mut args.mcp_http_header,
    )?;
    prompt_deps(interactive, args)?;
    Ok(())
}

/// Interactive add-loop for dependency install actions. Each entry collects a
/// name, an install shell command, and whether it needs system privilege, then
/// stacks onto `--dep`/`--dep-system` so `deps_from_args` consumes it uniformly.
fn prompt_deps(interactive: bool, args: &mut InitArgs) -> Result<()> {
    while prompt::confirm(interactive, "add a dependency to install?", false)? {
        let Some(name) = prompt::text(interactive, "dependency name (e.g. ripgrep)", true)? else {
            break;
        };
        let name = name.trim().to_owned();
        if name.is_empty() {
            continue;
        }
        let Some(shell) = prompt::text(interactive, "install shell command", true)? else {
            break;
        };
        let shell = shell.trim().to_owned();
        if shell.is_empty() {
            continue;
        }
        let entry = format!("{name}={shell}");
        if prompt::confirm(
            interactive,
            "does this need system privilege (sudo)?",
            false,
        )? {
            args.dep_system.push(entry);
        } else {
            args.dep.push(entry);
        }
    }
    Ok(())
}

fn prompt_mcp_preset(interactive: bool, args: &mut InitArgs) -> Result<()> {
    if !args.mcp_preset.is_empty()
        || !prompt::confirm(interactive, "add the Linear MCP preset?", false)?
    {
        return Ok(());
    }
    args.mcp_preset.push("linear".to_owned());
    Ok(())
}

fn prompt_repeated_values(
    interactive: bool,
    label: &str,
    add_prompt: &str,
    value_prompt: &str,
    values: &mut Vec<String>,
) -> Result<()> {
    if !values.is_empty() {
        println!("{label}: already configured ({})", values.len());
        return Ok(());
    }
    // Free-form entries (URLs, name=command, secret refs) have no fixed option
    // set, so this stays an add-loop: confirm to add, then a required text line.
    while prompt::confirm(interactive, add_prompt, false)? {
        match prompt::text(interactive, value_prompt, true)? {
            Some(value) => {
                let value = value.trim().to_owned();
                if !value.is_empty() {
                    values.push(value);
                }
            }
            None => break,
        }
    }
    Ok(())
}

fn reject_starter_only_mcp_arg(field: &'static str, values: &[String]) -> Result<()> {
    if values.is_empty() {
        return Ok(());
    }
    Err(StackError::InvalidParam {
        field,
        reason: "MCP init declarations apply only when creating a starter config".to_owned(),
    })
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

pub(super) fn starter_config(args: &InitArgs) -> Result<String> {
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
    let runtime_user = starter_runtime_user(args)?;

    let starter = Config {
        config_version: config::SUPPORTED_CONFIG_VERSION,
        api: ApiConfig {
            bind: config::DEFAULT_API_BIND.to_owned(),
            public_url: Some(format!("http://{}", config::DEFAULT_API_BIND)),
            max_request_bytes: STARTER_MAX_REQUEST_BYTES,
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
        updates: Default::default(),
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
            supabase: Some(starter_supabase_config(args)),
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
            subagent: None,
            auto_update: None,
            install: Some(AgentInstallConfig {
                install_type: STARTER_AGENT_INSTALL_TYPE.to_owned(),
                creates: STARTER_AGENT_INSTALL_CREATES.to_owned(),
                shell: Some(STARTER_AGENT_INSTALL_COMMAND.to_owned()),
            }),
        },
        permissions: Default::default(),
        commands: Default::default(),
        prompts: Default::default(),
        dependencies: Default::default(),
        mcp: mcp_from_args(args)?,
        acpctl: Default::default(),
    };

    let canonical = starter.to_canonical_toml()?;
    config::load_config_from_str(&canonical)?;
    Ok(canonical)
}

fn starter_supabase_config(args: &InitArgs) -> SupabaseLoggingConfig {
    if args.no_supabase {
        return disabled_supabase_config();
    }
    match args.supabase_url.clone() {
        Some(url) => enabled_supabase_config(
            url,
            Some(
                args.supabase_schema
                    .clone()
                    .unwrap_or_else(|| SUPABASE_DEFAULT_SCHEMA.to_owned()),
            ),
            Some(
                args.supabase_api_key_ref
                    .clone()
                    .unwrap_or_else(|| SUPABASE_DEFAULT_API_KEY_REF.to_owned()),
            ),
        ),
        None => disabled_supabase_config(),
    }
}

fn starter_runtime_user(args: &InitArgs) -> Result<String> {
    if let Some(runtime_user) = args.runtime_user.clone() {
        return Ok(runtime_user);
    }
    if std::io::stdin().is_terminal()
        && crate::ownership::resolve_runtime_user_uid(config::DEFAULT_RUNTIME_USER)
            .map_err(|source| StackError::ServeIo { source })?
            .is_none()
        && crate::ownership::process_euid() != 0
        && let Some(current_user) =
            crate::ownership::current_username().map_err(|source| StackError::ServeIo { source })?
    {
        return Ok(current_user);
    }
    Ok(config::DEFAULT_RUNTIME_USER.to_owned())
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

fn mcp_from_args(args: &InitArgs) -> Result<McpConfig> {
    let mut servers = Vec::new();
    for preset in &args.mcp_preset {
        match preset.as_str() {
            "linear" => servers.push(McpServerConfig::Http(McpHttpServer {
                name: "linear".to_owned(),
                url: "https://mcp.linear.app/mcp".to_owned(),
                headers: vec![HttpHeaderRef {
                    name: "Authorization".to_owned(),
                    value_ref: "LINEAR_API_KEY".to_owned(),
                }],
            })),
            other => {
                return Err(StackError::InvalidParam {
                    field: "mcp-preset",
                    reason: format!("unsupported MCP preset `{other}`"),
                });
            }
        }
    }
    for value in &args.mcp_stdio {
        let (name, command) = split_mcp_pair("mcp-stdio", value)?;
        servers.push(McpServerConfig::Stdio(McpStdioServer {
            name,
            command,
            args: Vec::new(),
            env: Vec::new(),
        }));
    }
    for value in &args.mcp_http {
        let (name, url) = split_mcp_pair("mcp-http", value)?;
        validate_mcp_https_url(&name, &url)?;
        servers.push(McpServerConfig::Http(McpHttpServer {
            name,
            url,
            headers: Vec::new(),
        }));
    }
    apply_mcp_stdio_env_refs(&mut servers, &args.mcp_stdio_env)?;
    apply_mcp_http_headers(&mut servers, &args.mcp_http_header)?;
    Ok(McpConfig { servers })
}

fn apply_mcp_stdio_env_refs(servers: &mut [McpServerConfig], values: &[String]) -> Result<()> {
    for value in values {
        let (server_name, env_ref) = split_mcp_pair("mcp-stdio-env", value)?;
        let server = find_mcp_server_mut(servers, &server_name, "mcp-stdio-env")?;
        match server {
            McpServerConfig::Stdio(stdio) => stdio.env.push(env_ref),
            McpServerConfig::Http(_) => {
                return Err(StackError::InvalidParam {
                    field: "mcp-stdio-env",
                    reason: format!("MCP server `{server_name}` is not a stdio server"),
                });
            }
        }
    }
    Ok(())
}

fn apply_mcp_http_headers(servers: &mut [McpServerConfig], values: &[String]) -> Result<()> {
    for value in values {
        let (server_name, header_ref) = split_mcp_pair("mcp-http-header", value)?;
        let (header_name, value_ref) = split_mcp_header_ref(&header_ref)?;
        let server = find_mcp_server_mut(servers, &server_name, "mcp-http-header")?;
        match server {
            McpServerConfig::Http(http) => {
                if http
                    .headers
                    .iter()
                    .any(|header| header.name.eq_ignore_ascii_case(&header_name))
                {
                    return Err(StackError::InvalidParam {
                        field: "mcp-http-header",
                        reason: format!(
                            "MCP HTTP server `{server_name}` already has header `{header_name}`"
                        ),
                    });
                }
                http.headers.push(HttpHeaderRef {
                    name: header_name,
                    value_ref,
                });
            }
            McpServerConfig::Stdio(_) => {
                return Err(StackError::InvalidParam {
                    field: "mcp-http-header",
                    reason: format!("MCP server `{server_name}` is not an HTTP server"),
                });
            }
        }
    }
    Ok(())
}

fn find_mcp_server_mut<'a>(
    servers: &'a mut [McpServerConfig],
    server_name: &str,
    field: &'static str,
) -> Result<&'a mut McpServerConfig> {
    servers
        .iter_mut()
        .find(|server| server.name() == server_name)
        .ok_or_else(|| StackError::InvalidParam {
            field,
            reason: format!("MCP server `{server_name}` is not declared"),
        })
}

fn split_mcp_pair(field: &'static str, value: &str) -> Result<(String, String)> {
    let Some((name, target)) = value.split_once('=') else {
        return Err(StackError::InvalidParam {
            field,
            reason: format!("`{value}` must use NAME=VALUE"),
        });
    };
    let name = name.trim();
    let target = target.trim();
    if name.is_empty() || target.is_empty() {
        return Err(StackError::InvalidParam {
            field,
            reason: format!("`{value}` must include a non-empty name and value"),
        });
    }
    Ok((name.to_owned(), target.to_owned()))
}

fn split_mcp_header_ref(value: &str) -> Result<(String, String)> {
    let Some((header_name, value_ref)) = value.split_once(':') else {
        return Err(StackError::InvalidParam {
            field: "mcp-http-header",
            reason: format!("`{value}` must use HEADER:SECRET_REF"),
        });
    };
    let header_name = header_name.trim();
    let value_ref = value_ref.trim();
    if header_name.is_empty() || value_ref.is_empty() {
        return Err(StackError::InvalidParam {
            field: "mcp-http-header",
            reason: format!("`{value}` must include a non-empty header and secret ref"),
        });
    }
    HeaderName::from_bytes(header_name.as_bytes()).map_err(|_| StackError::InvalidParam {
        field: "mcp-http-header",
        reason: format!("`{header_name}` is not a valid HTTP header name"),
    })?;
    Ok((header_name.to_owned(), value_ref.to_owned()))
}

fn validate_mcp_https_url(name: &str, url: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(url).map_err(|_| StackError::InvalidParam {
        field: "mcp-http",
        reason: format!("MCP HTTP server `{name}` URL is not valid"),
    })?;
    if parsed.scheme() != "https" || parsed.host_str().is_none() {
        return Err(StackError::InvalidParam {
            field: "mcp-http",
            reason: format!("MCP HTTP server `{name}` must use an https:// URL with a host"),
        });
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(StackError::InvalidParam {
            field: "mcp-http",
            reason: format!("MCP HTTP server `{name}` URL must not include credentials"),
        });
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn collection(fresh: &[(&str, &str)]) -> AgentEnvCollection {
        AgentEnvCollection {
            flag_refs: Vec::new(),
            fresh: fresh
                .iter()
                .map(|(name, value)| {
                    (
                        (*name).to_owned(),
                        zeroize::Zeroizing::new((*value).to_owned()),
                    )
                })
                .collect(),
        }
    }

    // A fresh agent-env name that collides with a secret already in the store
    // must be rejected before the upsert, leaving the existing secret untouched.
    #[test]
    fn apply_agent_env_refuses_to_overwrite_existing_secret() {
        let home = tempdir().expect("tempdir");
        let mut store = SecretStore::open_or_create(home.path()).expect("store");
        store
            .set("ADMIN_KEY", "original-admin-secret")
            .expect("seed");

        let error =
            apply_agent_env_collection(&mut store, &collection(&[("ADMIN_KEY", "attacker")]))
                .expect_err("collision with an existing secret must be rejected");
        assert!(error.to_string().contains("already exists"), "got: {error}");
        assert_eq!(
            store.get("ADMIN_KEY").expect("preserved"),
            "original-admin-secret",
            "the existing secret must not be overwritten"
        );
    }

    #[test]
    fn apply_agent_env_rejects_invalid_ref_name() {
        let home = tempdir().expect("tempdir");
        let mut store = SecretStore::open_or_create(home.path()).expect("store");

        let error = apply_agent_env_collection(&mut store, &collection(&[("bad-name", "v")]))
            .expect_err("an invalid ref name must be rejected");
        assert!(
            error.to_string().contains("valid secret ref name"),
            "got: {error}"
        );
    }

    #[test]
    fn apply_agent_env_stores_a_new_secret() {
        let home = tempdir().expect("tempdir");
        let mut store = SecretStore::open_or_create(home.path()).expect("store");

        apply_agent_env_collection(&mut store, &collection(&[("GITHUB_TOKEN", "ghp_value")]))
            .expect("a new, valid ref should be stored");
        assert_eq!(store.get("GITHUB_TOKEN").expect("stored"), "ghp_value");
    }
}
