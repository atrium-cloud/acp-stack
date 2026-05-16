use crate::config::Config;
use crate::error::Result;
use clap::Subcommand;

#[derive(Debug, Subcommand)]
pub enum DepsCommand {
    /// Print declared dependency status.
    Check,
}

pub(super) fn run_deps_command(command: DepsCommand) -> Result<()> {
    match command {
        DepsCommand::Check => {
            let config = Config::load_from_default_path()?;
            let report = crate::deps::check_dependencies(&config);
            if report.dependencies.is_empty() {
                println!("no dependencies declared in [dependencies]");
                return Ok(());
            }
            for entry in &report.dependencies {
                let status = if entry.available {
                    if let Some(path) = &entry.path {
                        format!("OK  {path}")
                    } else {
                        "OK".to_owned()
                    }
                } else {
                    let reason = entry.reason.as_deref().unwrap_or("unavailable");
                    format!("MISS {reason}")
                };
                let required = if entry.required { "*" } else { " " };
                println!(
                    "{required}{kind:<8} {name:<24} {status}",
                    kind = format!("{:?}", entry.kind).to_lowercase(),
                    name = entry.name,
                );
            }
            Ok(())
        }
    }
}
