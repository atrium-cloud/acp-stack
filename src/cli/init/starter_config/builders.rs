use super::*;

// === CONSTANTS ===
const HEADER_TEMPLATE_SEPARATOR: &str = ":=";
const HEADER_REF_SEPARATOR: char = ':';

pub(crate) fn validate_deployment_overrides_match_existing(
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
    )?;
    reject_conflicting_deployment_override(
        "--sandbox",
        args.sandbox.as_deref(),
        sandbox_mode_str(config.workspace.sandbox.mode),
    )
}

pub(crate) fn reject_starter_only_mcp_args_for_existing_config(args: &InitArgs) -> Result<()> {
    reject_starter_only_mcp_arg("--mcp-preset", &args.mcp_preset)?;
    reject_starter_only_mcp_arg("--mcp-stdio", &args.mcp_stdio)?;
    reject_starter_only_mcp_arg("--mcp-stdio-env", &args.mcp_stdio_env)?;
    reject_starter_only_mcp_arg("--mcp-http", &args.mcp_http)?;
    reject_starter_only_mcp_arg("--mcp-http-header", &args.mcp_http_header)?;
    // Structured declarations (hosted request or wizard) declare into a fresh
    // starter config only, same as the flag forms above.
    if !args.prompt_mcp_stdio.is_empty() {
        return starter_only_mcp_error("--mcp-stdio");
    }
    if !args.prompt_mcp_http.is_empty() {
        return starter_only_mcp_error("--mcp-http");
    }
    Ok(())
}

/// `--data-from` and structured data-source declarations seed a fresh starter
/// config only; reject them when a config already exists (the operator edits
/// `[[workspace.data_sources]]` directly).
pub(crate) fn reject_data_source_args_for_existing_config(args: &InitArgs) -> Result<()> {
    // The structured field only arrives from the hosted request (or wizard),
    // so name the hosted field in that error rather than the CLI flag.
    let field = if !args.data_from.is_empty() {
        Some("--data-from")
    } else if !args.prompt_data_sources.is_empty() {
        Some("data_sources")
    } else {
        None
    };
    if let Some(field) = field {
        return Err(StackError::InvalidParam {
            field,
            reason: "data source declarations apply only when creating a starter config".to_owned(),
        });
    }
    Ok(())
}

fn sandbox_mode_str(mode: SandboxMode) -> &'static str {
    match mode {
        SandboxMode::Off => "off",
        SandboxMode::Unshare => "unshare",
        SandboxMode::Bwrap => "bwrap",
        SandboxMode::Custom => "custom",
    }
}

fn parse_sandbox_mode(raw: &str) -> Result<SandboxMode> {
    match raw {
        "off" => Ok(SandboxMode::Off),
        "unshare" => Ok(SandboxMode::Unshare),
        "bwrap" => Ok(SandboxMode::Bwrap),
        "custom" => Ok(SandboxMode::Custom),
        other => Err(StackError::InvalidParam {
            field: "--sandbox",
            reason: format!("expected off|unshare|bwrap|custom, got `{other}`"),
        }),
    }
}

/// Sandbox config for a freshly-created starter config. Only the `mode` is
/// settable here (the rest of `[workspace.sandbox]` is reserved for hand-edited
/// or imported configs); an absent flag keeps the `off` default so a plain
/// `acps init` is unchanged. `custom` needs a wrapper that this flag cannot
/// supply, so config validation rejects it downstream — fail-fast, by design.
fn sandbox_from_args(args: &InitArgs) -> Result<SandboxConfig> {
    let Some(raw) = args.sandbox.as_deref() else {
        return Ok(SandboxConfig::default());
    };
    Ok(SandboxConfig {
        mode: parse_sandbox_mode(raw)?,
        ..SandboxConfig::default()
    })
}

