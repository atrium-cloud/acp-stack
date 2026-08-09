use super::*;

/// Interactive Daily/Weekly/Custom frequency picker shared by the stack and
/// agent update prompts. A custom value is validated against `field` using the
/// consumer's `limits` (stack and agent updaters declare their own).
fn prompt_update_frequency(field: &'static str, limits: &DurationLimits) -> Result<String> {
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
            format!(
                "e.g. {}; minimum {}",
                limits.examples(),
                limits.render_minimum()
            ),
        ),
    ];
    match prompt::select(true, "update frequency", &items)? {
        Some(FrequencyChoice::Weekly) => Ok("1w".to_owned()),
        Some(FrequencyChoice::Custom) => {
            let raw = prompt::text(
                true,
                &format!(
                    "frequency (e.g. {}; minimum {})",
                    limits.examples(),
                    limits.render_minimum()
                ),
                true,
            )?
            .unwrap_or_else(|| "1d".to_owned());
            normalize_duration(field, &raw, limits)
        }
        // Daily, or a non-interactive/empty select, defaults to daily.
        _ => Ok("1d".to_owned()),
    }
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

pub(crate) fn validate_stack_update_args(args: &InitArgs) -> Result<()> {
    let policy = args
        .stack_update
        .as_deref()
        .map(parse_stack_update_policy)
        .transpose()?;
    if policy != Some(StackUpdatePolicy::Manual)
        && let Some(raw) = args.stack_update_frequency.as_deref()
    {
        normalize_duration(
            "stack-update-frequency",
            raw,
            &STACK_UPDATE_FREQUENCY_LIMITS,
        )?;
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

/// Configure `[updates.acp_stack]` from `--stack-update`/`--stack-update-frequency`
/// or, interactively, a policy + frequency prompt placed after model selection.
/// `on` → Compatible, `security` → SecurityCritical, `off` → Manual. A frequency
/// is only collected for non-Manual policies. Returns whether config changed; a
/// non-interactive run with no flags leaves the schema defaults intact.
pub(crate) fn configure_stack_update_for_init(
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
            Some(raw) => Some(normalize_duration(
                "stack-update-frequency",
                raw,
                &STACK_UPDATE_FREQUENCY_LIMITS,
            )?),
            None if interactive => Some(prompt_update_frequency(
                "stack-update-frequency",
                &STACK_UPDATE_FREQUENCY_LIMITS,
            )?),
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

/// Rejection reason when `--agent-update on` targets a custom/escape-hatch agent
/// the managed updater cannot drive. Same clauses `acps agent update set` uses
/// (it prefixes the agent id) so the two surfaces read the same.
const AGENT_UPDATE_UNMANAGED_REASON: &str = "this agent is not a managed registry agent; auto-update is unavailable for escape-hatch \
     installs";

fn parse_agent_update_choice(raw: &str) -> Result<bool> {
    match raw {
        "on" => Ok(true),
        "off" => Ok(false),
        other => Err(StackError::InvalidParam {
            field: "--agent-update",
            reason: format!("expected on|off, got `{other}`"),
        }),
    }
}

pub(crate) fn validate_agent_update_args(args: &InitArgs) -> Result<()> {
    let enabled = args
        .agent_update
        .as_deref()
        .map(parse_agent_update_choice)
        .transpose()?;
    // A `--custom-agent-id` conflicts with `--agent` at the clap layer, so its
    // presence means the run targets a custom agent. Reject `--agent-update on`
    // here — before key generation and install — rather than only at the late
    // configure step. Custom configs from `--from-file` are still caught there.
    if enabled == Some(true) && args.custom_agent_id.is_some() {
        return Err(StackError::InvalidParam {
            field: "--agent-update",
            reason: AGENT_UPDATE_UNMANAGED_REASON.to_owned(),
        });
    }
    if enabled == Some(true)
        && let Some(raw) = args.agent_update_frequency.as_deref()
    {
        normalize_duration(
            "agent-update-frequency",
            raw,
            &AGENT_UPDATE_FREQUENCY_LIMITS,
        )?;
    }
    Ok(())
}

fn prompt_agent_update_enabled() -> Result<bool> {
    prompt::confirm(true, "Auto-update this agent's harness?", true)
}

/// Configure `[agent.auto_update]` from `--agent-update`/`--agent-update-frequency`
/// or, interactively, an enable + frequency prompt. `managed` is whether the
/// resolved agent is a registry agent (source of truth is the registry, not block
/// presence — a re-init or imported config may lack the block that
/// `apply_registry_entry_to_config` normally seeds). For a managed agent the block
/// is created when absent so the choice is always honored, mirroring
/// `acps agent update set`. A custom/escape-hatch agent cannot be managed-updated:
/// an explicit `--agent-update on` is rejected, `off` strips any stale
/// hand-written block, and the interactive prompt is skipped. Returns whether
/// config changed.
pub(crate) fn configure_agent_update_for_init(
    args: &InitArgs,
    config: &mut Config,
    managed: bool,
    interactive: bool,
) -> Result<bool> {
    let enabled = match args.agent_update.as_deref() {
        Some(raw) => Some(parse_agent_update_choice(raw)?),
        None if interactive && managed => Some(prompt_agent_update_enabled()?),
        None => None,
    };
    let Some(enabled) = enabled else {
        return Ok(false);
    };
    if !managed {
        // Unmanaged agent: enabling is impossible. Disabling needs no block, so
        // drop any stale hand-written one — left in place, the daemon would keep
        // recording `agent.update.skipped` for it every cycle.
        if enabled {
            return Err(StackError::InvalidParam {
                field: "--agent-update",
                reason: AGENT_UPDATE_UNMANAGED_REASON.to_owned(),
            });
        }
        return Ok(config.agent.auto_update.take().is_some());
    }

    let frequency = if enabled {
        match args.agent_update_frequency.as_deref() {
            Some(raw) => Some(normalize_duration(
                "agent-update-frequency",
                raw,
                &AGENT_UPDATE_FREQUENCY_LIMITS,
            )?),
            None if interactive => Some(prompt_update_frequency(
                "agent-update-frequency",
                &AGENT_UPDATE_FREQUENCY_LIMITS,
            )?),
            None => None,
        }
    } else {
        None
    };

    let created = config.agent.auto_update.is_none();
    let auto_update = config
        .agent
        .auto_update
        .get_or_insert_with(|| AgentAutoUpdateConfig {
            enabled,
            frequency: DEFAULT_AGENT_AUTO_UPDATE_FREQUENCY.to_owned(),
        });

    let mut changed = created;
    if auto_update.enabled != enabled {
        auto_update.enabled = enabled;
        changed = true;
    }
    if let Some(frequency) = frequency
        && auto_update.frequency != frequency
    {
        auto_update.frequency = frequency;
        changed = true;
    }
    Ok(changed)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EnvironmentSetupPath {
    Standard,
    Advanced,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum McpTransportChoice {
    Stdio,
    Http,
    Done,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SetupRowAction {
    AddAnother,
    Discard,
    Done,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DataSourceKind {
    Local,
    Https,
    S3,
}

pub(crate) fn prompt_environment_configuration_if_needed(
    args: &mut InitArgs,
    registry: &RegistryCatalog,
    skill_catalog: &SkillCatalog,
) -> Result<()> {
    let interactive = prompts_enabled(args);
    if !interactive {
        return Ok(());
    }
    let setup_path = prompt::select(
        interactive,
        "Environment configuration",
        &[
            (
                EnvironmentSetupPath::Standard,
                "Standard Setup".to_owned(),
                "Opinionated defaults: essential dependencies, browser-use, skills, data sources"
                    .to_owned(),
            ),
            (
                EnvironmentSetupPath::Advanced,
                "Advanced Setup".to_owned(),
                "Clean slate: custom dependencies, skills, MCP, agent env, data sources".to_owned(),
            ),
        ],
    )?;
    // An interactive terminal always yields a choice (Esc aborts init upstream),
    // so `None` only arises when a hosted driver leaves this out-of-v1-scope
    // prompt unhandled; that skips environment configuration like a
    // non-interactive run rather than failing.
    match setup_path {
        Some(EnvironmentSetupPath::Standard) => {
            prompt_standard_setup(interactive, args, registry, skill_catalog)
        }
        Some(EnvironmentSetupPath::Advanced) => prompt_advanced_setup(interactive, args, registry),
        None => Ok(()),
    }
}

// Standard Setup: up to four opinionated prompts. Declining every offered one is
// the intended "set it up later" path, so there is deliberately no separate
// skip option.
fn prompt_standard_setup(
    interactive: bool,
    args: &mut InitArgs,
    registry: &RegistryCatalog,
    skill_catalog: &SkillCatalog,
) -> Result<()> {
    if prompt::confirm(interactive, "Install essential dependencies?", true)? {
        args.standard_agent_work_deps = true;
    }
    if prompt::confirm(interactive, "Install browser-use?", false)? {
        args.browser_use_profile = true;
    }
    if agent_supports_skills(args, registry)
        && essential_agent_skills_available(skill_catalog)
        && !args.no_skills
        && !args.essential_skills
        && args.skills_source.is_none()
        && args.skills.is_empty()
        && prompt::confirm(interactive, "Add essential agent skills?", false)?
    {
        apply_essential_agent_skills(args, skill_catalog);
    }
    if args.data_from.is_empty()
        && args.prompt_data_sources.is_empty()
        && prompt::confirm(interactive, "Add data sources now?", false)?
    {
        prompt_data_sources(interactive, args)?;
    }
    Ok(())
}

// Advanced Setup: a clean slate of opt-in prompts. Each is gated on the
// matching values not already arriving by flag, so flags suppress re-prompts.
// MCP is deliberately absent here: MCP support is only knowable from the
// installed agent's ACP advertisement, so those prompts run in the
// post-install `mcp_configure` step instead.
fn prompt_advanced_setup(
    interactive: bool,
    args: &mut InitArgs,
    registry: &RegistryCatalog,
) -> Result<()> {
    if args.dep.is_empty()
        && args.dep_system.is_empty()
        && prompt::confirm(interactive, "Add custom dependencies?", false)?
    {
        prompt_deps(interactive, args)?;
    }
    if agent_supports_skills(args, registry)
        && !args.no_skills
        && !args.essential_skills
        && args.skills_source.is_none()
        && args.skills.is_empty()
        && prompt::confirm(interactive, "Add agent skills?", false)?
    {
        args.prompt_skills = true;
    }
    if args.agent_env_ref.is_empty()
        && prompt::confirm(interactive, "Add agent environment variables?", false)?
    {
        args.prompt_agent_env_refs = true;
    }
    if args.data_from.is_empty()
        && args.prompt_data_sources.is_empty()
        && prompt::confirm(interactive, "Add data sources now?", false)?
    {
        prompt_data_sources(interactive, args)?;
    }
    Ok(())
}

// "Add MCP servers" spans both transports: the operator picks a transport,
// adds rows for it, and repeats until choosing Done. Driven by the post-probe
// `mcp_configure` step; `offer_http` reflects whether the agent advertised
// `mcpCapabilities.http`, so the select never offers a transport that would
// be skipped at session time.
pub(in crate::cli::init) fn prompt_mcp_servers(
    interactive: bool,
    args: &mut InitArgs,
    offer_http: bool,
) -> Result<()> {
    loop {
        let mut items = vec![(
            McpTransportChoice::Stdio,
            "stdio server".to_owned(),
            "Local command, args, env refs".to_owned(),
        )];
        if offer_http {
            items.push((
                McpTransportChoice::Http,
                "HTTP server".to_owned(),
                "Remote URL and header refs".to_owned(),
            ));
        }
        items.push((
            McpTransportChoice::Done,
            "Done".to_owned(),
            "Finish adding MCP servers".to_owned(),
        ));
        let choice = prompt::select(interactive, "MCP transport", &items)?;
        match choice {
            Some(McpTransportChoice::Stdio) => prompt_mcp_stdio_servers(interactive, args)?,
            Some(McpTransportChoice::Http) => prompt_mcp_http_servers(interactive, args)?,
            Some(McpTransportChoice::Done) | None => break,
        }
    }
    Ok(())
}

fn agent_supports_skills(args: &InitArgs, registry: &RegistryCatalog) -> bool {
    let Some(agent_id) = args.agent.as_deref() else {
        return false;
    };
    registry.lookup(agent_id).is_some_and(|entry| {
        entry.supports_agent_skills && entry.agent_skills_install_dir.is_some()
    })
}

fn essential_agent_skills_available(skill_catalog: &SkillCatalog) -> bool {
    skill_catalog
        .sources()
        .iter()
        .any(|source| !source.essential_skills.is_empty())
}

fn apply_essential_agent_skills(args: &mut InitArgs, _skill_catalog: &SkillCatalog) {
    args.essential_skills = true;
}

fn prompt_data_sources(interactive: bool, args: &mut InitArgs) -> Result<()> {
    loop {
        let Some(kind) = prompt::select(
            interactive,
            "data source type",
            &[
                (
                    DataSourceKind::S3,
                    "S3 bucket".to_owned(),
                    "Bucket, region, credential refs".to_owned(),
                ),
                (
                    DataSourceKind::Https,
                    "HTTPS archive".to_owned(),
                    "Download URL".to_owned(),
                ),
                (
                    DataSourceKind::Local,
                    "Local path".to_owned(),
                    "Absolute path".to_owned(),
                ),
            ],
        )?
        else {
            break;
        };
        let Some(source) = prompt_data_source_row(interactive, kind)? else {
            break;
        };
        match prompt_setup_row_action(interactive, "Data source row")? {
            SetupRowAction::AddAnother => args.prompt_data_sources.push(source),
            SetupRowAction::Discard => continue,
            SetupRowAction::Done => {
                args.prompt_data_sources.push(source);
                break;
            }
        }
    }
    Ok(())
}

fn prompt_data_source_row(
    interactive: bool,
    kind: DataSourceKind,
) -> Result<Option<DataSourceConfig>> {
    match kind {
        DataSourceKind::Local => {
            let Some(path) = prompt::text(interactive, "local path (blank to finish)", false)?
            else {
                return Ok(None);
            };
            let path = path.trim();
            if path.is_empty() {
                return Ok(None);
            }
            classify_data_from(path).map(Some)
        }
        DataSourceKind::Https => {
            let Some(url) =
                prompt::text(interactive, "HTTPS archive URL (blank to finish)", false)?
            else {
                return Ok(None);
            };
            let url = url.trim();
            if url.is_empty() {
                return Ok(None);
            }
            classify_data_from(url).map(Some)
        }
        DataSourceKind::S3 => {
            let Some(bucket) = prompt::text(interactive, "S3 bucket (blank to finish)", false)?
            else {
                return Ok(None);
            };
            let bucket = bucket.trim().to_owned();
            if bucket.is_empty() {
                return Ok(None);
            }
            let Some(region) = prompt::text(interactive, "S3 region", true)? else {
                return Ok(None);
            };
            let region = region.trim().to_owned();
            let Some(access_key_ref) = prompt::text(
                interactive,
                "access key ref (e.g., AWS_ACCESS_KEY_ID)",
                true,
            )?
            else {
                return Ok(None);
            };
            let access_key_ref = access_key_ref.trim().to_owned();
            let Some(secret_key_ref) = prompt::text(
                interactive,
                "secret key ref (e.g., AWS_SECRET_ACCESS_KEY)",
                true,
            )?
            else {
                return Ok(None);
            };
            let secret_key_ref = secret_key_ref.trim().to_owned();
            let prefix = prompt::text(interactive, "S3 prefix (blank for bucket root)", false)?
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty());
            Ok(Some(DataSourceConfig {
                source_type: "s3".to_owned(),
                name: None,
                path: None,
                url: None,
                expected_sha256: None,
                max_download_bytes: None,
                max_extracted_bytes: None,
                bucket: Some(bucket),
                prefix,
                region: Some(region),
                access_key_ref: Some(access_key_ref),
                secret_key_ref: Some(secret_key_ref),
            }))
        }
    }
}

fn prompt_mcp_stdio_servers(interactive: bool, args: &mut InitArgs) -> Result<()> {
    loop {
        let Some(name) = prompt::text(interactive, "MCP name (blank to finish)", false)? else {
            break;
        };
        let name = name.trim().to_owned();
        if name.is_empty() {
            break;
        }
        let Some(command) = prompt::text(interactive, "command", true)? else {
            break;
        };
        let command = command.trim().to_owned();
        if command.is_empty() {
            continue;
        }
        let cli_args =
            match prompt::text(interactive, "args (comma-separated, blank for none)", false)? {
                Some(raw) => parse_comma_separated_prompt_values(&raw),
                None => Vec::new(),
            };
        let env = match prompt::text(
            interactive,
            "env refs (comma-separated, blank for none)",
            false,
        )? {
            Some(raw) => parse_secret_ref_prompt_values("mcp-stdio-env", &raw)?,
            None => Vec::new(),
        };
        let row = InitMcpStdioServer {
            name,
            command,
            args: cli_args,
            env,
        };
        match prompt_setup_row_action(interactive, "MCP row")? {
            SetupRowAction::AddAnother => args.prompt_mcp_stdio.push(row),
            SetupRowAction::Discard => continue,
            SetupRowAction::Done => {
                args.prompt_mcp_stdio.push(row);
                break;
            }
        }
    }
    Ok(())
}

fn prompt_mcp_http_servers(interactive: bool, args: &mut InitArgs) -> Result<()> {
    loop {
        let Some(name) = prompt::text(interactive, "MCP name (blank to finish)", false)? else {
            break;
        };
        let name = name.trim().to_owned();
        if name.is_empty() {
            break;
        }
        let Some(url) = prompt::text(interactive, "URL", true)? else {
            break;
        };
        let url = url.trim().to_owned();
        if url.is_empty() {
            continue;
        }
        let headers = match prompt::text(
            interactive,
            "headers (comma-separated Header:SECRET_REF, blank for none)",
            false,
        )? {
            Some(raw) => parse_http_header_prompt_values(&raw)?,
            None => Vec::new(),
        };
        let row = InitMcpHttpServer { name, url, headers };
        match prompt_setup_row_action(interactive, "MCP row")? {
            SetupRowAction::AddAnother => args.prompt_mcp_http.push(row),
            SetupRowAction::Discard => continue,
            SetupRowAction::Done => {
                args.prompt_mcp_http.push(row);
                break;
            }
        }
    }
    Ok(())
}

fn prompt_setup_row_action(interactive: bool, prompt_label: &str) -> Result<SetupRowAction> {
    let items = [
        (
            SetupRowAction::AddAnother,
            "Add another".to_owned(),
            "Save this row and continue".to_owned(),
        ),
        (
            SetupRowAction::Discard,
            "Discard".to_owned(),
            "Drop this row and continue".to_owned(),
        ),
        (
            SetupRowAction::Done,
            "Done".to_owned(),
            "Save this row and finish".to_owned(),
        ),
    ];
    Ok(prompt::select(interactive, prompt_label, &items)?.unwrap_or(SetupRowAction::Done))
}

fn parse_secret_ref_prompt_values(field: &'static str, raw: &str) -> Result<Vec<String>> {
    let values = parse_comma_separated_prompt_values(raw);
    for value in &values {
        // Screening runs first so a pasted credential is redacted rather
        // than echoed by the name-shape errors below. Bare `NAME` keeps the
        // fast-fail identifier check; `VAR=template` entries get the full
        // template validation. Comma-splitting above means a template
        // containing `,` is unrepresentable in the wizard; the flag and
        // hosted forms carry those.
        crate::config::screen_env_entry(field, value)?;
        if value.contains('=') {
            crate::config::parse_env_entry(field, value)?;
        } else if !is_valid_secret_ref_name(value) {
            return Err(StackError::InvalidParam {
                field,
                reason: format!(
                    "`{value}` is not a valid secret ref name (letters, digits, and underscore; must not start with a digit)"
                ),
            });
        }
    }
    Ok(values)
}

fn parse_http_header_prompt_values(raw: &str) -> Result<Vec<InitMcpHttpHeader>> {
    let mut out = Vec::new();
    for value in parse_comma_separated_prompt_values(raw) {
        let header = split_mcp_header_ref(&value)?;
        out.push(InitMcpHttpHeader {
            name: header.name,
            value_ref: header.value_ref,
            value: header.value,
        });
    }
    Ok(out)
}

fn parse_comma_separated_prompt_values(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Interactive add-loop for dependency install actions. Each entry collects a
/// name, an install shell command, and whether it needs system privilege, then
/// stacks onto `--dep`/`--dep-system` so `deps_from_args` consumes it uniformly.
fn prompt_deps(interactive: bool, args: &mut InitArgs) -> Result<()> {
    loop {
        let Some(name) = prompt::text(interactive, "dependency name (blank to finish)", false)?
        else {
            break;
        };
        let name = name.trim().to_owned();
        if name.is_empty() {
            break;
        }
        let Some(shell) = prompt::text(interactive, "install shell command", true)? else {
            break;
        };
        let shell = shell.trim().to_owned();
        if shell.is_empty() {
            continue;
        }
        let entry = format!("{name}={shell}");
        let scope = prompt::select(
            interactive,
            "dependency scope",
            &[
                (
                    DependencyInstallScope::User,
                    "User".to_owned(),
                    "Runtime user install".to_owned(),
                ),
                (
                    DependencyInstallScope::System,
                    "System".to_owned(),
                    "Requires OS privilege".to_owned(),
                ),
            ],
        )?
        .unwrap_or_default();
        match scope {
            DependencyInstallScope::User => args.dep.push(entry),
            DependencyInstallScope::System => args.dep_system.push(entry),
        }
    }
    Ok(())
}
