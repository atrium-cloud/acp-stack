use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::{self, IsTerminal};

use crate::config::{AgentConfig, AgentProviderConfig, Config};
use crate::error::{Result, StackError};
use crate::fs_util::atomic_write_owner_only;
use crate::runtime::agent::provider_keys::{
    companion_env_refs_for_provider_id, env_var_for_provider_id, optional_env_refs_for_provider_id,
    provider_id_is_known,
};
use crate::secrets::{ProviderCredential, ProviderCredentialSet, SecretStore};

use super::provider_shared::{
    ensure_credential_value_is_new, ensure_provider_settings, provision_configured_targets,
    resolve_alias, resolve_configured_target_environments, sync_primary_agent,
};

#[derive(Debug)]
pub(super) struct LegacyCredentialMigration {
    pub(super) changed: bool,
    pub(super) cleanup_candidates: BTreeSet<String>,
}

pub(super) fn migrate_legacy_provider_credentials(
    config: &mut Config,
    secrets: &mut SecretStore,
) -> Result<LegacyCredentialMigration> {
    let mut refs_by_provider: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for target in &config.array.targets {
        collect_legacy_provider_ref(target.agent.provider.as_ref(), &mut refs_by_provider);
        if let Some(subagent_provider) = target
            .agent
            .subagent
            .as_ref()
            .and_then(|subagent| subagent.provider.as_ref())
        {
            collect_legacy_provider_ref(Some(subagent_provider), &mut refs_by_provider);
        }
    }
    if refs_by_provider.is_empty() {
        return Ok(LegacyCredentialMigration {
            changed: false,
            cleanup_candidates: BTreeSet::new(),
        });
    }
    let cleanup_candidates = refs_by_provider
        .values()
        .flat_map(BTreeSet::iter)
        .cloned()
        .collect();
    let mut catalog = secrets.provider_credentials().clone();
    let mut aliases_by_provider_ref: BTreeMap<(String, String), String> = BTreeMap::new();
    for (provider_id, refs) in &refs_by_provider {
        let Some(mut existing) = catalog.remove(provider_id) else {
            let mut credentials = Vec::new();
            for secret_ref in refs {
                credentials.push((
                    secret_ref.clone(),
                    credential_from_legacy_ref(provider_id, secret_ref, secrets)?,
                ));
            }
            if credentials.len() == 1 {
                catalog.insert(
                    provider_id.clone(),
                    ProviderCredentialSet::aliasless(credentials.remove(0).1),
                );
                continue;
            }
            require_interactive_legacy_aliases(provider_id)?;
            let mut aliases = BTreeMap::new();
            for (secret_ref, credential) in credentials {
                let alias = resolve_alias(
                    None,
                    &format!("alias for provider `{provider_id}` legacy ref `{secret_ref}`"),
                )?;
                if aliases.insert(alias.clone(), credential).is_some() {
                    return Err(StackError::InvalidParam {
                        field: "alias",
                        reason: format!("duplicate alias `{alias}`"),
                    });
                }
                aliases_by_provider_ref.insert((provider_id.clone(), secret_ref), alias);
            }
            catalog.insert(
                provider_id.clone(),
                ProviderCredentialSet::promoted(aliases),
            );
            continue;
        };

        if !existing.is_promoted() {
            let sole =
                existing
                    .sole
                    .as_ref()
                    .ok_or_else(|| StackError::SecretStorePlaintextInvalid {
                        reason: format!(
                            "provider credential `{provider_id}` is missing its aliasless value"
                        ),
                    })?;
            let all_refs_match = refs.iter().try_fold(true, |matches, secret_ref| {
                Ok::<_, StackError>(
                    matches
                        && credential_matches_legacy_ref(provider_id, sole, secret_ref, secrets)?,
                )
            })?;
            if all_refs_match {
                catalog.insert(provider_id.clone(), existing);
                continue;
            }
            require_interactive_legacy_aliases(provider_id)?;
            let existing_alias = resolve_alias(
                None,
                &format!("alias for provider `{provider_id}` existing credential"),
            )?;
            let sole =
                existing
                    .sole
                    .take()
                    .ok_or_else(|| StackError::SecretStorePlaintextInvalid {
                        reason: format!(
                            "provider credential `{provider_id}` is missing its aliasless value"
                        ),
                    })?;
            existing =
                ProviderCredentialSet::promoted(BTreeMap::from([(existing_alias.clone(), sole)]));
        }

        for secret_ref in refs {
            if let Some(alias) = matching_legacy_alias(provider_id, &existing, secret_ref, secrets)?
            {
                aliases_by_provider_ref.insert((provider_id.clone(), secret_ref.clone()), alias);
                continue;
            }
            require_interactive_legacy_aliases(provider_id)?;
            let alias = resolve_alias(
                None,
                &format!("alias for provider `{provider_id}` legacy ref `{secret_ref}`"),
            )?;
            if existing.aliases.contains_key(&alias) {
                return Err(StackError::InvalidParam {
                    field: "alias",
                    reason: format!("provider `{provider_id}` already has alias `{alias}`"),
                });
            }
            let credential = credential_from_legacy_ref(provider_id, secret_ref, secrets)?;
            ensure_credential_value_is_new(provider_id, &existing, &credential)?;
            existing.aliases.insert(alias.clone(), credential);
            aliases_by_provider_ref.insert((provider_id.clone(), secret_ref.clone()), alias);
        }
        catalog.insert(provider_id.clone(), existing);
    }

    let migrated_providers = refs_by_provider.keys().cloned().collect::<HashSet<_>>();
    for target in &mut config.array.targets {
        migrate_agent_provider_refs(
            &mut target.agent,
            &migrated_providers,
            &aliases_by_provider_ref,
        );
    }
    sync_primary_agent(config);
    secrets.stage_provider_credentials(catalog)?;
    Ok(LegacyCredentialMigration {
        changed: true,
        cleanup_candidates,
    })
}

