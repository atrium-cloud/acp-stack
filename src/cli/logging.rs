use std::io::{BufRead as _, IsTerminal as _, Write as _};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use base64::Engine as _;
use clap::{Args, Subcommand};
use rand::RngExt;
use serde_json::Value;

use crate::config::{
    self, Config, SupabaseLoggingBackend, SupabaseLoggingConfig, is_valid_secret_ref_name,
};
use crate::error::{Result, StackError};
use crate::fs_util::{atomic_write_owner_only, home_dir};
use crate::runtime::logging::supabase_mirror::{
    MIRRORED_TABLES, SUPABASE_DEFAULT_DB_URL_REF,
    SUPABASE_DEFAULT_SCHEMA as SUPABASE_POSTGRES_DEFAULT_SCHEMA, SUPABASE_DEFAULT_TABLE_PREFIX,
    SUPABASE_WRITER_ROLE, canary_event, setup_sql,
};
use crate::runtime::logging::supabase_sink::{check_postgres_table, send_postgres_batch};
use crate::secrets::{SecretStore, reject_auth_ref_mutation};

use super::core::{OutputFormat, print_json};

pub(super) const SUPABASE_EXAMPLE_URL: &str = "https://example.supabase.co";
pub(super) const SUPABASE_DEFAULT_API_KEY_REF: &str = "SUPABASE_SECRET_KEY";
pub(super) const SUPABASE_DEFAULT_SCHEMA: &str = "acp_stack";

pub(super) const SUPABASE_ENABLED_ENV: &str = "ACP_STACK_SUPABASE_ENABLED";
pub(super) const SUPABASE_URL_ENV: &str = "ACP_STACK_SUPABASE_URL";
pub(super) const SUPABASE_SCHEMA_ENV: &str = "ACP_STACK_SUPABASE_SCHEMA";
pub(super) const SUPABASE_API_KEY_REF_ENV: &str = "ACP_STACK_SUPABASE_API_KEY_REF";
pub(super) const SUPABASE_SECRET_KEY_ENV: &str = "ACP_STACK_SUPABASE_SECRET_KEY";

#[derive(Debug, Subcommand)]
pub enum LoggingCommand {
    /// Configure the external Supabase logging sink.
    Supabase {
        #[command(subcommand)]
        command: SupabaseCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum SupabaseCommand {
    /// Print Supabase logging configuration and secret presence.
    Status,
    /// Provision Supabase logging through the Supabase CLI.
    Setup(SupabaseSetupArgs),
    /// Validate Supabase logging by writing a marked canary row.
    Check,
    /// Print the SQL used by manual Supabase CLI provisioning.
    Sql(SupabaseSqlArgs),
    /// Enable Supabase logging and write the durable config stanza.
    Enable(SupabaseEnableArgs),
    /// Disable Supabase logging while preserving endpoint settings.
    Disable,
    /// Store the Supabase secret API key in the encrypted secret store.
    SetSecret(SupabaseSetSecretArgs),
    /// Store the Supabase Postgres writer URL in the encrypted secret store.
    SetDbUrl(SupabaseSetDbUrlArgs),
}

#[derive(Debug, Args)]
pub struct SupabaseEnableArgs {
    /// Supabase project URL, for example https://project-ref.supabase.co.
    #[arg(long)]
    url: String,
    /// Postgres schema exposed through the Supabase Data API.
    #[arg(long, default_value = SUPABASE_DEFAULT_SCHEMA)]
    schema: String,
    /// Secret-store ref containing the Supabase secret API key.
    #[arg(long = "api-key-ref", default_value = SUPABASE_DEFAULT_API_KEY_REF)]
    api_key_ref: String,
}

#[derive(Debug, Args)]
pub struct SupabaseSetupArgs {
    /// Supabase project URL, for example https://project-ref.supabase.co.
    #[arg(long)]
    url: String,
    /// Supabase project ref. Defaults to the subdomain from --url.
    #[arg(long = "project-ref")]
    project_ref: Option<String>,
    /// Skip the confirmation prompt.
    #[arg(long)]
    yes: bool,
}

#[derive(Debug, Args)]
pub struct SupabaseSqlArgs {
    /// Placeholder writer password to embed in the printed SQL.
    #[arg(
        long = "writer-password",
        default_value = "REPLACE_WITH_GENERATED_PASSWORD"
    )]
    writer_password: String,
    /// Schema for the generated mirror tables.
    #[arg(long, default_value = SUPABASE_POSTGRES_DEFAULT_SCHEMA)]
    schema: String,
    /// Prefix for generated mirror tables.
    #[arg(long = "table-prefix", default_value = SUPABASE_DEFAULT_TABLE_PREFIX)]
    table_prefix: String,
}

