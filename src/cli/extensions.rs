//! `acps extensions` — read-only status for declared extension instances.
//!
//! There is deliberately no mutating CLI: extension declarations are edited in
//! the config TOML, and managed-state namespaces are written only by their
//! external orchestrator through the admin apply endpoint. A second local
//! writer would blur the store-level provenance the seam depends on.

use clap::Subcommand;
use serde_json::{Value, json};

use crate::config::{Config, ExtensionType};
use crate::error::Result;
use crate::fs_util::home_dir;
use crate::secrets::{SecretStore, secret_store_path};

use super::core::{OutputFormat, print_json};

#[derive(Debug, Subcommand)]
pub enum ExtensionsCommand {
    /// List declared extensions; for managed-state, the applied revision.
    Status,
}

pub(super) fn run_extensions_command(
    command: ExtensionsCommand,
    output: OutputFormat,
) -> Result<()> {
    match command {
        ExtensionsCommand::Status => run_status(output),
    }
}

fn run_status(output: OutputFormat) -> Result<()> {
    let config = Config::load_from_default_path()?;
    // The secret store only matters for managed-state watermarks; a
    // network-provider-only config must not require an initialized store, and
    // a declared-but-never-applied namespace on a pre-init host is simply "no
    // watermark yet". Any other open failure still fails fast.
    let has_managed_state = config
        .extensions
        .values()
        .any(|extension| extension.extension_type == ExtensionType::ManagedState);
    let home = home_dir()?;
    let store = if has_managed_state && secret_store_path(&home).exists() {
        Some(SecretStore::open_read_only(&home)?)
    } else {
        None
    };

    if output.is_json() {
        let mut extensions = serde_json::Map::new();
        for (name, extension) in &config.extensions {
            extensions.insert(
                name.clone(),
                extension_json(name, extension, store.as_ref()),
            );
        }
        print_json(&Value::Object(extensions))?;
        return Ok(());
    }

    if config.extensions.is_empty() {
        println!("extensions: none declared");
        return Ok(());
    }
    println!("extensions: {} declared", config.extensions.len());
    for (name, extension) in &config.extensions {
        println!("{name}: {}", extension.extension_type.as_str());
        match extension.extension_type {
            ExtensionType::NetworkProvider => {
                if extension.provider.is_empty() {
                    println!("  provider: none (deny-all)");
                } else {
                    println!("  provider: {}", extension.provider.join(" "));
                }
                println!(
                    "  provider_timeout: {}",
                    extension
                        .provider_timeout
                        .as_deref()
                        .unwrap_or(crate::config::DEFAULT_NETWORK_PROVIDER_TIMEOUT)
                );
                println!("  provider_stderr: {}", extension.provider_stderr.as_str());
            }
            ExtensionType::ManagedState => {
                if let Some(capability) = extension.capability.as_deref() {
                    println!("  capability: {capability}");
                }
                match store
                    .as_ref()
                    .and_then(|store| store.managed_state_record(name))
                {
                    Some(record) => {
                        println!("  applied_revision: {}", record.revision);
                        println!(
                            "  provider: {}",
                            record.provider_id.as_deref().unwrap_or("none")
                        );
                    }
                    None => println!("  applied_revision: none"),
                }
            }
        }
    }
    Ok(())
}

fn extension_json(
    name: &str,
    extension: &crate::config::ExtensionConfig,
    store: Option<&SecretStore>,
) -> Value {
    match extension.extension_type {
        ExtensionType::NetworkProvider => json!({
            "type": extension.extension_type.as_str(),
            "provider_configured": !extension.provider.is_empty(),
            "provider_timeout": extension
                .provider_timeout
                .as_deref()
                .unwrap_or(crate::config::DEFAULT_NETWORK_PROVIDER_TIMEOUT),
            "provider_stderr": extension.provider_stderr.as_str(),
        }),
        ExtensionType::ManagedState => {
            let record = store.and_then(|store| store.managed_state_record(name));
            json!({
                "type": extension.extension_type.as_str(),
                "capability": extension.capability,
                "applied_revision": record.map(|record| record.revision),
                "provider_id": record.and_then(|record| record.provider_id.clone()),
            })
        }
    }
}
