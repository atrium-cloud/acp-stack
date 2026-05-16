use crate::config::{self, Config};
use crate::error::Result;
use crate::fs_util::{
    create_dir_owner_only, home_dir, parent_dir, pre_create_owner_only, set_owner_only_dir,
    set_owner_only_file,
};
use crate::state::{StateStore, default_state_path};

pub(super) fn run_status() -> Result<()> {
    let home = home_dir()?;
    let config_path = config::default_config_path()?;
    let config_dir = parent_dir(&config_path)?;
    if config_dir.exists() {
        set_owner_only_dir(config_dir)?;
    }
    if config_path.exists() {
        set_owner_only_file(&config_path)?;
    }
    Config::load_from_path(&config_path)?;

    let state_path = default_state_path(&home);
    let state_dir = parent_dir(&state_path)?;
    create_dir_owner_only(state_dir)?;
    pre_create_owner_only(&state_path)?;
    let store = StateStore::open(&state_path)?;
    store.migrate()?;
    set_owner_only_file(&state_path)?;
    store.append_event_with_source(
        "info",
        "status.checked",
        crate::state::EVENT_SOURCE_CLI,
        "status checked",
        "{}",
    )?;

    let schema_version = store.schema_version()?;
    let latest_event = store
        .latest_event_timestamp()?
        .unwrap_or_else(|| "none".to_owned());

    println!("config: ok ({})", config_path.display());
    println!("state: ok ({})", state_path.display());
    println!("schema_version: {schema_version}");
    println!("latest_event: {latest_event}");

    Ok(())
}