fn collect_legacy_provider_ref(
    provider: Option<&AgentProviderConfig>,
    refs_by_provider: &mut BTreeMap<String, BTreeSet<String>>,
) {
    let Some(provider) = provider else {
        return;
    };
    if provider.custom.is_some() || !provider_id_is_known(&provider.id) {
        return;
    }
    if let Some(api_key_ref) = provider.api_key_ref.as_ref() {
        refs_by_provider
            .entry(provider.id.clone())
            .or_default()
            .insert(api_key_ref.clone());
    }
}

fn credential_from_legacy_ref(
    provider_id: &str,
    api_key_ref: &str,
    secrets: &SecretStore,
) -> Result<ProviderCredential> {
    let primary = env_var_for_provider_id(provider_id).ok_or_else(|| StackError::InvalidParam {
        field: "provider",
        reason: format!("provider `{provider_id}` has no canonical API-key env var"),
    })?;
    let mut values = BTreeMap::new();
    let mut source_refs = BTreeMap::new();
    values.insert(primary.to_owned(), secrets.get(api_key_ref)?.to_owned());
    source_refs.insert(primary.to_owned(), api_key_ref.to_owned());
    for env_name in companion_env_refs_for_provider_id(provider_id) {
        values.insert(env_name.to_owned(), secrets.get(env_name)?.to_owned());
        source_refs.insert(env_name.to_owned(), env_name.to_owned());
    }
    for env_name in optional_env_refs_for_provider_id(provider_id) {
        if secrets.contains(env_name) {
            values.insert(env_name.to_owned(), secrets.get(env_name)?.to_owned());
            source_refs.insert(env_name.to_owned(), env_name.to_owned());
        }
    }
    let mut credential = ProviderCredential::new(values, source_refs);
    credential.migrated = true;
    Ok(credential)
}

fn require_interactive_legacy_aliases(provider_id: &str) -> Result<()> {
    if io::stdin().is_terminal() {
        return Ok(());
    }
    Err(StackError::InvalidParam {
        field: "alias",
        reason: format!(
            "provider `{provider_id}` has multiple legacy credentials; run interactively to assign aliases"
        ),
    })
}