pub(crate) fn starter_config(args: &InitArgs) -> Result<String> {
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

    let agent = AgentConfig {
        id: STARTER_AGENT_ID.to_owned(),
        name: STARTER_AGENT_NAME.to_owned(),
        command: STARTER_AGENT_COMMAND.to_owned(),
        args: Vec::new(),
        cwd: Some(workspace_root.clone()),
        env: Vec::new(),
        expected_sha256: None,
        restart: STARTER_AGENT_RESTART.to_owned(),
        mode: None,
        model: None,
        effort: None,
        config_options: Default::default(),
        harness_version: None,
        adapter: None,
        adapter_override: None,
        provider: None,
        providers: None,
        subagent: None,
        auto_update: None,
        install: Some(AgentInstallConfig {
            install_type: STARTER_AGENT_INSTALL_TYPE.to_owned(),
            creates: STARTER_AGENT_INSTALL_CREATES.to_owned(),
            shell: Some(STARTER_AGENT_INSTALL_COMMAND.to_owned()),
        }),
    };
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
            sandbox: sandbox_from_args(args)?,
            max_file_bytes: STARTER_WORKSPACE_MAX_FILE_BYTES,
            code_sources: code_sources_from_args(args),
            data_sources: data_sources_from_args(args)?,
        },
        logging: LoggingConfig {
            level: STARTER_LOG_LEVEL.to_owned(),
            local_retention_days: STARTER_LOCAL_RETENTION_DAYS,
            supabase: Some(starter_supabase_config(args)),
        },
        agent: agent.clone(),
        array: config::ArrayConfig::from_agent(agent),
        permissions: Default::default(),
        commands: Default::default(),
        prompts: Default::default(),
        dependencies: Default::default(),
        mcp: mcp_from_args(args)?,
        skills: Default::default(),
        local: Default::default(),
        extensions: Default::default(),
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
    let mut sources: Vec<DataSourceConfig> = args
        .data_from
        .iter()
        .map(|value| classify_data_from(value))
        .collect::<Result<_>>()?;
    sources.extend(args.prompt_data_sources.iter().cloned());
    Ok(sources)
}