#[derive(Debug, Args)]
pub struct SupabaseSetSecretArgs {
    /// Secret-store ref to write. Defaults to the configured ref or SUPABASE_SECRET_KEY.
    #[arg(long = "api-key-ref")]
    api_key_ref: Option<String>,
}

#[derive(Debug, Args)]
pub struct SupabaseSetDbUrlArgs {
    /// Secret-store ref to write. Defaults to the configured ref or SUPABASE_LOG_DB_URL.
    #[arg(long = "db-url-ref")]
    db_url_ref: Option<String>,
}

pub(super) fn run_logging_command(command: LoggingCommand, output: OutputFormat) -> Result<()> {
    match command {
        LoggingCommand::Supabase { command } => run_supabase_command(command, output),
    }
}

fn run_supabase_command(command: SupabaseCommand, output: OutputFormat) -> Result<()> {
    match command {
        SupabaseCommand::Status => run_supabase_status(output),
        SupabaseCommand::Setup(args) => run_supabase_setup(args, output),
        SupabaseCommand::Check => run_supabase_check(output),
        SupabaseCommand::Sql(args) => run_supabase_sql(args, output),
        SupabaseCommand::Enable(args) => run_supabase_enable(args, output),
        SupabaseCommand::Disable => run_supabase_disable(output),
        SupabaseCommand::SetSecret(args) => run_supabase_set_secret(args, output),
        SupabaseCommand::SetDbUrl(args) => run_supabase_set_db_url(args, output),
    }
}

pub(super) fn disabled_supabase_config() -> SupabaseLoggingConfig {
    SupabaseLoggingConfig {
        enabled: false,
        backend: SupabaseLoggingBackend::Postgrest,
        url: SUPABASE_EXAMPLE_URL.to_owned(),
        table_prefix: String::new(),
        db_url_ref: None,
        api_key_ref: SUPABASE_DEFAULT_API_KEY_REF.to_owned(),
        schema: SUPABASE_DEFAULT_SCHEMA.to_owned(),
    }
}

pub(super) fn enabled_supabase_config(
    url: String,
    schema: Option<String>,
    api_key_ref: Option<String>,
) -> SupabaseLoggingConfig {
    SupabaseLoggingConfig {
        enabled: true,
        backend: SupabaseLoggingBackend::Postgrest,
        url,
        table_prefix: String::new(),
        db_url_ref: None,
        api_key_ref: api_key_ref.unwrap_or_else(|| SUPABASE_DEFAULT_API_KEY_REF.to_owned()),
        schema: schema.unwrap_or_else(|| SUPABASE_DEFAULT_SCHEMA.to_owned()),
    }
}

fn enabled_supabase_postgres_config(url: String) -> SupabaseLoggingConfig {
    SupabaseLoggingConfig {
        enabled: true,
        backend: SupabaseLoggingBackend::Postgres,
        url,
        table_prefix: SUPABASE_DEFAULT_TABLE_PREFIX.to_owned(),
        db_url_ref: Some(SUPABASE_DEFAULT_DB_URL_REF.to_owned()),
        api_key_ref: SUPABASE_DEFAULT_API_KEY_REF.to_owned(),
        schema: SUPABASE_POSTGRES_DEFAULT_SCHEMA.to_owned(),
    }
}

pub(super) fn apply_supabase_config(
    config: &mut Config,
    supabase: SupabaseLoggingConfig,
) -> Result<bool> {
    let changed = config.logging.supabase.as_ref() != Some(&supabase);
    config.logging.supabase = Some(supabase);
    validate_config(config)?;
    Ok(changed)
}