fn matching_legacy_alias(
    provider_id: &str,
    credentials: &ProviderCredentialSet,
    secret_ref: &str,
    secrets: &SecretStore,
) -> Result<Option<String>> {
    for (alias, credential) in &credentials.aliases {
        if credential_matches_legacy_ref(provider_id, credential, secret_ref, secrets)? {
            return Ok(Some(alias.clone()));
        }
    }
    Ok(None)
}

fn credential_matches_legacy_ref(
    provider_id: &str,
    credential: &ProviderCredential,
    secret_ref: &str,
    secrets: &SecretStore,
) -> Result<bool> {
    let primary = env_var_for_provider_id(provider_id).ok_or_else(|| StackError::InvalidParam {
        field: "provider",
        reason: format!("provider `{provider_id}` has no canonical API-key env var"),
    })?;
    Ok(credential.values.get(primary).map(String::as_str) == Some(secrets.get(secret_ref)?))
}

fn migrated_flat_secret_candidates(secrets: &SecretStore) -> BTreeSet<String> {
    secrets
        .provider_credentials()
        .values()
        .flat_map(|set| {
            set.sole
                .iter()
                .chain(set.aliases.values())
                .filter(|credential| credential.migrated)
                .flat_map(|credential| credential.source_refs.values())
        })
        .cloned()
        .collect()
}

pub(super) fn prune_migrated_flat_secrets_with_candidates(
    config: &Config,
    secrets: &mut SecretStore,
    retained_candidates: &BTreeSet<String>,
) -> Result<()> {
    let candidates = migrated_flat_secret_candidates(secrets)
        .into_iter()
        .chain(retained_candidates.iter().cloned())
        .filter(|name| !config_references_secret(config, name))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return Ok(());
    }
    secrets.replace_provider_credentials(secrets.provider_credentials().clone(), &candidates)
}

pub(super) fn replace_catalog_then_config(
    secrets: &mut SecretStore,
    catalog: BTreeMap<String, ProviderCredentialSet>,
    previous: BTreeMap<String, ProviderCredentialSet>,
    config: &Config,
    home: &std::path::Path,
    config_path: &std::path::Path,
    canonical: &str,
) -> Result<()> {
    secrets.replace_provider_credentials(catalog, &[])?;
    let apply_result = resolve_configured_target_environments(config, secrets)
        .and_then(|()| provision_configured_targets(config, home).map(|_| ()))
        .and_then(|()| atomic_write_owner_only(config_path, canonical.as_bytes()));
    if let Err(config_error) = apply_result {
        if let Err(rollback_error) = secrets.replace_provider_credentials(previous, &[]) {
            return Err(StackError::ProviderCredentialRollbackFailed {
                original: config_error.to_string(),
                rollback: rollback_error.to_string(),
            });
        }
        return Err(config_error);
    }
    Ok(())
}

pub(super) fn persist_migrated_catalog_then_config(
    secrets: &mut SecretStore,
    previous: BTreeMap<String, ProviderCredentialSet>,
    migration_changed: bool,
    config_path: &std::path::Path,
    canonical: &str,
) -> Result<()> {
    if !migration_changed {
        return atomic_write_owner_only(config_path, canonical.as_bytes());
    }
    secrets.replace_provider_credentials(secrets.provider_credentials().clone(), &[])?;
    if let Err(config_error) = atomic_write_owner_only(config_path, canonical.as_bytes()) {
        if let Err(rollback_error) = secrets.replace_provider_credentials(previous, &[]) {
            return Err(StackError::ProviderCredentialRollbackFailed {
                original: config_error.to_string(),
                rollback: rollback_error.to_string(),
            });
        }
        return Err(config_error);
    }
    Ok(())
}