fn mcp_from_args(args: &InitArgs) -> Result<McpConfig> {
    let mut servers = Vec::new();
    for preset in &args.mcp_preset {
        match preset.as_str() {
            "linear" => servers.push(McpServerConfig::Http(McpHttpServer {
                name: "linear".to_owned(),
                url: "https://mcp.linear.app/mcp".to_owned(),
                headers: vec![HttpHeaderRef::from_ref("Authorization", "LINEAR_API_KEY")],
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
    servers.extend(mcp_servers_from_prompted(&args.prompt_mcp_stdio, &[])?);
    for value in &args.mcp_http {
        let (name, url) = split_mcp_pair("mcp-http", value)?;
        validate_mcp_https_url(&name, &url)?;
        servers.push(McpServerConfig::Http(McpHttpServer {
            name,
            url,
            headers: Vec::new(),
        }));
    }
    servers.extend(mcp_servers_from_prompted(&[], &args.prompt_mcp_http)?);
    apply_mcp_stdio_env_refs(&mut servers, &args.mcp_stdio_env)?;
    apply_mcp_http_headers(&mut servers, &args.mcp_http_header)?;
    Ok(McpConfig { servers })
}

/// Convert prompt-collected MCP rows into config servers. Shared between the
/// starter-config build (flag lane) and the post-probe `mcp_configure` step
/// (interactive lane) so validation cannot drift: the lenient config loader
/// silently drops invalid servers, so a bad URL must be rejected here where
/// the operator can still see it.
pub(in crate::cli::init) fn mcp_servers_from_prompted(
    stdio: &[InitMcpStdioServer],
    http: &[InitMcpHttpServer],
) -> Result<Vec<McpServerConfig>> {
    let mut servers = Vec::new();
    for value in stdio {
        servers.push(McpServerConfig::Stdio(McpStdioServer {
            name: value.name.clone(),
            command: value.command.clone(),
            args: value.args.clone(),
            env: value.env.clone(),
        }));
    }
    for value in http {
        validate_mcp_https_url(&value.name, &value.url)?;
        servers.push(McpServerConfig::Http(McpHttpServer {
            name: value.name.clone(),
            url: value.url.clone(),
            headers: value
                .headers
                .iter()
                .map(|header| HttpHeaderRef {
                    name: header.name.clone(),
                    value_ref: header.value_ref.clone(),
                    value: header.value.clone(),
                })
                .collect(),
        }));
    }
    Ok(servers)
}

/// Merge interactively-added servers into the config, rejecting names that
/// collide with an existing or previously-added server. Returns the added
/// names in order for the step payload.
pub(in crate::cli::init) fn merge_prompted_mcp_servers(
    existing: &mut Vec<McpServerConfig>,
    new_servers: Vec<McpServerConfig>,
) -> Result<Vec<String>> {
    let mut added = Vec::new();
    for server in &new_servers {
        let name = server.name();
        if added.iter().any(|existing: &String| existing == name)
            || existing.iter().any(|server| server.name() == name)
        {
            return Err(StackError::InvalidParam {
                field: "mcp",
                reason: format!("duplicate MCP server name `{name}`"),
            });
        }
        added.push(name.to_owned());
    }
    existing.extend(new_servers);
    Ok(added)
}

fn apply_mcp_stdio_env_refs(servers: &mut [McpServerConfig], values: &[String]) -> Result<()> {
    for value in values {
        let (server_name, env_entry) = split_mcp_pair("mcp-stdio-env", value)?;
        crate::config::screen_env_entry("mcp-stdio-env", &env_entry)?;
        crate::config::parse_env_entry("mcp-stdio-env", &env_entry)?;
        let server = find_mcp_server_mut(servers, &server_name, "mcp-stdio-env")?;
        match server {
            McpServerConfig::Stdio(stdio) => stdio.env.push(env_entry),
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
        let header = split_mcp_header_ref(&header_ref)?;
        let server = find_mcp_server_mut(servers, &server_name, "mcp-http-header")?;
        match server {
            McpServerConfig::Http(http) => {
                if http
                    .headers
                    .iter()
                    .any(|existing| existing.name.eq_ignore_ascii_case(&header.name))
                {
                    return Err(StackError::InvalidParam {
                        field: "mcp-http-header",
                        reason: format!(
                            "MCP HTTP server `{server_name}` already has header `{}`",
                            header.name
                        ),
                    });
                }
                http.headers.push(header);
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
        // Screened before the echo, on the same grounds as
        // `split_mcp_header_ref`: an entry that never split is the one shape
        // that can be a bare pasted credential.
        crate::config::screen_ref_name(field, value)?;
        return Err(StackError::InvalidParam {
            field,
            reason: "entry must use NAME=VALUE".to_owned(),
        });
    };
    let name = name.trim();
    let target = target.trim();
    if name.is_empty() || target.is_empty() {
        // A composite paste splits somewhere the screen does not recognize —
        // base64 padding leaves an empty target — so, like the arm above, the
        // complaint states the shape rather than repeating what arrived.
        crate::config::screen_ref_name(field, value)?;
        return Err(StackError::InvalidParam {
            field,
            reason: "entry must include a non-empty name and value".to_owned(),
        });
    }
    Ok((name.to_owned(), target.to_owned()))
}

/// Split a `HEADER:SECRET_REF` (whole-value ref) or `HEADER:=TEMPLATE`
/// (interpolated value) declaration. `:=` is unambiguous because an HTTP
/// header name can contain neither `:` nor `=`, while templates contain `:`
/// freely (`Bearer ${X}` does not, but URLs do).
pub(super) fn split_mcp_header_ref(value: &str) -> Result<HttpHeaderRef> {
    let (header_name, header_value, is_template) =
        if let Some((header_name, template)) = value.split_once(HEADER_TEMPLATE_SEPARATOR) {
            (header_name, template, true)
        } else if let Some((header_name, value_ref)) = value.split_once(HEADER_REF_SEPARATOR) {
            (header_name, value_ref, false)
        } else {
            // A whole entry with no separator is most often the credential
            // pasted where its ref name belongs, so it is screened before the
            // shape complaint — and the complaint describes the expected shape
            // rather than repeating what arrived. Both errors reach the client
            // as an init failure reason and stay in replayable history.
            crate::config::screen_ref_name("mcp-http-header", value)?;
            return Err(StackError::InvalidParam {
                field: "mcp-http-header",
                reason: "MCP HTTP header must use HEADER:SECRET_REF or HEADER:=TEMPLATE".to_owned(),
            });
        };
    let header_name = header_name.trim();
    let header_value = header_value.trim();
    if header_name.is_empty() || header_value.is_empty() {
        return Err(StackError::InvalidParam {
            field: "mcp-http-header",
            reason: "MCP HTTP header must include a non-empty header and value".to_owned(),
        });
    }
    // A paste that happens to contain a colon lands its head in the header
    // position. Screening catches the shapes it recognizes, and the validity
    // error describes the constraint rather than quoting the name back, so a
    // credential the heuristic does not recognize still cannot ride the error
    // out to the terminal frame and replayable history.
    crate::config::screen_ref_name("mcp-http-header", header_name)?;
    HeaderName::from_bytes(header_name.as_bytes()).map_err(|_| StackError::InvalidParam {
        field: "mcp-http-header",
        reason: "MCP HTTP header name is not a valid HTTP header name; it must be a token with no spaces, separators, or control characters".to_owned(),
    })?;
    if is_template {
        crate::config::screen_template("mcp-http-header", header_value)?;
        crate::config::SecretTemplate::parse("mcp-http-header", header_value)?;
        Ok(HttpHeaderRef::from_template(header_name, header_value))
    } else {
        crate::config::screen_ref_name("mcp-http-header", header_value)?;
        Ok(HttpHeaderRef::from_ref(header_name, header_value))
    }
}

fn validate_mcp_https_url(name: &str, url: &str) -> Result<()> {
    crate::config::validate_mcp_http_url("mcp-http", name, url)
}

pub(super) fn classify_data_from(value: &str) -> Result<DataSourceConfig> {
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

fn reject_starter_only_mcp_arg(field: &'static str, values: &[String]) -> Result<()> {
    if values.is_empty() {
        return Ok(());
    }
    starter_only_mcp_error(field)
}

fn starter_only_mcp_error(field: &'static str) -> Result<()> {
    Err(StackError::InvalidParam {
        field,
        reason: "MCP init declarations apply only when creating a starter config".to_owned(),
    })
}
