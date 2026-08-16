#![cfg(all(feature = "dev-tools", feature = "test-fixtures"))]

mod common;

#[path = "cli_secrets_config_tests/support.rs"]
mod support;

#[path = "cli_secrets_config_tests/auto_update.rs"]
mod auto_update;
#[path = "cli_secrets_config_tests/config_import.rs"]
mod config_import;
#[path = "cli_secrets_config_tests/credentials_extensions.rs"]
mod credentials_extensions;
#[path = "cli_secrets_config_tests/init_keys.rs"]
mod init_keys;
#[path = "cli_secrets_config_tests/init_resume.rs"]
mod init_resume;
#[path = "cli_secrets_config_tests/secrets.rs"]
mod secrets;
#[path = "cli_secrets_config_tests/supabase.rs"]
mod supabase;