pub(super) fn write_config(config: &Config) -> Result<()> {
    let canonical = config.to_canonical_toml()?;
    let validated = config::load_config_from_str(&canonical)?;
    let target = config::default_config_path()?;
    atomic_write_owner_only(&target, validated.to_canonical_toml()?.as_bytes())
}

pub(super) fn ensure_supabase_secret(
    secret_store: &mut SecretStore,
    api_key_ref: &str,
    interactive: bool,
) -> Result<bool> {
    validate_secret_ref(api_key_ref)?;
    if let Ok(value) = std::env::var(SUPABASE_SECRET_KEY_ENV)
        && !value.is_empty()
    {
        secret_store.set(api_key_ref, &value)?;
        return Ok(true);
    }
    if secret_store.contains(api_key_ref) {
        return Ok(false);
    }
    if interactive {
        let value = read_secret_interactive(api_key_ref)?;
        if !value.is_empty() {
            secret_store.set(api_key_ref, &value)?;
            return Ok(true);
        }
    }
    Err(StackError::MissingSupabaseApiKey {
        name: api_key_ref.to_owned(),
    })
}

fn run_supabase_status(output: OutputFormat) -> Result<()> {
    let home = home_dir()?;
    let config = Config::load_from_default_path()?;
    let supabase = config
        .logging
        .supabase
        .clone()
        .unwrap_or_else(disabled_supabase_config);
    let store = SecretStore::open(&home)?;
    let api_key_present = store.contains(&supabase.api_key_ref);
    let db_url_present = supabase
        .db_url_ref
        .as_ref()
        .is_some_and(|db_url_ref| store.contains(db_url_ref));
    let active_credential_present = match supabase.backend {
        SupabaseLoggingBackend::Postgrest => api_key_present,
        SupabaseLoggingBackend::Postgres => db_url_present,
    };

    if output.is_json() {
        print_json(&serde_json::json!({
            "configured": config.logging.supabase.is_some(),
            "enabled": supabase.enabled,
            "backend": supabase_backend_label(supabase.backend),
            "url": supabase.url,
            "schema": supabase.schema,
            "table_prefix": supabase.table_prefix,
            "api_key_ref": supabase.api_key_ref,
            "db_url_ref": supabase.db_url_ref,
            "api_key_present": api_key_present,
            "secret_present": api_key_present,
            "db_url_present": db_url_present,
            "active_credential_present": active_credential_present,
        }))?;
    } else {
        println!(
            "supabase: {}",
            if supabase.enabled {
                "enabled"
            } else {
                "disabled"
            }
        );
        println!("url: {}", supabase.url);
        println!("backend: {}", supabase_backend_label(supabase.backend));
        println!("schema: {}", supabase.schema);
        println!("table_prefix: {}", supabase.table_prefix);
        match supabase.backend {
            SupabaseLoggingBackend::Postgrest => {
                println!("api_key_ref: {}", supabase.api_key_ref);
                println!(
                    "api_key: {}",
                    if api_key_present {
                        "present"
                    } else {
                        "missing"
                    }
                );
            }
            SupabaseLoggingBackend::Postgres => {
                println!(
                    "db_url_ref: {}",
                    supabase.db_url_ref.as_deref().unwrap_or("unset")
                );
                println!(
                    "db_url: {}",
                    if db_url_present { "present" } else { "missing" }
                );
            }
        }
    }
    Ok(())
}

