use crate::config::{self, Config};
use crate::error::{Result, StackError};
use crate::fs_util::{
    atomic_write_owner_only, create_dir_owner_only, parent_dir, write_new_file_owner_only,
};
use base64::Engine;
use clap::{Args, Subcommand};
use std::path::PathBuf;

use super::core::{OutputFormat, print_json};

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
    /// Validate and report without writing or auditing.
    #[arg(long)]
    dry_run: bool,
}

pub(super) fn run_config_command(
    command: ConfigCommand,
    output_format: OutputFormat,
) -> Result<()> {
    match command {
        ConfigCommand::Validate(args) => {
            let path = args.path.clone();
            load_config(args.path)?;
            if output_format.is_json() {
                print_json(&serde_json::json!({
                    "valid": true,
                    "path": path.map(|value| value.display().to_string()),
                }))?;
            } else {
                println!("config is valid");
            }
            Ok(())
        }
        ConfigCommand::Export(args) => {
            let config = Config::load_from_default_path()?;
            let canonical = config.to_canonical_toml()?;
            let rendered = if args.base64 {
                base64::engine::general_purpose::STANDARD.encode(canonical)
            } else {
                canonical
            };

            if let Some(path) = args.output {
                std::fs::write(&path, &rendered).map_err(|source| StackError::ConfigWrite {
                    path: path.clone(),
                    source,
                })?;
                if output_format.is_json() {
                    print_json(&serde_json::json!({
                        "format": if args.base64 { "base64" } else { "toml" },
                        "output_path": path.display().to_string(),
                        "bytes": rendered.len(),
                    }))?;
                }
            } else {
                if output_format.is_json() {
                    let bytes = rendered.len();
                    print_json(&serde_json::json!({
                        "format": if args.base64 { "base64" } else { "toml" },
                        "value": rendered,
                        "bytes": bytes,
                    }))?;
                } else {
                    println!("{rendered}");
                }
            }

            Ok(())
        }
        ConfigCommand::Import(args) => run_config_import(args, output_format),
    }
}

fn run_config_import(args: ConfigImportArgs, output: OutputFormat) -> Result<()> {
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
            let bytes = std::fs::read(path).map_err(|source| StackError::ConfigRead {
                path: path.to_path_buf(),
                source,
            })?;
            if bytes.len() > config::IMPORT_SIZE_LIMIT {
                return Err(StackError::ImportTooLarge {
                    limit: config::IMPORT_SIZE_LIMIT,
                    actual: bytes.len(),
                });
            }
            String::from_utf8(bytes).map_err(|source| StackError::ImportUtf8 { source })?
        }
        (None, Some(encoded)) => {
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|source| StackError::ImportBase64Decode { source })?;
            if decoded.len() > config::IMPORT_SIZE_LIMIT {
                return Err(StackError::ImportTooLarge {
                    limit: config::IMPORT_SIZE_LIMIT,
                    actual: decoded.len(),
                });
            }
            String::from_utf8(decoded).map_err(|source| StackError::ImportUtf8 { source })?
        }
    };

    let config = config::load_config_from_str(&raw_toml)?;
    let canonical = config.to_canonical_toml()?;
    let target = config::default_config_path()?;

    if args.dry_run {
        let auth_refs_ok = if target.exists() {
            let current = Config::load_from_path(&target)?;
            config::compare_auth_refs(&current.auth, &config.auth).is_ok()
        } else {
            true
        };
        if output.is_json() {
            print_json(&serde_json::json!({
                "dry_run": true,
                "config_version": config.config_version,
                "canonical_toml_bytes": canonical.len(),
                "input_bytes": raw_toml.len(),
                "auth_refs_unchanged": auth_refs_ok,
                "target_path": target.display().to_string(),
                "target_exists": target.exists(),
            }))?;
        } else {
            println!("import dry-run complete");
            println!("  config_version: {}", config.config_version);
            println!("  canonical TOML size: {} bytes", canonical.len());
            println!("  input size: {} bytes", raw_toml.len());
            println!("  auth refs unchanged: {auth_refs_ok}");
            println!("  would write to: {}", target.display());
            println!("  target exists: {}", target.exists());
        }
        return Ok(());
    }

    let target_dir = parent_dir(&target)?;
    create_dir_owner_only(target_dir)?;

    if target.exists() {
        if !args.force {
            return Err(StackError::ConfigExists {
                path: target.clone(),
            });
        }
        let current = Config::load_from_path(&target)?;
        config::compare_auth_refs(&current.auth, &config.auth)?;
        atomic_write_owner_only(&target, canonical.as_bytes())?;
        if output.is_json() {
            print_json(&serde_json::json!({
                "imported": true,
                "replaced": true,
                "path": target.display().to_string(),
                "bytes": canonical.len(),
            }))?;
        } else {
            println!("imported config (replaced): {}", target.display());
        }
    } else {
        write_new_file_owner_only(&target, canonical.as_bytes())?;
        if output.is_json() {
            print_json(&serde_json::json!({
                "imported": true,
                "replaced": false,
                "path": target.display().to_string(),
                "bytes": canonical.len(),
            }))?;
        } else {
            println!("imported config: {}", target.display());
        }
    }

    Ok(())
}

fn load_config(path: Option<PathBuf>) -> Result<Config> {
    match path {
        Some(path) => Config::load_from_path(path),
        None => Config::load_from_default_path(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_base64_rejects_oversized_decoded_input() {
        let encoded = base64::engine::general_purpose::STANDARD
            .encode("x".repeat(config::IMPORT_SIZE_LIMIT + 1));

        let error = run_config_import(
            ConfigImportArgs {
                path: None,
                base64: Some(encoded),
                force: false,
                dry_run: true,
            },
            OutputFormat::Text,
        )
        .expect_err("decoded payload over the import limit must fail");

        assert!(matches!(
            error,
            StackError::ImportTooLarge {
                limit: config::IMPORT_SIZE_LIMIT,
                actual
            } if actual == config::IMPORT_SIZE_LIMIT + 1
        ));
    }
}
