use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, IsTerminal};

use crate::cli::core::{OutputFormat, print_json};
use crate::config::{self, Config, is_valid_secret_ref_name};
use crate::error::{Result, StackError};
use crate::fs_util::{acquire_agent_config_mutation_file_lock, home_dir};
use crate::runtime::agent::provider_keys::{
    companion_env_refs_for_provider_id, env_var_for_provider_id, optional_env_refs_for_provider_id,
    target_uses_provider,
};
use crate::secrets::{ProviderCredential, ProviderCredentialSet, SecretStore};

use super::provider_migration::{
    migrate_legacy_provider_credentials, prune_migrated_flat_secrets_with_candidates,
    replace_catalog_then_config,
};
use super::provider_shared::{
    canonicalize_for_write, ensure_credential_value_is_new, ensure_provider_settings,
    missing_provider_credential, resolve_alias, sync_primary_agent, validate_alias,
    validate_catalog_provider,
};
use super::{
    AgentProviderCredentialAddArgs, AgentProviderCredentialDeleteArgs,
    AgentProviderCredentialListArgs, AgentProviderCredentialUpdateArgs,
};

pub(super) fn run_credential_add(
    args: AgentProviderCredentialAddArgs,
    output: OutputFormat,
) -> Result<()> {
    validate_catalog_provider(&args.provider)?;
    let home = home_dir()?;
    let config_path = config::default_config_path()?;
    let _mutation = acquire_agent_config_mutation_file_lock(&config_path)?;
    let mut config = Config::load_from_path(&config_path)?;
    let mut secrets = SecretStore::open(&home)?;
    let previous_catalog = secrets.provider_credentials().clone();
    let migration = migrate_legacy_provider_credentials(&mut config, &mut secrets)?;
    let mut catalog = secrets.provider_credentials().clone();
    let mut added_alias = None;
    match catalog.remove(&args.provider) {
        None => {
            if args.alias.is_some() || args.existing_alias.is_some() {
                return Err(StackError::InvalidParam {
                    field: "alias",
                    reason: "the first credential is aliasless; omit alias flags".to_owned(),
                });
            }
            let credential = collect_credential(&args.provider, &args.from_secret, &secrets)?;
            catalog.insert(
                args.provider.clone(),
                ProviderCredentialSet::aliasless(credential),
            );
        }
        Some(existing) if !existing.is_promoted() => {
            let existing_alias = resolve_alias(
                args.existing_alias.as_deref(),
                "alias for the existing credential",
            )?;
            let new_alias = resolve_alias(args.alias.as_deref(), "alias for the new credential")?;
            if existing_alias == new_alias {
                return Err(StackError::InvalidParam {
                    field: "alias",
                    reason: "existing and new aliases must differ".to_owned(),
                });
            }
            let credential = collect_credential(&args.provider, &args.from_secret, &secrets)?;
            ensure_credential_value_is_new(&args.provider, &existing, &credential)?;
            let mut aliases = BTreeMap::new();
            aliases.insert(
                existing_alias.clone(),
                existing
                    .sole
                    .ok_or_else(|| StackError::SecretStorePlaintextInvalid {
                        reason: format!(
                            "provider credential `{}` is missing its aliasless value",
                            args.provider
                        ),
                    })?,
            );
            aliases.insert(new_alias.clone(), credential);
            catalog.insert(
                args.provider.clone(),
                ProviderCredentialSet::promoted(aliases),
            );
            for target in &mut config.array.targets {
                if target_uses_provider(&target.agent, &args.provider) {
                    ensure_provider_settings(&mut target.agent)
                        .selected_aliases
                        .insert(args.provider.clone(), existing_alias.clone());
                }
            }
            added_alias = Some(new_alias);
        }
        Some(mut existing) => {
            if args.existing_alias.is_some() {
                return Err(StackError::InvalidParam {
                    field: "existing-alias",
                    reason: "provider credentials are already promoted".to_owned(),
                });
            }
            let alias = resolve_alias(args.alias.as_deref(), "alias for the new credential")?;
            if existing.aliases.contains_key(&alias) {
                return Err(StackError::InvalidParam {
                    field: "alias",
                    reason: format!("provider `{}` already has alias `{alias}`", args.provider),
                });
            }
            let credential = collect_credential(&args.provider, &args.from_secret, &secrets)?;
            ensure_credential_value_is_new(&args.provider, &existing, &credential)?;
            existing.aliases.insert(alias.clone(), credential);
            catalog.insert(args.provider.clone(), existing);
            added_alias = Some(alias);
        }
    }
    sync_primary_agent(&mut config);
    let (validated, canonical) = canonicalize_for_write(&config)?;
    replace_catalog_then_config(
        &mut secrets,
        catalog,
        previous_catalog,
        &validated,
        &home,
        &config_path,
        &canonical,
    )?;
    prune_migrated_flat_secrets_with_candidates(
        &validated,
        &mut secrets,
        &migration.cleanup_candidates,
    )?;
    print_credential_mutation(output, "added", &args.provider, added_alias.as_deref())
}