fn migrate_agent_provider_refs(
    agent: &mut AgentConfig,
    migrated_providers: &HashSet<String>,
    aliases_by_provider_ref: &BTreeMap<(String, String), String>,
) {
    let provider_ids = agent
        .provider
        .iter()
        .chain(
            agent
                .subagent
                .as_ref()
                .and_then(|subagent| subagent.provider.as_ref()),
        )
        .filter(|provider| provider.custom.is_none() && migrated_providers.contains(&provider.id))
        .map(|provider| provider.id.clone())
        .collect::<BTreeSet<_>>();
    let mut agent_removals = BTreeSet::new();
    let main_selection = migrate_one_provider_ref(
        agent.provider.as_mut(),
        migrated_providers,
        aliases_by_provider_ref,
        &mut agent_removals,
    );
    let subagent_selection = agent.subagent.as_mut().and_then(|subagent| {
        migrate_one_provider_ref(
            subagent.provider.as_mut(),
            migrated_providers,
            aliases_by_provider_ref,
            &mut agent_removals,
        )
    });
    for provider_id in provider_ids {
        agent_removals.extend(
            companion_env_refs_for_provider_id(&provider_id)
                .into_iter()
                .map(str::to_owned),
        );
        agent_removals.extend(
            optional_env_refs_for_provider_id(&provider_id)
                .into_iter()
                .map(str::to_owned),
        );
    }
    for (provider_id, alias) in main_selection.into_iter().chain(subagent_selection) {
        ensure_provider_settings(agent)
            .selected_aliases
            .insert(provider_id, alias);
    }
    let remaining_provider_refs = agent
        .provider
        .iter()
        .chain(
            agent
                .subagent
                .as_ref()
                .and_then(|subagent| subagent.provider.as_ref()),
        )
        .filter_map(|provider| provider.api_key_ref.as_ref())
        .cloned()
        .collect::<BTreeSet<_>>();
    agent
        .env
        .retain(|name| !agent_removals.contains(name) || remaining_provider_refs.contains(name));
}

fn migrate_one_provider_ref(
    provider: Option<&mut AgentProviderConfig>,
    migrated_providers: &HashSet<String>,
    aliases_by_provider_ref: &BTreeMap<(String, String), String>,
    candidate_removals: &mut BTreeSet<String>,
) -> Option<(String, String)> {
    let provider = provider?;
    if provider.custom.is_some() || !migrated_providers.contains(&provider.id) {
        return None;
    }
    let api_key_ref = provider.api_key_ref.take()?;
    candidate_removals.insert(api_key_ref.clone());
    aliases_by_provider_ref
        .get(&(provider.id.clone(), api_key_ref))
        .cloned()
        .map(|alias| (provider.id.clone(), alias))
}

fn config_references_secret(config: &Config, name: &str) -> bool {
    if config.array.targets.iter().any(|target| {
        target.agent.env.iter().any(|value| value == name)
            || target
                .agent
                .provider
                .as_ref()
                .and_then(|provider| provider.api_key_ref.as_deref())
                == Some(name)
            || target
                .agent
                .subagent
                .as_ref()
                .and_then(|subagent| subagent.provider.as_ref())
                .and_then(|provider| provider.api_key_ref.as_deref())
                == Some(name)
    }) {
        return true;
    }
    if config.edge.cloudflare.as_ref().is_some_and(|edge| {
        edge.api_token_ref.as_deref() == Some(name) || edge.account_id_ref.as_deref() == Some(name)
    }) {
        return true;
    }
    if config.logging.supabase.as_ref().is_some_and(|supabase| {
        supabase.api_key_ref == name || supabase.db_url_ref.as_deref() == Some(name)
    }) {
        return true;
    }
    if config
        .workspace
        .code_sources
        .iter()
        .any(|source| source.credential_ref.as_deref() == Some(name))
        || config.workspace.data_sources.iter().any(|source| {
            source.access_key_ref.as_deref() == Some(name)
                || source.secret_key_ref.as_deref() == Some(name)
        })
    {
        return true;
    }
    config.mcp.servers.iter().any(|server| match server {
        crate::config::McpServerConfig::Stdio(server) => {
            server.env.iter().any(|value| value == name)
        }
        crate::config::McpServerConfig::Http(server) => {
            server.headers.iter().any(|header| header.value_ref == name)
        }
    })
}