fn run_supabase_setup(args: SupabaseSetupArgs, output: OutputFormat) -> Result<()> {
    crate::runtime::dependencies::deps::resolve_command_path("supabase").ok_or_else(|| {
        StackError::InvalidParam {
            field: "supabase",
            reason: "`supabase` CLI not found or not executable on PATH".to_owned(),
        }
    })?;
    validate_supabase_url(&args.url)?;
    let url = args.url.trim_end_matches('/').to_owned();
    let project_ref = args
        .project_ref
        .clone()
        .unwrap_or_else(|| derive_project_ref(&url));
    validate_project_ref(&project_ref)?;
    if !args.yes && !confirm_setup(&project_ref)? {
        return Err(StackError::InvalidParam {
            field: "confirmation",
            reason: "Supabase setup was not confirmed".to_owned(),
        });
    }

    let writer_password = generate_writer_password();
    let db_url = runtime_writer_db_url(&project_ref, &writer_password);
    let tempdir = tempfile::tempdir().map_err(|source| StackError::ConfigWrite {
        path: PathBuf::from("supabase-tempdir"),
        source,
    })?;
    run_supabase_cli(tempdir.path(), ["init"].as_slice())?;
    run_supabase_cli(
        tempdir.path(),
        ["link", "--project-ref", project_ref.as_str()].as_slice(),
    )?;
    let migrations_dir = tempdir.path().join("supabase").join("migrations");
    std::fs::create_dir_all(&migrations_dir).map_err(|source| StackError::ConfigWrite {
        path: migrations_dir.clone(),
        source,
    })?;
    let migration_path = migrations_dir.join("20260531000000_acp_stack_logging.sql");
    let sql = setup_sql(
        SUPABASE_POSTGRES_DEFAULT_SCHEMA,
        SUPABASE_DEFAULT_TABLE_PREFIX,
        &writer_password,
    );
    atomic_write_owner_only(&migration_path, sql.as_bytes())?;
    let mut db_push_args = vec!["db", "push"];
    if args.yes {
        db_push_args.push("--yes");
    }
    run_supabase_cli(tempdir.path(), &db_push_args)?;

    let home = home_dir()?;
    let mut store = SecretStore::open(&home)?;
    store.set(SUPABASE_DEFAULT_DB_URL_REF, &db_url)?;
    let mut config = Config::load_from_default_path()?;
    let supabase = enabled_supabase_postgres_config(url);
    apply_supabase_config(&mut config, supabase.clone())?;
    write_config(&config)?;

    if output.is_json() {
        print_json(&serde_json::json!({
            "action": "setup",
            "backend": "postgres",
            "url": supabase.url,
            "schema": supabase.schema,
            "table_prefix": supabase.table_prefix,
            "db_url_ref": supabase.db_url_ref,
            "writer_role": SUPABASE_WRITER_ROLE,
        }))?;
    } else {
        println!("supabase logging setup complete");
        println!("backend: postgres");
        println!("url: {}", supabase.url);
        println!("schema: {}", supabase.schema);
        println!("table_prefix: {}", supabase.table_prefix);
        println!("db_url_ref: {}", SUPABASE_DEFAULT_DB_URL_REF);
        println!("writer_role: {}", SUPABASE_WRITER_ROLE);
    }
    Ok(())
}

fn run_supabase_check(output: OutputFormat) -> Result<()> {
    let home = home_dir()?;
    let config = Config::load_from_default_path()?;
    let supabase = config
        .logging
        .supabase
        .clone()
        .ok_or_else(|| StackError::InvalidParam {
            field: "logging.supabase",
            reason: "Supabase logging is not configured".to_owned(),
        })?;
    if !supabase.enabled {
        return Err(StackError::InvalidParam {
            field: "logging.supabase.enabled",
            reason: "Supabase logging is disabled".to_owned(),
        });
    }
    let store = SecretStore::open(&home)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|source| StackError::ServeIo { source })?;
    match supabase.backend {
        SupabaseLoggingBackend::Postgres => {
            let db_url_ref = supabase
                .db_url_ref
                .as_deref()
                .unwrap_or(SUPABASE_DEFAULT_DB_URL_REF);
            let db_url = store.get(db_url_ref)?.to_owned();
            let mut missing_tables = Vec::new();
            for table in MIRRORED_TABLES {
                let present = runtime.block_on(check_postgres_table(&supabase, &db_url, table))?;
                if !present {
                    missing_tables.push(*table);
                }
            }
            if !missing_tables.is_empty() {
                return Err(StackError::InvalidParam {
                    field: "logging.supabase",
                    reason: format!(
                        "missing Supabase mirror tables: {}",
                        missing_tables.join(", ")
                    ),
                });
            }
            let canary = Value::Object(canary_event());
            runtime.block_on(send_postgres_batch(&supabase, &db_url, "events", &[canary]))?;
        }
        SupabaseLoggingBackend::Postgrest => {
            let api_key = store.get(&supabase.api_key_ref)?.to_owned();
            runtime.block_on(check_postgrest_tables_and_canary(&supabase, &api_key))?;
        }
    }

    if output.is_json() {
        print_json(&serde_json::json!({
            "ok": true,
            "backend": supabase_backend_label(supabase.backend),
            "schema": supabase.schema,
            "table_prefix": supabase.table_prefix,
            "canary": "written",
        }))?;
    } else {
        println!("supabase logging check: ok");
        println!("backend: {}", supabase_backend_label(supabase.backend));
        println!("canary: written");
    }
    Ok(())
}

