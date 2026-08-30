use std::collections::BTreeMap;
use std::path::Path;

use crate::config::{AgentAdapterConfig, AgentProviderConfig, AgentProvidersConfig, Config};
use crate::error::{Result, StackError};
use crate::runtime::agent::provider_keys::{
    agent_provider_accepts_endpoint_override, api_key_ref_can_migrate_for_provider,
    env_refs_for_agent_id, env_var_for_agent_provider_id, provider_id_is_known,
    provider_id_supports_agent, provider_uses_agent_native_auth,
    required_env_refs_for_agent_provider_id,
};
use crate::runtime::install::agent_registry::{RegistryCatalog, RegistryEntry, RegistryKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSwitchRequest {
    pub target_agent: String,
    pub provider_id: Option<String>,
    pub api_key_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentSwitchPlan {
    pub old_agent_id: String,
    pub target_agent_id: String,
    pub provider_status: AgentSwitchProviderStatus,
    pub required_env_refs: Vec<String>,
    pub secret_migrations: Vec<AgentSwitchSecretMigration>,
    pub config: Config,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSwitchSecretMigration {
    pub from_ref: String,
    pub to_ref: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentSwitchProviderStatus {
    NotApplicable,
    Reused {
        provider_id: String,
        api_key_ref: Option<String>,
    },
    Set {
        provider_id: String,
        api_key_ref: Option<String>,
    },
}

impl AgentSwitchProviderStatus {
    pub fn label(&self) -> &'static str {
        match self {
            Self::NotApplicable => "not_applicable",
            Self::Reused { .. } => "reused",
            Self::Set { .. } => "set",
        }
    }

    pub fn provider_id(&self) -> Option<&str> {
        match self {
            Self::NotApplicable => None,
            Self::Reused { provider_id, .. } | Self::Set { provider_id, .. } => Some(provider_id),
        }
    }

    pub fn api_key_ref(&self) -> Option<&str> {
        match self {
            Self::NotApplicable => None,
            Self::Reused { api_key_ref, .. } | Self::Set { api_key_ref, .. } => {
                api_key_ref.as_deref()
            }
        }
    }
}

/// Every agent-change path MUST call this before committing: a stored endpoint
/// override lives in the agent's native config, and a target that cannot carry
/// it would silently send protected traffic back to the vendor.
pub fn ensure_endpoint_override_survives_target(
    home: &Path,
    target_agent_id: &str,
    target_supports_base_url: bool,
    target_provider_id: Option<&str>,
) -> Result<()> {
    let Some(endpoint) = crate::secrets::managed_provider_endpoint_override_for_home(home)? else {
        return Ok(());
    };
    if !target_supports_base_url {
        return Err(StackError::InvalidParam {
            field: "agent",
            reason: format!(
                "agent `{target_agent_id}` cannot route a provider through a custom endpoint, but \
                 provider `{}` is currently routed through one; clear the managed-state \
                 namespace's credential endpoint before switching",
                endpoint.provider_id
            ),
        });
    }
    // Kilo and Antigravity provision the override for the credential's provider whatever the
    // configured selection, so the pair check applies to them regardless of the target provider.
    let override_follows_credential = matches!(
        target_agent_id,
        crate::runtime::agent::provider_keys::KILO_AGENT_ID
            | crate::runtime::agent::provider_keys::ANTIGRAVITY_AGENT_ID
    );
    if (override_follows_credential || target_provider_id == Some(endpoint.provider_id.as_str()))
        && !agent_provider_accepts_endpoint_override(target_agent_id, &endpoint.provider_id)
    {
        return Err(StackError::InvalidParam {
            field: "agent",
            reason: format!(
                "agent `{target_agent_id}` cannot route provider `{}` through a custom endpoint; \
                 clear the managed-state namespace's credential endpoint, or select `openrouter` \
                 or a configured custom provider",
                endpoint.provider_id
            ),
        });
    }
    Ok(())
}

pub fn plan_agent_switch(
    home: &Path,
    current: &Config,
    registry: &RegistryCatalog,
    request: AgentSwitchRequest,
) -> Result<AgentSwitchPlan> {
    let entry = registry.lookup_required(&request.target_agent)?;
    entry.ensure_supported()?;
    if current.agent.id == entry.id {
        return Err(StackError::InvalidParam {
            field: "agent",
            reason: format!("agent `{}` is already configured", entry.id),
        });
    }

    let old_agent_id = current.agent.id.clone();
    let mut config = current.clone();
    apply_switch_registry_entry(&mut config, entry);
    let (provider_status, required_env_refs, secret_migrations) =
        configure_switch_provider(current, &mut config, entry, request)?;
    // MUST run after provider resolution so the pair-level refusal sees the
    // provider the target would actually run.
    ensure_endpoint_override_survives_target(
        home,
        &entry.id,
        entry.set_provider_base_url,
        provider_status.provider_id(),
    )?;

    Ok(AgentSwitchPlan {
        old_agent_id,
        target_agent_id: entry.id.clone(),
        provider_status,
        required_env_refs,
        secret_migrations,
        config,
    })
}

pub fn adapter_from_registry_entry(entry: &RegistryEntry) -> Option<AgentAdapterConfig> {
    if !matches!(entry.kind, RegistryKind::Adapter) {
        return None;
    }
    let harness = entry.harness.as_ref()?;
    let adapter = entry.adapter.as_ref()?;
    Some(AgentAdapterConfig {
        id: adapter.id.clone(),
        name: entry.name.clone(),
        upstream_agent: harness.id.clone(),
        source_url: adapter.github.as_deref().and_then(|github| {
            crate::runtime::install::agent_registry::github_url_from_value(
                &entry.id,
                "adapter.github",
                github,
            )
            .ok()
        }),
    })
}

fn apply_switch_registry_entry(config: &mut Config, entry: &RegistryEntry) {
    config.agent.id = entry.id.clone();
    config.agent.name = entry.name.clone();
    config.agent.cwd = Some(config.workspace.root.clone());
    config.agent.env = default_agent_env_refs(&entry.id);
    config.agent.mode = None;
    config.agent.model = None;
    config.agent.effort = None;
    config.agent.config_options = Default::default();
    config.agent.provider = None;
    config.agent.providers = None;
    config.agent.subagent = None;
    config.agent.expected_sha256 = None;
    config.agent.restart = "on-crash".to_owned();
    config.agent.harness_version = None;
    config.agent.adapter = adapter_from_registry_entry(entry);
    // Same-agent switches are rejected upstream, so a switch always targets a
    // different agent and the operator's designated adapter never carries over.
    config.agent.adapter_override = None;
    config.agent.install = None;

    match entry.kind {
        RegistryKind::Native => {
            let harness = entry.harness.as_ref().expect("validated registry harness");
            config.agent.command = harness.id.clone();
            config.agent.args = harness.acp_args.clone();
        }
        RegistryKind::Adapter => {
            let adapter = entry.adapter.as_ref().expect("validated registry adapter");
            config.agent.command = adapter.id.clone();
            config.agent.args = Vec::new();
        }
    }
}

fn configure_switch_provider(
    current: &Config,
    config: &mut Config,
    entry: &RegistryEntry,
    request: AgentSwitchRequest,
) -> Result<(
    AgentSwitchProviderStatus,
    Vec<String>,
    Vec<AgentSwitchSecretMigration>,
)> {
    if !entry.set_provider {
        if request.provider_id.is_some() || request.api_key_ref.is_some() {
            return Err(StackError::InvalidParam {
                field: "provider",
                reason: format!("{} does not support provider configuration", entry.name),
            });
        }
        return Ok((
            AgentSwitchProviderStatus::NotApplicable,
            default_agent_env_refs(&entry.id),
            Vec::new(),
        ));
    }

    if let Some(provider_id) = request.provider_id {
        let api_key_ref_was_explicit = request.api_key_ref.is_some();
        let (provider, refs, mut secret_migrations) = build_provider_for_target(
            &entry.id,
            &entry.name,
            provider_id.clone(),
            request.api_key_ref,
            AgentSwitchProviderStatusKind::Set,
            None,
            false,
        )?;
        if !api_key_ref_was_explicit
            && let Some(current_provider) = current.agent.provider.as_ref()
            && current_provider.custom.is_none()
            && current_provider.id == provider_id
            && let (Some(from_ref), Some(to_ref)) = (
                current_provider.api_key_ref.as_deref(),
                provider.api_key_ref.as_deref(),
            )
            && from_ref != to_ref
            && api_key_ref_can_migrate_for_provider(&provider_id, from_ref, to_ref)
        {
            secret_migrations.push(AgentSwitchSecretMigration {
                from_ref: from_ref.to_owned(),
                to_ref: to_ref.to_owned(),
            });
        }
        config.agent.provider = Some(provider);
        append_missing_refs(&mut config.agent.env, &refs);
        crate::runtime::agent::provider_keys::reconcile_kimi_lane_env_declarations(
            &mut config.agent,
        );
        return Ok((
            AgentSwitchProviderStatus::Set {
                provider_id: config
                    .agent
                    .provider
                    .as_ref()
                    .expect("provider set")
                    .id
                    .clone(),
                api_key_ref: config
                    .agent
                    .provider
                    .as_ref()
                    .and_then(|provider| provider.api_key_ref.clone()),
            },
            refs,
            secret_migrations,
        ));
    }

    let Some(current_provider) = current.agent.provider.as_ref() else {
        return Err(StackError::InvalidParam {
            field: "provider",
            reason: format!(
                "cannot infer provider for {}; pass --provider <provider-id>",
                entry.name
            ),
        });
    };
    if current_provider.custom.is_some() {
        return Err(StackError::InvalidParam {
            field: "provider",
            reason: "custom provider migration is not supported; pass --provider and --api-key-ref"
                .to_owned(),
        });
    }
    let uses_structured_credential = request.api_key_ref.is_none()
        && current_provider.api_key_ref.is_none()
        && !provider_uses_agent_native_auth(&current.agent.id, &current_provider.id)
        && !provider_uses_agent_native_auth(&entry.id, &current_provider.id);
    if uses_structured_credential {
        if !provider_id_supports_agent(&current_provider.id, &entry.id) {
            return Err(StackError::InvalidParam {
                field: "provider",
                reason: format!(
                    "provider `{}` is not supported for agent `{}`",
                    current_provider.id, entry.id
                ),
            });
        }
        config.agent.provider = Some(AgentProviderConfig {
            id: current_provider.id.clone(),
            model: None,
            api_key_ref: None,
            custom: None,
        });
        if let Some(alias) = current
            .agent
            .providers
            .as_ref()
            .and_then(|providers| providers.selected_aliases.get(&current_provider.id))
        {
            config.agent.providers = Some(AgentProvidersConfig {
                active: vec![current_provider.id.clone()],
                selected_aliases: BTreeMap::from([(current_provider.id.clone(), alias.clone())]),
            });
        }
        crate::runtime::agent::provider_keys::reconcile_kimi_lane_env_declarations(
            &mut config.agent,
        );
        return Ok((
            AgentSwitchProviderStatus::Reused {
                provider_id: current_provider.id.clone(),
                api_key_ref: None,
            },
            Vec::new(),
            Vec::new(),
        ));
    }
    let api_key_ref_was_explicit = request.api_key_ref.is_some();
    let explicit_api_key_ref = request.api_key_ref;
    let inherited_api_key_ref = current_provider.api_key_ref.clone();
    let current_api_key_ref = explicit_api_key_ref.or_else(|| inherited_api_key_ref.clone());
    let (provider, refs, secret_migrations) = build_provider_for_target(
        &entry.id,
        &entry.name,
        current_provider.id.clone(),
        current_api_key_ref,
        AgentSwitchProviderStatusKind::Reused,
        inherited_api_key_ref.as_deref(),
        api_key_ref_was_explicit,
    )?;
    config.agent.provider = Some(provider);
    append_missing_refs(&mut config.agent.env, &refs);
    crate::runtime::agent::provider_keys::reconcile_kimi_lane_env_declarations(&mut config.agent);
    Ok((
        AgentSwitchProviderStatus::Reused {
            provider_id: config
                .agent
                .provider
                .as_ref()
                .expect("provider set")
                .id
                .clone(),
            api_key_ref: config
                .agent
                .provider
                .as_ref()
                .and_then(|provider| provider.api_key_ref.clone()),
        },
        refs,
        secret_migrations,
    ))
}

#[derive(Debug, Clone, Copy)]
enum AgentSwitchProviderStatusKind {
    Reused,
    Set,
}

fn build_provider_for_target(
    target_agent_id: &str,
    target_agent_name: &str,
    provider_id: String,
    requested_api_key_ref: Option<String>,
    kind: AgentSwitchProviderStatusKind,
    inherited_api_key_ref: Option<&str>,
    api_key_ref_was_explicit: bool,
) -> Result<(
    AgentProviderConfig,
    Vec<String>,
    Vec<AgentSwitchSecretMigration>,
)> {
    if !provider_id_is_known(&provider_id) {
        return Err(StackError::InvalidParam {
            field: "provider",
            reason: format!("provider `{provider_id}` is not listed in provider/env mapping"),
        });
    }
    if !provider_id_supports_agent(&provider_id, target_agent_id) {
        return Err(StackError::InvalidParam {
            field: "provider",
            reason: format!(
                "provider `{provider_id}` is not supported for agent `{target_agent_id}`"
            ),
        });
    }

    let default_ref = env_var_for_agent_provider_id(target_agent_id, &provider_id);
    let native_auth = provider_uses_agent_native_auth(target_agent_id, &provider_id);
    if native_auth && requested_api_key_ref.is_some() {
        return Err(StackError::InvalidParam {
            field: "api-key-ref",
            reason: format!(
                "{target_agent_name} provider `{provider_id}` uses agent-native auth; do not pass --api-key-ref"
            ),
        });
    }
    if native_auth {
        let refs = required_env_refs_for_agent_provider_id(target_agent_id, &provider_id, None);
        return Ok((
            AgentProviderConfig {
                id: provider_id,
                model: None,
                api_key_ref: None,
                custom: None,
            },
            refs,
            Vec::new(),
        ));
    }
    let default_ref = default_ref.ok_or_else(|| StackError::AgentConfigProvision {
        path: std::path::PathBuf::from("provider/env mapping"),
        reason: format!(
            "{} provider `{provider_id}` has no API-key env mapping",
            target_agent_name
        ),
    })?;
    let mut secret_migrations = Vec::new();
    let mut api_key_ref = requested_api_key_ref.unwrap_or_else(|| default_ref.to_owned());
    if matches!(kind, AgentSwitchProviderStatusKind::Reused) && api_key_ref != default_ref {
        if !api_key_ref_was_explicit
            && inherited_api_key_ref == Some(api_key_ref.as_str())
            && api_key_ref_can_migrate_for_provider(&provider_id, &api_key_ref, default_ref)
        {
            secret_migrations.push(AgentSwitchSecretMigration {
                from_ref: api_key_ref,
                to_ref: default_ref.to_owned(),
            });
            api_key_ref = default_ref.to_owned();
        } else {
            return Err(StackError::InvalidParam {
                field: "api-key-ref",
                reason: format!(
                    "cannot reuse `{api_key_ref}` for {target_agent_name}; pass --provider {provider_id} --api-key-ref {default_ref}"
                ),
            });
        }
    }
    let refs =
        required_env_refs_for_agent_provider_id(target_agent_id, &provider_id, Some(&api_key_ref));
    Ok((
        AgentProviderConfig {
            id: provider_id,
            model: None,
            api_key_ref: Some(api_key_ref),
            custom: None,
        },
        refs,
        secret_migrations,
    ))
}

fn append_missing_refs(env: &mut Vec<String>, refs: &[String]) {
    for env_ref in refs {
        if !crate::config::agent_env_declares(env, env_ref) {
            env.push(env_ref.clone());
        }
    }
}

fn default_agent_env_refs(agent_id: &str) -> Vec<String> {
    env_refs_for_agent_id(agent_id)
        .into_iter()
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        HttpHeaderRef, McpConfig, McpHttpServer, McpServerConfig, McpStdioServer,
        load_config_from_str,
    };

    fn valid_config() -> Config {
        load_config_from_str(include_str!(
            "../../../tests/fixtures/valid-opencode-stack.toml"
        ))
        .expect("fixture parses")
    }

    fn mcp_config() -> McpConfig {
        McpConfig {
            servers: vec![
                McpServerConfig::Stdio(McpStdioServer {
                    name: "local-tools".to_owned(),
                    command: "/usr/local/bin/local-tools-mcp".to_owned(),
                    args: vec!["--stdio".to_owned()],
                    env: vec!["LOCAL_TOOLS_TOKEN".to_owned()],
                }),
                McpServerConfig::Http(McpHttpServer {
                    name: "linear".to_owned(),
                    url: "https://mcp.linear.app/mcp".to_owned(),
                    headers: vec![HttpHeaderRef::from_ref("Authorization", "LINEAR_API_KEY")],
                }),
            ],
        }
    }

    #[test]
    fn reuses_provider_when_target_default_ref_matches() {
        let mut config = valid_config();
        config.agent.provider = Some(AgentProviderConfig {
            id: "openai".to_owned(),
            model: Some("openai/gpt-5.5".to_owned()),
            api_key_ref: Some("OPENAI_API_KEY".to_owned()),
            custom: None,
        });
        let registry = RegistryCatalog::load_embedded().expect("registry loads");

        let plan = plan_agent_switch_locked(
            &config,
            &registry,
            AgentSwitchRequest {
                target_agent: "pi".to_owned(),
                provider_id: None,
                api_key_ref: None,
            },
        )
        .expect("switch planned");

        assert_eq!(plan.target_agent_id, "pi");
        assert_eq!(
            plan.provider_status,
            AgentSwitchProviderStatus::Reused {
                provider_id: "openai".to_owned(),
                api_key_ref: Some("OPENAI_API_KEY".to_owned())
            }
        );
        assert_eq!(
            plan.config
                .agent
                .provider
                .as_ref()
                .and_then(|provider| provider.model.as_ref()),
            None
        );
    }

    #[test]
    fn structured_provider_switch_preserves_default_alias_without_flat_ref() {
        let mut config = valid_config();
        config.agent.provider = Some(AgentProviderConfig {
            id: "opencode-go".to_owned(),
            model: Some("opencode-go/deepseek-v4-flash".to_owned()),
            api_key_ref: None,
            custom: None,
        });
        config.agent.providers = Some(AgentProvidersConfig {
            active: vec!["opencode-go".to_owned(), "openrouter".to_owned()],
            selected_aliases: BTreeMap::from([("opencode-go".to_owned(), "go_2".to_owned())]),
        });
        let registry = RegistryCatalog::load_embedded().expect("registry loads");

        let plan = plan_agent_switch_locked(
            &config,
            &registry,
            AgentSwitchRequest {
                target_agent: "pi".to_owned(),
                provider_id: None,
                api_key_ref: None,
            },
        )
        .expect("switch planned");

        assert_eq!(
            plan.provider_status,
            AgentSwitchProviderStatus::Reused {
                provider_id: "opencode-go".to_owned(),
                api_key_ref: None,
            }
        );
        assert_eq!(
            plan.config
                .agent
                .providers
                .as_ref()
                .and_then(|providers| providers.selected_aliases.get("opencode-go"))
                .map(String::as_str),
            Some("go_2")
        );
        assert_eq!(
            plan.config
                .agent
                .providers
                .as_ref()
                .map(|providers| providers.active.clone()),
            Some(vec!["opencode-go".to_owned()])
        );
        assert!(plan.required_env_refs.is_empty());
    }

    #[test]
    fn switch_clears_adapter_override_and_restores_registry_command() {
        let mut config = valid_config();
        config.agent.id = "goose".to_owned();
        config.agent.adapter_override = Some(crate::config::AgentAdapterOverrideConfig {
            command: "custom-acp".to_owned(),
            args: Vec::new(),
            github: None,
            install: crate::config::AgentAdapterOverrideInstall {
                shell: None,
                npm: Some(crate::config::AgentAdapterOverrideNpmInstall {
                    package: "custom-acp".to_owned(),
                    creates: "custom-acp".to_owned(),
                }),
                github: None,
            },
            update: Default::default(),
        });
        config.agent.command = "custom-acp".to_owned();
        let registry = RegistryCatalog::load_embedded().expect("registry loads");

        let plan = plan_agent_switch_locked(
            &config,
            &registry,
            AgentSwitchRequest {
                target_agent: "amp".to_owned(),
                provider_id: None,
                api_key_ref: None,
            },
        )
        .expect("switch planned");

        assert!(plan.config.agent.adapter_override.is_none());
        assert_eq!(plan.config.agent.command, "amp-acp");
    }

    #[test]
    fn switch_preserves_mcp_runtime_config() {
        let mut config = valid_config();
        let expected_mcp = mcp_config();
        config.mcp = expected_mcp.clone();
        let registry = RegistryCatalog::load_embedded().expect("registry loads");

        let plan = plan_agent_switch_locked(
            &config,
            &registry,
            AgentSwitchRequest {
                target_agent: "amp".to_owned(),
                provider_id: None,
                api_key_ref: None,
            },
        )
        .expect("switch planned");

        assert_eq!(plan.target_agent_id, "amp");
        assert_eq!(plan.config.mcp, expected_mcp);
        assert_eq!(plan.required_env_refs, ["AMP_API_KEY"]);
    }

    #[test]
    fn switch_to_kimi_without_provider_requires_explicit_provider() {
        let config = valid_config();
        let registry = RegistryCatalog::load_embedded().expect("registry loads");

        let error = plan_agent_switch_locked(
            &config,
            &registry,
            AgentSwitchRequest {
                target_agent: "kimi".to_owned(),
                provider_id: None,
                api_key_ref: None,
            },
        )
        .expect_err("provider-less switch to kimi must fail");

        assert!(
            error.to_string().contains("pass --provider"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn switch_to_kimi_reusing_moonshot_provider_swaps_credential_declaration() {
        let mut config = valid_config();
        config.agent.provider = Some(AgentProviderConfig {
            id: "moonshotai".to_owned(),
            model: Some("kimi-k3".to_owned()),
            api_key_ref: None,
            custom: None,
        });
        let registry = RegistryCatalog::load_embedded().expect("registry loads");

        let plan = plan_agent_switch_locked(
            &config,
            &registry,
            AgentSwitchRequest {
                target_agent: "kimi".to_owned(),
                provider_id: None,
                api_key_ref: None,
            },
        )
        .expect("switch planned");

        assert_eq!(plan.target_agent_id, "kimi");
        assert_eq!(plan.config.agent.env, ["MOONSHOT_API_KEY"]);
    }

    #[test]
    fn switch_to_kimi_with_moonshot_provider_swaps_credential_declaration() {
        let config = valid_config();
        let registry = RegistryCatalog::load_embedded().expect("registry loads");

        let plan = plan_agent_switch_locked(
            &config,
            &registry,
            AgentSwitchRequest {
                target_agent: "kimi".to_owned(),
                provider_id: Some("moonshotai".to_owned()),
                api_key_ref: None,
            },
        )
        .expect("switch planned");

        assert_eq!(plan.target_agent_id, "kimi");
        assert_eq!(plan.required_env_refs, ["MOONSHOT_API_KEY"]);
        assert_eq!(plan.config.agent.env, ["MOONSHOT_API_KEY"]);
        assert_eq!(
            plan.config
                .agent
                .provider
                .as_ref()
                .map(|provider| provider.id.as_str()),
            Some("moonshotai")
        );
    }

    #[test]
    fn switch_to_hermes_selects_provider_lane() {
        let mut config = valid_config();
        config.agent.provider = Some(AgentProviderConfig {
            id: "openrouter".to_owned(),
            model: Some("deepseek/deepseek-v4-flash".to_owned()),
            api_key_ref: Some("OPENROUTER_API_KEY".to_owned()),
            custom: None,
        });
        let registry = RegistryCatalog::load_embedded().expect("registry loads");

        let plan = plan_agent_switch_locked(
            &config,
            &registry,
            AgentSwitchRequest {
                target_agent: "hermes".to_owned(),
                provider_id: None,
                api_key_ref: None,
            },
        )
        .expect("switch planned");

        assert_eq!(plan.target_agent_id, "hermes");
        assert_eq!(
            plan.provider_status,
            AgentSwitchProviderStatus::Reused {
                provider_id: "openrouter".to_owned(),
                api_key_ref: Some("OPENROUTER_API_KEY".to_owned()),
            }
        );
        assert!(
            plan.config
                .agent
                .env
                .iter()
                .any(|entry| entry == "OPENROUTER_API_KEY")
        );
    }

    #[test]
    fn migrates_provider_secret_when_target_default_ref_differs() {
        let mut config = valid_config();
        config.agent.provider = Some(AgentProviderConfig {
            id: "cloudflare-ai-gateway".to_owned(),
            model: Some("cloudflare-ai-gateway/workers-ai/@cf/test".to_owned()),
            api_key_ref: Some("CLOUDFLARE_API_TOKEN".to_owned()),
            custom: None,
        });
        let registry = RegistryCatalog::load_embedded().expect("registry loads");

        let plan = plan_agent_switch_locked(
            &config,
            &registry,
            AgentSwitchRequest {
                target_agent: "pi".to_owned(),
                provider_id: None,
                api_key_ref: None,
            },
        )
        .expect("switch planned");

        assert_eq!(
            plan.provider_status,
            AgentSwitchProviderStatus::Reused {
                provider_id: "cloudflare-ai-gateway".to_owned(),
                api_key_ref: Some("CLOUDFLARE_API_KEY".to_owned()),
            }
        );
        assert_eq!(
            plan.secret_migrations,
            vec![AgentSwitchSecretMigration {
                from_ref: "CLOUDFLARE_API_TOKEN".to_owned(),
                to_ref: "CLOUDFLARE_API_KEY".to_owned(),
            }]
        );
    }

    #[test]
    fn explicit_same_provider_switch_migrates_target_default_ref() {
        let mut config = valid_config();
        config.agent.provider = Some(AgentProviderConfig {
            id: "cloudflare-ai-gateway".to_owned(),
            model: Some("cloudflare-ai-gateway/workers-ai/@cf/test".to_owned()),
            api_key_ref: Some("CLOUDFLARE_API_TOKEN".to_owned()),
            custom: None,
        });
        let registry = RegistryCatalog::load_embedded().expect("registry loads");

        let plan = plan_agent_switch_locked(
            &config,
            &registry,
            AgentSwitchRequest {
                target_agent: "pi".to_owned(),
                provider_id: Some("cloudflare-ai-gateway".to_owned()),
                api_key_ref: None,
            },
        )
        .expect("switch planned");

        assert_eq!(
            plan.provider_status,
            AgentSwitchProviderStatus::Set {
                provider_id: "cloudflare-ai-gateway".to_owned(),
                api_key_ref: Some("CLOUDFLARE_API_KEY".to_owned()),
            }
        );
        assert_eq!(
            plan.secret_migrations,
            vec![AgentSwitchSecretMigration {
                from_ref: "CLOUDFLARE_API_TOKEN".to_owned(),
                to_ref: "CLOUDFLARE_API_KEY".to_owned(),
            }]
        );
    }

    #[test]
    fn rejects_reuse_when_custom_ref_differs_from_target_default() {
        let mut config = valid_config();
        config.agent.provider = Some(AgentProviderConfig {
            id: "cloudflare-ai-gateway".to_owned(),
            model: Some("cloudflare-ai-gateway/workers-ai/@cf/test".to_owned()),
            api_key_ref: Some("MY_CLOUDFLARE_TOKEN".to_owned()),
            custom: None,
        });
        let registry = RegistryCatalog::load_embedded().expect("registry loads");

        let error = plan_agent_switch_locked(
            &config,
            &registry,
            AgentSwitchRequest {
                target_agent: "pi".to_owned(),
                provider_id: None,
                api_key_ref: None,
            },
        )
        .expect_err("custom ref should not be copied automatically");

        assert!(
            error
                .to_string()
                .contains("pass --provider cloudflare-ai-gateway --api-key-ref CLOUDFLARE_API_KEY")
        );
    }

    #[test]
    fn codex_openai_reuse_keeps_inherited_api_key_ref() {
        let mut config = valid_config();
        config.agent.provider = Some(AgentProviderConfig {
            id: "openai".to_owned(),
            model: Some("openai/gpt-5.5".to_owned()),
            api_key_ref: Some("OPENAI_API_KEY".to_owned()),
            custom: None,
        });
        let registry = RegistryCatalog::load_embedded().expect("registry loads");

        let plan = plan_agent_switch_locked(
            &config,
            &registry,
            AgentSwitchRequest {
                target_agent: "codex".to_owned(),
                provider_id: None,
                api_key_ref: None,
            },
        )
        .expect("switch planned");

        assert_eq!(
            plan.provider_status,
            AgentSwitchProviderStatus::Reused {
                provider_id: "openai".to_owned(),
                api_key_ref: Some("OPENAI_API_KEY".to_owned()),
            }
        );
        assert_eq!(
            plan.config
                .agent
                .provider
                .as_ref()
                .and_then(|provider| provider.api_key_ref.as_deref()),
            Some("OPENAI_API_KEY")
        );
        assert!(
            plan.config
                .agent
                .env
                .iter()
                .any(|env| env == "OPENAI_API_KEY")
        );
    }

    #[test]
    fn codex_openai_reuse_accepts_explicit_api_key_ref() {
        let mut config = valid_config();
        config.agent.provider = Some(AgentProviderConfig {
            id: "openai".to_owned(),
            model: Some("openai/gpt-5.5".to_owned()),
            api_key_ref: Some("OPENAI_API_KEY".to_owned()),
            custom: None,
        });
        let registry = RegistryCatalog::load_embedded().expect("registry loads");

        let plan = plan_agent_switch_locked(
            &config,
            &registry,
            AgentSwitchRequest {
                target_agent: "codex".to_owned(),
                provider_id: None,
                api_key_ref: Some("OPENAI_API_KEY".to_owned()),
            },
        )
        .expect("switch planned");

        assert_eq!(
            plan.provider_status,
            AgentSwitchProviderStatus::Reused {
                provider_id: "openai".to_owned(),
                api_key_ref: Some("OPENAI_API_KEY".to_owned()),
            }
        );
    }

    #[test]
    fn codex_openrouter_reuse_keeps_api_key_ref() {
        let mut config = valid_config();
        config.agent.provider = Some(AgentProviderConfig {
            id: "openrouter".to_owned(),
            model: Some("openrouter/deepseek/deepseek-v4-flash".to_owned()),
            api_key_ref: Some("OPENROUTER_API_KEY".to_owned()),
            custom: None,
        });
        let registry = RegistryCatalog::load_embedded().expect("registry loads");

        let plan = plan_agent_switch_locked(
            &config,
            &registry,
            AgentSwitchRequest {
                target_agent: "codex".to_owned(),
                provider_id: None,
                api_key_ref: None,
            },
        )
        .expect("switch planned");

        assert_eq!(
            plan.provider_status,
            AgentSwitchProviderStatus::Reused {
                provider_id: "openrouter".to_owned(),
                api_key_ref: Some("OPENROUTER_API_KEY".to_owned()),
            }
        );
        assert!(
            plan.config
                .agent
                .env
                .iter()
                .any(|env| env == "OPENROUTER_API_KEY")
        );
    }

    #[test]
    fn rejects_custom_provider_migration() {
        let mut config = valid_config();
        config.agent.provider = Some(AgentProviderConfig {
            id: "myprovider".to_owned(),
            model: Some("my-model".to_owned()),
            api_key_ref: Some("CUSTOM_API_KEY".to_owned()),
            custom: Some(crate::config::AgentCustomProviderConfig {
                name: "Custom".to_owned(),
                base_url: "https://example.com/v1".to_owned(),
                api: crate::config::CustomProviderApi::ChatCompletions,
                model_name: Some("Custom Model".to_owned()),
                context: crate::config::DEFAULT_CUSTOM_MODEL_CONTEXT,
                output_max_tokens: crate::config::DEFAULT_CUSTOM_MODEL_OUTPUT_MAX_TOKENS,
            }),
        });
        let registry = RegistryCatalog::load_embedded().expect("registry loads");

        let error = plan_agent_switch_locked(
            &config,
            &registry,
            AgentSwitchRequest {
                target_agent: "pi".to_owned(),
                provider_id: None,
                api_key_ref: None,
            },
        )
        .expect_err("custom provider migration is out of scope");

        assert!(error.to_string().contains("custom provider migration"));
    }

    // Serializes the tests that stage an override through `HomeEnvGuard`: that
    // guard mutates the process `HOME`, which is global, so the mutation must
    // not race another test in this module.
    static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        HOME_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// An empty tempdir as HOME: these cases stage no override, so the guard is a
    /// no-op and their outcome cannot depend on the developer's real secret store.
    fn plan_agent_switch_locked(
        current: &Config,
        registry: &RegistryCatalog,
        request: AgentSwitchRequest,
    ) -> Result<AgentSwitchPlan> {
        let _env_lock = env_lock();
        let tempdir = tempfile::tempdir().expect("tempdir");
        super::plan_agent_switch(tempdir.path(), current, registry, request)
    }

    struct HomeEnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        previous: Option<std::ffi::OsString>,
    }

    impl HomeEnvGuard {
        fn set(home: &std::path::Path) -> Self {
            let lock = env_lock();
            let previous = std::env::var_os("HOME");
            // SAFETY: HOME_LOCK serializes the HOME mutation against every
            // other test in this module that touches the override store.
            unsafe {
                std::env::set_var("HOME", home);
            }
            Self {
                _lock: lock,
                previous,
            }
        }
    }

    impl Drop for HomeEnvGuard {
        fn drop(&mut self) {
            // SAFETY: the lock is still held, so restoring the prior HOME
            // cannot race another test in this module.
            unsafe {
                match self.previous.take() {
                    Some(value) => std::env::set_var("HOME", value),
                    None => std::env::remove_var("HOME"),
                }
            }
        }
    }

    /// Stage an externally-owned credential carrying an endpoint override.
    fn stage_endpoint_override(home: &std::path::Path, provider_id: &str) {
        let mut store = crate::secrets::SecretStore::open_or_create(home).expect("secret store");
        store
            .apply_managed_state_credential(
                "platform-state",
                "provider-credential",
                1,
                Some(crate::secrets::ManagedCredentialSelection {
                    provider_id: provider_id.to_owned(),
                    values: BTreeMap::from([("TEST_API_KEY".to_owned(), "sk-test".to_owned())]),
                    source_refs: BTreeMap::new(),
                    base_url: Some("http://127.0.0.1:3129".to_owned()),
                }),
            )
            .expect("stage override");
    }

    #[test]
    fn override_guard_rejects_target_without_base_url_support() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let _home = HomeEnvGuard::set(tempdir.path());
        stage_endpoint_override(tempdir.path(), "openai");

        let error = ensure_endpoint_override_survives_target(tempdir.path(), "kimi", false, None)
            .expect_err("a target with no endpoint field must be rejected");
        assert!(
            matches!(error, StackError::InvalidParam { field: "agent", .. }),
            "{error}"
        );
        let message = error.to_string();
        assert!(
            message.contains("agent `kimi` cannot route a provider through a custom endpoint"),
            "{message}"
        );
        assert!(message.contains("openai"), "{message}");
    }

    #[test]
    fn override_guard_checks_the_credential_pair_for_agents_that_follow_it() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let _home = HomeEnvGuard::set(tempdir.path());
        // kilo does not run `openai`, and it provisions whatever provider the override names.
        stage_endpoint_override(tempdir.path(), "openai");

        for agent_id in ["kilo", "antigravity"] {
            let error =
                ensure_endpoint_override_survives_target(tempdir.path(), agent_id, true, None)
                    .expect_err("an override the target cannot map must be rejected at plan time");
            let message = error.to_string();
            assert!(
                message.contains(&format!(
                    "agent `{agent_id}` cannot route provider `openai` through a custom endpoint"
                )),
                "{message}"
            );
        }
        ensure_endpoint_override_survives_target(tempdir.path(), "opencode", true, None)
            .expect("a provider-selecting agent is gated on its configured provider");
    }

    #[test]
    fn override_guard_passes_without_a_stored_override() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let _home = HomeEnvGuard::set(tempdir.path());

        ensure_endpoint_override_survives_target(tempdir.path(), "kimi", false, None)
            .expect("no override stored means any target passes");
    }

    #[test]
    fn plan_switch_rejects_codex_openai_pair_with_fresh_provider() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let _home = HomeEnvGuard::set(tempdir.path());
        stage_endpoint_override(tempdir.path(), "openai");
        let mut config = valid_config();
        config.agent.provider = Some(AgentProviderConfig {
            id: "openai".to_owned(),
            model: Some("openai/gpt-5.5".to_owned()),
            api_key_ref: Some("OPENAI_API_KEY".to_owned()),
            custom: None,
        });
        let registry = RegistryCatalog::load_embedded().expect("registry loads");

        // `super::` because HOME_LOCK is already held and must not re-acquire.
        let error = super::plan_agent_switch(
            tempdir.path(),
            &config,
            &registry,
            AgentSwitchRequest {
                target_agent: "codex".to_owned(),
                provider_id: Some("openai".to_owned()),
                api_key_ref: None,
            },
        )
        .expect_err("codex + openai cannot carry the override");
        let message = error.to_string();
        assert!(
            message
                .contains("agent `codex` cannot route provider `openai` through a custom endpoint"),
            "{message}"
        );
    }

    #[test]
    fn plan_switch_rejects_codex_openai_pair_with_reused_provider() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let _home = HomeEnvGuard::set(tempdir.path());
        stage_endpoint_override(tempdir.path(), "openai");
        let mut config = valid_config();
        config.agent.provider = Some(AgentProviderConfig {
            id: "openai".to_owned(),
            model: Some("openai/gpt-5.5".to_owned()),
            api_key_ref: Some("OPENAI_API_KEY".to_owned()),
            custom: None,
        });
        let registry = RegistryCatalog::load_embedded().expect("registry loads");

        let error = super::plan_agent_switch(
            tempdir.path(),
            &config,
            &registry,
            AgentSwitchRequest {
                target_agent: "codex".to_owned(),
                provider_id: None,
                api_key_ref: None,
            },
        )
        .expect_err("codex + openai cannot carry the override");
        assert!(
            error
                .to_string()
                .contains("agent `codex` cannot route provider `openai` through a custom endpoint"),
            "{error}"
        );
    }

    #[test]
    fn plan_switch_allows_codex_openrouter_with_openrouter_override() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let _home = HomeEnvGuard::set(tempdir.path());
        stage_endpoint_override(tempdir.path(), "openrouter");
        let mut config = valid_config();
        config.agent.provider = Some(AgentProviderConfig {
            id: "openrouter".to_owned(),
            model: Some("openrouter/deepseek/deepseek-v4-flash".to_owned()),
            api_key_ref: Some("OPENROUTER_API_KEY".to_owned()),
            custom: None,
        });
        let registry = RegistryCatalog::load_embedded().expect("registry loads");

        let plan = super::plan_agent_switch(
            tempdir.path(),
            &config,
            &registry,
            AgentSwitchRequest {
                target_agent: "codex".to_owned(),
                provider_id: None,
                api_key_ref: None,
            },
        )
        .expect("codex + openrouter accepts an override");
        assert_eq!(plan.target_agent_id, "codex");
    }
}
