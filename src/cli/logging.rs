use std::io::{BufRead as _, IsTerminal as _};
use std::path::PathBuf;

use clap::{Args, Subcommand};

use crate::config::{self, Config, SupabaseLoggingConfig, is_valid_secret_ref_name};
use crate::error::{Result, StackError};
use crate::fs_util::{atomic_write_owner_only, home_dir};
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
    /// Enable Supabase logging and write the durable config stanza.
    Enable(SupabaseEnableArgs),
    /// Disable Supabase logging while preserving endpoint settings.
    Disable,
    /// Store the Supabase secret API key in the encrypted secret store.
    SetSecret(SupabaseSetSecretArgs),
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
pub struct SupabaseSetSecretArgs {
    /// Secret-store ref to write. Defaults to the configured ref or SUPABASE_SECRET_KEY.
    #[arg(long = "api-key-ref")]
    api_key_ref: Option<String>,
}

pub(super) fn run_logging_command(command: LoggingCommand, output: OutputFormat) -> Result<()> {
    match command {
        LoggingCommand::Supabase { command } => run_supabase_command(command, output),
    }
}

fn run_supabase_command(command: SupabaseCommand, output: OutputFormat) -> Result<()> {
    match command {
        SupabaseCommand::Status => run_supabase_status(output),
        SupabaseCommand::Enable(args) => run_supabase_enable(args, output),
        SupabaseCommand::Disable => run_supabase_disable(output),
        SupabaseCommand::SetSecret(args) => run_supabase_set_secret(args, output),
    }
}

pub(super) fn disabled_supabase_config() -> SupabaseLoggingConfig {
    SupabaseLoggingConfig {
        enabled: false,
        url: SUPABASE_EXAMPLE_URL.to_owned(),
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
        url,
        api_key_ref: api_key_ref.unwrap_or_else(|| SUPABASE_DEFAULT_API_KEY_REF.to_owned()),
        schema: schema.unwrap_or_else(|| SUPABASE_DEFAULT_SCHEMA.to_owned()),
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
    let secret_present = store.contains(&supabase.api_key_ref);

    if output.is_json() {
        print_json(&serde_json::json!({
            "configured": config.logging.supabase.is_some(),
            "enabled": supabase.enabled,
            "url": supabase.url,
            "schema": supabase.schema,
            "api_key_ref": supabase.api_key_ref,
            "secret_present": secret_present,
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
        println!("schema: {}", supabase.schema);
        println!("api_key_ref: {}", supabase.api_key_ref);
        println!(
            "secret: {}",
            if secret_present { "present" } else { "missing" }
        );
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