fn run_supabase_sql(args: SupabaseSqlArgs, output: OutputFormat) -> Result<()> {
    // The generated DDL interpolates these identifiers into SQL (including
    // PL/pgSQL `format()` literals), so hold CLI overrides to the same rules
    // config-loaded values must satisfy instead of emitting corrupt SQL.
    crate::config::validate_supabase_identifiers(&args.schema, &args.table_prefix)?;
    let sql = setup_sql(&args.schema, &args.table_prefix, &args.writer_password);
    if output.is_json() {
        print_json(&serde_json::json!({
            "schema": args.schema,
            "table_prefix": args.table_prefix,
            "sql": sql,
        }))?;
    } else {
        print!("{sql}");
    }
    Ok(())
}

fn run_supabase_enable(args: SupabaseEnableArgs, output: OutputFormat) -> Result<()> {
    validate_secret_ref(&args.api_key_ref)?;
    let mut config = Config::load_from_default_path()?;
    let supabase = enabled_supabase_config(args.url, Some(args.schema), Some(args.api_key_ref));
    apply_supabase_config(&mut config, supabase.clone())?;
    write_config(&config)?;

    if output.is_json() {
        print_json(&serde_json::json!({
            "action": "enabled",
            "url": supabase.url,
            "schema": supabase.schema,
            "api_key_ref": supabase.api_key_ref,
        }))?;
    } else {
        println!("supabase logging enabled");
        println!("url: {}", supabase.url);
        println!("schema: {}", supabase.schema);
        println!("api_key_ref: {}", supabase.api_key_ref);
    }
    Ok(())
}

fn run_supabase_disable(output: OutputFormat) -> Result<()> {
    let mut config = Config::load_from_default_path()?;
    let mut supabase = config
        .logging
        .supabase
        .clone()
        .unwrap_or_else(disabled_supabase_config);
    supabase.enabled = false;
    apply_supabase_config(&mut config, supabase)?;
    write_config(&config)?;

    if output.is_json() {
        print_json(&serde_json::json!({ "action": "disabled" }))?;
    } else {
        println!("supabase logging disabled");
    }
    Ok(())
}

fn run_supabase_set_secret(args: SupabaseSetSecretArgs, output: OutputFormat) -> Result<()> {
    let home = home_dir()?;
    let config = Config::load_from_default_path()?;
    let api_key_ref = args
        .api_key_ref
        .or_else(|| {
            config
                .logging
                .supabase
                .as_ref()
                .map(|supabase| supabase.api_key_ref.clone())
        })
        .unwrap_or_else(|| SUPABASE_DEFAULT_API_KEY_REF.to_owned());
    validate_secret_ref(&api_key_ref)?;
    reject_auth_ref_mutation(&api_key_ref, &config)?;
    let value = read_secret_value(&api_key_ref)?;
    if value.is_empty() {
        return Err(StackError::InvalidParam {
            field: "secret",
            reason: "must not be empty".to_owned(),
        });
    }
    let mut store = SecretStore::open(&home)?;
    store.set(&api_key_ref, &value)?;

    if output.is_json() {
        print_json(&serde_json::json!({
            "action": "set",
            "name": api_key_ref,
        }))?;
    } else {
        println!("set supabase secret: {api_key_ref}");
    }
    Ok(())
}