pub(super) fn run_credential_update(
    args: AgentProviderCredentialUpdateArgs,
    output: OutputFormat,
) -> Result<()> {
    validate_catalog_provider(&args.provider)?;
    let home = home_dir()?;
    let config_path = config::default_config_path()?;
    let _mutation = acquire_agent_config_mutation_file_lock(&config_path)?;
    let mut config = Config::load_from_path(&config_path)?;
    let mut secrets = SecretStore::open(&home)?;
    let previous_catalog = secrets.provider_credentials().clone();
    let migration = migrate_legacy_provider_credentials(&mut config, &mut secrets)?;
    let replacement = collect_credential(&args.provider, &args.from_secret, &secrets)?;
    let mut catalog = secrets.provider_credentials().clone();
    let credentials = catalog
        .get_mut(&args.provider)
        .ok_or_else(|| missing_provider_credential(&args.provider))?;
    match (&mut credentials.sole, args.alias.as_deref()) {
        (Some(existing), None) => {
            existing.rotate(replacement.values, replacement.source_refs);
        }
        (Some(_), Some(_)) => {
            return Err(StackError::InvalidParam {
                field: "alias",
                reason: format!("provider `{}` has one aliasless credential", args.provider),
            });
        }
        (None, Some(alias)) => {
            validate_alias(alias)?;
            let existing =
                credentials
                    .aliases
                    .get_mut(alias)
                    .ok_or_else(|| StackError::InvalidParam {
                        field: "alias",
                        reason: format!("provider `{}` has no alias `{alias}`", args.provider),
                    })?;
            existing.rotate(replacement.values, replacement.source_refs);
        }
        (None, None) => {
            return Err(StackError::InvalidParam {
                field: "alias",
                reason: format!("provider `{}` requires an alias", args.provider),
            });
        }
    }
    sync_primary_agent(&mut config);
    let (validated, canonical) = canonicalize_for_write(&config)?;
    replace_catalog_then_config(
        &mut secrets,
        catalog,
        previous_catalog,
        &validated,
        &home,
        &config_path,
        &canonical,
    )?;
    prune_migrated_flat_secrets_with_candidates(
        &validated,
        &mut secrets,
        &migration.cleanup_candidates,
    )?;
    print_credential_mutation(output, "updated", &args.provider, args.alias.as_deref())
}

