use crate::config;
use crate::error::{Result, StackError};
use crate::fs_util::home_dir;
use crate::secrets::{age_key_path, secret_store_path};
use crate::state::default_state_path;
use clap::Args;

#[derive(Debug, Args)]
pub struct ResetArgs {
    /// Confirm deletion of config, state, age key, and secret store.
    #[arg(long)]
    yes: bool,
}

pub(super) fn run_reset(args: ResetArgs) -> Result<()> {
    let home = home_dir()?;
    let config_path = config::default_config_path()?;
    let state_path = default_state_path(&home);
    let age_key = age_key_path(&home);
    let store_path = secret_store_path(&home);

    let targets = [&config_path, &state_path, &age_key, &store_path];

    if !args.yes {
        println!("acps reset would delete:");
        for target in targets {
            println!("  {}", target.display());
        }
        println!("re-run with --yes to confirm");
        return Err(StackError::ResetNotConfirmed);
    }

    for target in targets {
        if !target.exists() {
            continue;
        }
        std::fs::remove_file(target).map_err(|source| StackError::FileRemove {
            path: target.to_path_buf(),
            source,
        })?;
    }

    println!("reset acp-stack: removed config, state, age key, and secret store");
    Ok(())
}