fn run_supabase_set_db_url(args: SupabaseSetDbUrlArgs, output: OutputFormat) -> Result<()> {
    let home = home_dir()?;
    let config = Config::load_from_default_path()?;
    let db_url_ref = args
        .db_url_ref
        .or_else(|| {
            config
                .logging
                .supabase
                .as_ref()
                .and_then(|supabase| supabase.db_url_ref.clone())
        })
        .unwrap_or_else(|| SUPABASE_DEFAULT_DB_URL_REF.to_owned());
    validate_secret_ref(&db_url_ref)?;
    reject_auth_ref_mutation(&db_url_ref, &config)?;
    let value = read_secret_value(&db_url_ref)?;
    if value.is_empty() {
        return Err(StackError::InvalidParam {
            field: "db-url",
            reason: "must not be empty".to_owned(),
        });
    }
    let mut store = SecretStore::open(&home)?;
    store.set(&db_url_ref, &value)?;

    if output.is_json() {
        print_json(&serde_json::json!({
            "action": "set",
            "name": db_url_ref,
        }))?;
    } else {
        println!("set supabase db url: {db_url_ref}");
    }
    Ok(())
}

fn read_secret_value(api_key_ref: &str) -> Result<String> {
    if std::io::stdin().is_terminal() {
        return read_secret_interactive(api_key_ref);
    }
    let mut buffer = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut buffer)
        .map_err(|source| StackError::StdinRead { source })?;
    Ok(buffer.trim_end_matches(['\n', '\r']).to_owned())
}

fn read_secret_interactive(api_key_ref: &str) -> Result<String> {
    rpassword::prompt_password(format!("{api_key_ref}: ")).map_err(|source| {
        StackError::ConfigWrite {
            path: PathBuf::from("stdin"),
            source,
        }
    })
}

fn validate_secret_ref(name: &str) -> Result<()> {
    if is_valid_secret_ref_name(name) {
        return Ok(());
    }
    Err(StackError::InvalidParam {
        field: "api-key-ref",
        reason: "must be a valid secret reference name".to_owned(),
    })
}

fn validate_config(config: &Config) -> Result<()> {
    let canonical = config.to_canonical_toml()?;
    config::load_config_from_str(&canonical)?;
    Ok(())
}

fn supabase_backend_label(backend: SupabaseLoggingBackend) -> &'static str {
    match backend {
        SupabaseLoggingBackend::Postgrest => "postgrest",
        SupabaseLoggingBackend::Postgres => "postgres",
    }
}

fn validate_supabase_url(url: &str) -> Result<()> {
    let normalized = url.trim_end_matches('/');
    if normalized.starts_with("https://") && normalized.ends_with(".supabase.co") {
        return Ok(());
    }
    Err(StackError::InvalidSupabaseUrl {
        url: url.to_owned(),
    })
}

fn derive_project_ref(url: &str) -> String {
    url.trim_start_matches("https://")
        .trim_end_matches('/')
        .split('.')
        .next()
        .unwrap_or_default()
        .to_owned()
}

fn validate_project_ref(project_ref: &str) -> Result<()> {
    if !project_ref.is_empty()
        && project_ref
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    {
        return Ok(());
    }
    Err(StackError::InvalidParam {
        field: "project-ref",
        reason: "must contain only lowercase ASCII letters and digits".to_owned(),
    })
}

fn confirm_setup(project_ref: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        return Err(StackError::InvalidParam {
            field: "--yes",
            reason: "required for non-interactive Supabase setup".to_owned(),
        });
    }
    print!("provision Supabase logging for project {project_ref}? [y/N]: ");
    std::io::stdout()
        .flush()
        .map_err(|source| StackError::ServeIo { source })?;
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|source| StackError::StdinRead { source })?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes" | "YES"))
}

fn generate_writer_password() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn runtime_writer_db_url(project_ref: &str, password: &str) -> String {
    format!(
        "postgresql://{SUPABASE_WRITER_ROLE}:{password}@db.{project_ref}.supabase.co:5432/postgres?sslmode=require"
    )
}

fn run_supabase_cli(cwd: &std::path::Path, args: &[&str]) -> Result<()> {
    let status = Command::new("supabase")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|source| StackError::ConfigWrite {
            path: PathBuf::from("supabase"),
            source,
        })?;
    if status.success() {
        return Ok(());
    }
    Err(StackError::SupabaseCliFailed {
        command: format!("supabase {}", args.join(" ")),
        status: status
            .code()
            .map(|code| code.to_string())
            .unwrap_or_else(|| "signal".to_owned()),
        stderr_tail: "see Supabase CLI output above".to_owned(),
    })
}