pub(super) fn run_credential_delete(
    args: AgentProviderCredentialDeleteArgs,
    output: OutputFormat,
) -> Result<()> {
    validate_catalog_provider(&args.provider)?;
    let home = home_dir()?;
    let config_path = config::default_config_path()?;
    let _mutation = acquire_agent_config_mutation_file_lock(&config_path)?;
    let mut config = Config::load_from_path(&config_path)?;
    let mut secrets = SecretStore::open(&home)?;
    let previous_catalog = secrets.provider_credentials().clone();
    let migration = migrate_legacy_provider_credentials(&mut config, &mut secrets)?;
    let mut catalog = secrets.provider_credentials().clone();
    let credentials = catalog
        .get_mut(&args.provider)
        .ok_or_else(|| missing_provider_credential(&args.provider))?;
    let mut remove_provider = false;
    if let Some(alias) = args.alias.as_deref() {
        validate_alias(alias)?;
        if !credentials.is_promoted() {
            return Err(StackError::InvalidParam {
                field: "alias",
                reason: format!("provider `{}` has one aliasless credential", args.provider),
            });
        }
        for target in &config.array.targets {
            if target
                .agent
                .providers
                .as_ref()
                .and_then(|providers| providers.selected_aliases.get(&args.provider))
                .is_some_and(|selected| selected == alias)
            {
                return Err(StackError::InvalidParam {
                    field: "alias",
                    reason: format!(
                        "provider `{}` alias `{alias}` is selected by target `{}`",
                        args.provider, target.id
                    ),
                });
            }
        }
        if credentials.aliases.remove(alias).is_none() {
            return Err(StackError::InvalidParam {
                field: "alias",
                reason: format!("provider `{}` has no alias `{alias}`", args.provider),
            });
        }
        remove_provider = credentials.aliases.is_empty();
    } else {
        if credentials.is_promoted() {
            return Err(StackError::InvalidParam {
                field: "alias",
                reason: format!("provider `{}` requires an alias", args.provider),
            });
        }
        if let Some(target) = config
            .array
            .targets
            .iter()
            .find(|target| target_uses_provider(&target.agent, &args.provider))
        {
            return Err(StackError::InvalidParam {
                field: "provider",
                reason: format!(
                    "provider `{}` credential is required by target `{}`",
                    args.provider, target.id
                ),
            });
        }
        catalog.remove(&args.provider);
    }
    if remove_provider {
        catalog.remove(&args.provider);
    }
    sync_primary_agent(&mut config);
    let (validated, canonical) = canonicalize_for_write(&config)?;
    replace_catalog_then_config(
        &mut secrets,
        catalog,
        previous_catalog,
        &validated,
        &home,
        &config_path,
        &canonical,
    )?;
    prune_migrated_flat_secrets_with_candidates(
        &validated,
        &mut secrets,
        &migration.cleanup_candidates,
    )?;
    print_credential_mutation(output, "deleted", &args.provider, args.alias.as_deref())
}

pub(super) fn run_credential_list(
    args: AgentProviderCredentialListArgs,
    output: OutputFormat,
) -> Result<()> {
    if let Some(provider_id) = args.provider.as_deref() {
        validate_catalog_provider(provider_id)?;
    }
    let home = home_dir()?;
    let secrets = SecretStore::open(&home)?;
    let credentials = secrets
        .provider_credentials()
        .iter()
        .filter(|(provider_id, _)| {
            args.provider
                .as_deref()
                .is_none_or(|requested| requested == provider_id.as_str())
        })
        .map(|(provider_id, set)| credential_set_json(provider_id, set))
        .collect::<Vec<_>>();
    if let Some(provider_id) = args.provider.as_deref()
        && credentials.is_empty()
    {
        return Err(missing_provider_credential(provider_id));
    }
    if output.is_json() {
        print_json(&serde_json::json!({ "provider_credentials": credentials }))?;
    } else if credentials.is_empty() {
        println!("provider credentials: none");
    } else {
        for credential in credentials {
            println!(
                "provider: {}",
                credential["provider"].as_str().unwrap_or("?")
            );
            println!("mode: {}", credential["mode"].as_str().unwrap_or("?"));
            for alias in credential["aliases"].as_array().into_iter().flatten() {
                println!(
                    "  alias: {} env={}",
                    alias["alias"].as_str().unwrap_or("(aliasless)"),
                    alias["env_names"]
                        .as_array()
                        .map(|values| values
                            .iter()
                            .filter_map(|value| value.as_str())
                            .collect::<Vec<_>>()
                            .join(","))
                        .unwrap_or_default(),
                );
            }
        }
    }
    Ok(())
}

