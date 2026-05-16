use crate::config::{self, Config};
use crate::error::{Result, StackError};
use crate::fs_util::{
    atomic_write_owner_only, create_dir_owner_only, parent_dir, write_new_file_owner_only,
};
use base64::Engine;
use clap::{Args, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    Validate(ConfigValidateArgs),
    Export(ConfigExportArgs),
    Import(ConfigImportArgs),
}

#[derive(Debug, Args)]
pub struct ConfigValidateArgs {
    path: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct ConfigExportArgs {
    #[arg(long)]
    output: Option<PathBuf>,
    #[arg(long)]
    base64: bool,
}

#[derive(Debug, Args)]
pub struct ConfigImportArgs {
    /// Path to a TOML config file. Mutually exclusive with --base64.
    path: Option<PathBuf>,
    /// Base64-encoded canonical TOML. Mutually exclusive with `path`.
    #[arg(long, conflicts_with = "path")]
    base64: Option<String>,
    /// Replace the existing config; without --force, import refuses to clobber.
    #[arg(long)]
    force: bool,
}

pub(super) fn run_config_command(command: ConfigCommand) -> Result<()> {
    match command {
        ConfigCommand::Validate(args) => {
            load_config(args.path)?;
            println!("config is valid");
            Ok(())
        }
        ConfigCommand::Export(args) => {
            let config = Config::load_from_default_path()?;
            let canonical = config.to_canonical_toml()?;
            let output = if args.base64 {
                base64::engine::general_purpose::STANDARD.encode(canonical)
            } else {
                canonical
            };

            if let Some(path) = args.output {
                std::fs::write(&path, output)
                    .map_err(|source| StackError::ConfigWrite { path, source })?;
            } else {
                println!("{output}");
            }

            Ok(())
        }
        ConfigCommand::Import(args) => run_config_import(args),
    }
}

fn run_config_import(args: ConfigImportArgs) -> Result<()> {
    let raw_toml = match (args.path.as_deref(), args.base64.as_deref()) {
        (None, None) => {
            return Err(StackError::MissingField {
                field: "config import requires either <path> or --base64",
            });
        }
        (Some(_), Some(_)) => {
            return Err(StackError::MissingField {
                field: "config import accepts only one of <path> or --base64",
            });
        }
        (Some(path), None) => {
            std::fs::read_to_string(path).map_err(|source| StackError::ConfigRead {
                path: path.to_path_buf(),
                source,
            })?
        }
        (None, Some(encoded)) => {
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|source| StackError::ImportBase64Decode { source })?;
            String::from_utf8(decoded).map_err(|source| StackError::ImportUtf8 { source })?
        }
    };

    let config = config::load_config_from_str(&raw_toml)?;
    let canonical = config.to_canonical_toml()?;
    let target = config::default_config_path()?;

    let target_dir = parent_dir(&target)?;
    create_dir_owner_only(target_dir)?;

    if target.exists() {
        if !args.force {
            return Err(StackError::ConfigExists {
                path: target.clone(),
            });
        }
        // Refuse to change the auth-ref names through import. Allowing it
        // would let an operator point `admin_key_ref` at a secret of their
        // own choosing, effectively replacing the original admin key without
        // going through `acps reset --yes` — bypassing the documented
        // reset-only rotation path for the admin key.
        let current = Config::load_from_path(&target)?;
        config::compare_auth_refs(&current.auth, &config.auth)?;
        // Atomic replace via temp file + rename, with owner-only mode on both
        // the temp and the final file. Avoids leaving a truncated config on
        // crash mid-write, which would otherwise brick the next `acps` run.
        atomic_write_owner_only(&target, canonical.as_bytes())?;
        println!("imported config (replaced): {}", target.display());
    } else {
        write_new_file_owner_only(&target, canonical.as_bytes())?;
        println!("imported config: {}", target.display());
    }

    Ok(())
}

fn load_config(path: Option<PathBuf>) -> Result<Config> {
    match path {
        Some(path) => Config::load_from_path(path),
        None => Config::load_from_default_path(),
    }
}