async fn check_postgrest_tables_and_canary(
    supabase: &SupabaseLoggingConfig,
    api_key: &str,
) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|err| StackError::SupabaseSinkHttp {
            status: 0,
            body: format!("client build failed: {err}"),
        })?;
    for table in MIRRORED_TABLES {
        let remote_table =
            crate::runtime::logging::supabase_mirror::remote_table_name(supabase, table)?;
        let url = format!(
            "{}/rest/v1/{remote_table}?select=id&limit=0",
            supabase.url.trim_end_matches('/')
        );
        let response = client
            .get(&url)
            .headers(postgrest_headers(
                supabase,
                api_key,
                PostgrestProfileHeader::Accept,
            )?)
            .send()
            .await
            .map_err(|err| StackError::SupabaseSinkHttp {
                status: 0,
                body: format!("transport error: {err}"),
            })?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(StackError::SupabaseSinkHttp {
                status: status.as_u16(),
                body,
            });
        }
    }

    let remote_table =
        crate::runtime::logging::supabase_mirror::remote_table_name(supabase, "events")?;
    let url = format!(
        "{}/rest/v1/{remote_table}",
        supabase.url.trim_end_matches('/')
    );
    let response = client
        .post(&url)
        .headers(postgrest_headers(
            supabase,
            api_key,
            PostgrestProfileHeader::Content,
        )?)
        .header("prefer", "resolution=merge-duplicates,return=minimal")
        .json(&[Value::Object(canary_event())])
        .send()
        .await
        .map_err(|err| StackError::SupabaseSinkHttp {
            status: 0,
            body: format!("transport error: {err}"),
        })?;
    if response.status().is_success() {
        return Ok(());
    }
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    Err(StackError::SupabaseSinkHttp {
        status: status.as_u16(),
        body,
    })
}

#[derive(Debug, Clone, Copy)]
enum PostgrestProfileHeader {
    Accept,
    Content,
}

fn postgrest_headers(
    supabase: &SupabaseLoggingConfig,
    api_key: &str,
    profile_header: PostgrestProfileHeader,
) -> Result<reqwest::header::HeaderMap> {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::HeaderName::from_static("apikey"),
        reqwest::header::HeaderValue::from_str(api_key).map_err(|err| {
            StackError::SupabaseSinkHttp {
                status: 0,
                body: format!("invalid API key header: {err}"),
            }
        })?,
    );
    let profile_name = match profile_header {
        PostgrestProfileHeader::Accept => "accept-profile",
        PostgrestProfileHeader::Content => "content-profile",
    };
    headers.insert(
        reqwest::header::HeaderName::from_static(profile_name),
        reqwest::header::HeaderValue::from_str(&supabase.schema).map_err(|err| {
            StackError::SupabaseSinkHttp {
                status: 0,
                body: format!("invalid {profile_name} header: {err}"),
            }
        })?,
    );
    headers.insert(
        reqwest::header::CONTENT_TYPE,
        reqwest::header::HeaderValue::from_static("application/json"),
    );
    Ok(headers)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_supabase_config() -> SupabaseLoggingConfig {
        SupabaseLoggingConfig {
            enabled: true,
            backend: SupabaseLoggingBackend::Postgrest,
            url: "https://example.supabase.co".to_owned(),
            table_prefix: String::new(),
            db_url_ref: None,
            api_key_ref: SUPABASE_DEFAULT_API_KEY_REF.to_owned(),
            schema: "acp_stack".to_owned(),
        }
    }

    #[test]
    fn postgrest_read_headers_use_accept_profile() {
        let headers = postgrest_headers(
            &test_supabase_config(),
            "sb_secret_test",
            PostgrestProfileHeader::Accept,
        )
        .expect("headers");

        assert_eq!(
            headers
                .get("accept-profile")
                .and_then(|value| value.to_str().ok()),
            Some("acp_stack")
        );
        assert!(headers.get("content-profile").is_none());
    }

    #[test]
    fn postgrest_write_headers_use_content_profile() {
        let headers = postgrest_headers(
            &test_supabase_config(),
            "sb_secret_test",
            PostgrestProfileHeader::Content,
        )
        .expect("headers");

        assert_eq!(
            headers
                .get("content-profile")
                .and_then(|value| value.to_str().ok()),
            Some("acp_stack")
        );
        assert!(headers.get("accept-profile").is_none());
    }
}