pub(super) fn collect_credential(
    provider_id: &str,
    from_secret: &[String],
    secrets: &SecretStore,
) -> Result<ProviderCredential> {
    let primary = env_var_for_provider_id(provider_id).ok_or_else(|| StackError::InvalidParam {
        field: "provider",
        reason: format!("provider `{provider_id}` has no canonical API-key env var"),
    })?;
    let required = std::iter::once(primary)
        .chain(companion_env_refs_for_provider_id(provider_id))
        .collect::<BTreeSet<_>>();
    let optional = optional_env_refs_for_provider_id(provider_id)
        .into_iter()
        .collect::<BTreeSet<_>>();
    let allowed = required
        .iter()
        .chain(optional.iter())
        .copied()
        .collect::<BTreeSet<_>>();
    let mut values = BTreeMap::new();
    let mut source_refs = BTreeMap::new();
    if from_secret.is_empty() {
        if !io::stdin().is_terminal() {
            return Err(StackError::InvalidParam {
                field: "from-secret",
                reason: "non-interactive credential input requires --from-secret ENV=REF"
                    .to_owned(),
            });
        }
        for env_name in &required {
            let value = rpassword::prompt_password(format!("{provider_id} {env_name}: "))
                .map_err(|source| StackError::ServeIo { source })?;
            if value.is_empty() {
                return Err(StackError::InvalidParam {
                    field: "credential",
                    reason: format!("required field `{env_name}` must not be empty"),
                });
            }
            values.insert((*env_name).to_owned(), value);
        }
        for env_name in optional {
            let value = rpassword::prompt_password(format!(
                "{provider_id} {env_name} (optional, blank to skip): "
            ))
            .map_err(|source| StackError::ServeIo { source })?;
            if !value.is_empty() {
                values.insert(env_name.to_owned(), value);
            }
        }
    } else {
        for assignment in from_secret {
            let (env_name, source_ref) =
                assignment
                    .split_once('=')
                    .ok_or_else(|| StackError::InvalidParam {
                        field: "from-secret",
                        reason: format!("`{assignment}` must use ENV=REF"),
                    })?;
            if !allowed.contains(env_name) {
                return Err(StackError::InvalidParam {
                    field: "from-secret",
                    reason: format!(
                        "`{env_name}` is not a credential field for provider `{provider_id}`"
                    ),
                });
            }
            if values.contains_key(env_name) {
                return Err(StackError::InvalidParam {
                    field: "from-secret",
                    reason: format!("duplicate field `{env_name}`"),
                });
            }
            if !is_valid_secret_ref_name(source_ref) {
                return Err(StackError::InvalidSecretRefName {
                    name: source_ref.to_owned(),
                });
            }
            values.insert(env_name.to_owned(), secrets.get(source_ref)?.to_owned());
            source_refs.insert(env_name.to_owned(), source_ref.to_owned());
        }
        for env_name in required {
            if values.get(env_name).is_none_or(String::is_empty) {
                return Err(StackError::InvalidParam {
                    field: "from-secret",
                    reason: format!("missing or empty required field `{env_name}`"),
                });
            }
        }
    }
    Ok(ProviderCredential::new(values, source_refs))
}

fn print_credential_mutation(
    output: OutputFormat,
    action: &'static str,
    provider_id: &str,
    alias: Option<&str>,
) -> Result<()> {
    if output.is_json() {
        print_json(&serde_json::json!({
            "status": action,
            "provider": provider_id,
            "alias": alias,
            "restart_required_for_running_targets": true,
        }))?;
    } else {
        println!("provider credential: {action}");
        println!("provider: {provider_id}");
        if let Some(alias) = alias {
            println!("alias: {alias}");
        }
        println!("running targets must be restarted to load this change");
    }
    Ok(())
}

fn credential_set_json(provider_id: &str, set: &ProviderCredentialSet) -> serde_json::Value {
    let aliases = if let Some(credential) = set.sole.as_ref() {
        vec![credential_json(None, credential)]
    } else {
        set.aliases
            .iter()
            .map(|(alias, credential)| credential_json(Some(alias), credential))
            .collect()
    };
    serde_json::json!({
        "provider": provider_id,
        "mode": if set.is_promoted() { "aliases" } else { "aliasless" },
        "aliases": aliases,
    })
}

fn credential_json(alias: Option<&str>, credential: &ProviderCredential) -> serde_json::Value {
    serde_json::json!({
        "alias": alias,
        "env_names": credential.values.keys().collect::<Vec<_>>(),
        "source_refs": credential.source_refs,
    })
}
