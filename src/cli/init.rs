use crate::auth::generate_api_key;
use crate::config::{self, Config};
use crate::error::{Result, StackError};
use crate::fs_util::{
    create_dir_owner_only, home_dir, parent_dir, pre_create_owner_only, set_owner_only_file,
    write_new_file_owner_only,
};
use crate::secrets::{SecretStore, age_key_path, secret_store_path};
use crate::state::{StateStore, default_state_path};

pub(super) fn run_init() -> Result<()> {
    let home = home_dir()?;
    let config_path = config::default_config_path()?;
    let state_path = default_state_path(&home);
    let config_dir = parent_dir(&config_path)?;
    let state_dir = parent_dir(&state_path)?;

    create_dir_owner_only(config_dir)?;
    create_dir_owner_only(state_dir)?;

    let config_status = if config_path.exists() {
        // Repair perms before validation so a failure to parse the file does not
        // leave a permissive config on disk; matches the behavior of `acps status`.
        set_owner_only_file(&config_path)?;
        Config::load_from_path(&config_path)?;
        "validated existing config"
    } else {
        write_new_file_owner_only(&config_path, starter_config().as_bytes())?;
        Config::load_from_path(&config_path)?;
        "created starter config"
    };

    pre_create_owner_only(&state_path)?;
    let store = StateStore::open(&state_path)?;
    store.migrate()?;
    set_owner_only_file(&state_path)?;

    let config = Config::load_from_path(&config_path)?;
    let session_ref = config.auth.session_key_ref.clone();
    let admin_ref = config.auth.admin_key_ref.clone();
    let store_existed = secret_store_path(&home).exists();
    let mut secret_store = SecretStore::open_or_create(&home)?;
    let session_present = secret_store.contains(&session_ref);
    let admin_present = secret_store.contains(&admin_ref);
    let auth_status = if store_existed {
        // Pre-existing store: both refs must be present. Half-initialized state
        // (e.g. one ref deleted, or unrelated secrets but no auth refs) is an
        // anomaly. Refuse to proceed — admin key is not regenerable in place;
        // the documented recovery path is `acps reset --yes`.
        if !admin_present {
            return Err(StackError::MissingAdminKey { name: admin_ref });
        }
        if !session_present {
            return Err(StackError::MissingSessionKey { name: session_ref });
        }
        "preserved existing API keys"
    } else {
        // Fresh store: generate both keys. Print the values BEFORE the durable
        // event write, so a downstream failure in `append_event` cannot leave
        // the persisted-but-never-revealed admin key unrecoverable.
        let session_value = generate_api_key();
        let admin_value = generate_api_key();
        println!("---");
        println!("session key ({session_ref}): {session_value}");
        println!("admin key ({admin_ref}): {admin_value}");
        println!(
            "save the admin key now; it is never regenerable. use `acps reset --yes` to rotate it."
        );
        println!("---");
        // Write both refs in one atomic persist so a mid-init failure cannot
        // leave the store with one key set and the other missing, which the
        // fail-fast logic would then treat as a corrupted state requiring
        // reset.
        secret_store.set_many([
            (session_ref.as_str(), session_value.as_str()),
            (admin_ref.as_str(), admin_value.as_str()),
        ])?;
        store.append_event_with_source(
            "info",
            "auth.keys_generated",
            crate::state::EVENT_SOURCE_CLI,
            "generated session and admin API keys",
            &serde_json::json!({
                "session_key_ref": session_ref,
                "admin_key_ref": admin_ref,
            })
            .to_string(),
        )?;
        "generated session and admin API keys"
    };

    // Record init.completed AFTER secret-store setup so a half-finished init
    // (e.g. failed key generation) does not leave a misleading
    // "initialized" event in the durable log.
    store.append_event_with_source(
        "info",
        "init.completed",
        crate::state::EVENT_SOURCE_CLI,
        "initialized",
        "{}",
    )?;

    println!("initialized acp-stack");
    println!("{config_status}: {}", config_path.display());
    println!("state: {}", state_path.display());
    println!("secrets: {}", secret_store.store_path().display());
    println!("age key: {}", age_key_path(&home).display());
    println!("auth: {auth_status}");

    Ok(())
}

fn starter_config() -> &'static str {
    r#"[api]
bind = "127.0.0.1:7700"
public_url = "http://127.0.0.1:7700"
max_request_bytes = 104857600

[auth]
session_key_ref = "ACP_STACK_SESSION_KEY"
admin_key_ref = "ACP_STACK_ADMIN_KEY"

[security.http]
max_request_bytes = 104857600
rate_limit_per_minute = 120
burst = 30
auth_failures_per_minute = 5
auth_block_duration = "15m"
allowed_origins = []
trust_proxy_headers = false

[workspace]
root = "/workspace"
uploads = "/workspace/uploads"
default_shell = "/bin/bash"
runtime_user = "acp"
max_file_bytes = 8388608

[workspace.source]
type = "none"

[logging]
level = "info"
local_retention_days = 30

[logging.supabase]
enabled = false
url = "https://example.supabase.co"
service_role_key_ref = "SUPABASE_SERVICE_ROLE_KEY"
schema = "acp_stack"

[agent]
id = "placeholder"
name = "Placeholder Agent"
command = "acp-agent"
args = []
cwd = "/workspace"
env = []
restart = "never"

[agent.install]
type = "shell"
shell = "true"
creates = "acp-agent"
"#
}
