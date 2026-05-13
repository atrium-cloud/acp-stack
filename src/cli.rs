use crate::config::Config;
use crate::error::{Result, StackError};
use base64::Engine;
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "acps", version, about = env!("CARGO_PKG_DESCRIPTION"))]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    Validate(ConfigValidateArgs),
    Export(ConfigExportArgs),
}

#[derive(Debug, Args)]
struct ConfigValidateArgs {
    path: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct ConfigExportArgs {
    #[arg(long)]
    output: Option<PathBuf>,
    #[arg(long)]
    base64: bool,
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    run_cli(cli)
}

fn run_cli(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Config { command } => run_config_command(command),
    }
}

fn run_config_command(command: ConfigCommand) -> Result<()> {
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
    }
}

fn load_config(path: Option<PathBuf>) -> Result<Config> {
    match path {
        Some(path) => Config::load_from_path(path),
        None => Config::load_from_default_path(),
    }
}
