use crate::config::{AgentAdapterConfig, AgentProviderConfig, Config};
use crate::error::{Result, StackError};
use crate::runtime::agent::provider_keys::{
    api_key_ref_can_migrate_for_provider, env_refs_for_agent_id, env_var_for_agent_provider_id,
    provider_id_is_known, provider_id_supports_agent, provider_uses_agent_native_auth,
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

pub fn plan_agent_switch(
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
    config.agent.provider = None;
    config.agent.subagent = None;
    config.agent.expected_sha256 = None;
    config.agent.restart = "on-crash".to_owned();
    config.agent.harness_version = None;
    config.agent.adapter = adapter_from_registry_entry(entry);
    config.agent.install = None;

    match entry.kind {
        RegistryKind::Native => {
            let harness = entry.harness.as_ref().expect("validated registry harness");
            config.agent.command = harness.id.clone();
            config.agent.args = vec!["acp".to_owned()];
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
            Vec::new(),
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
    let api_key_ref_was_explicit = request.api_key_ref.is_some();
    let explicit_api_key_ref = request.api_key_ref;
    let inherited_api_key_ref = current_provider.api_key_ref.clone();
    let current_api_key_ref = match (
        explicit_api_key_ref,
        entry.id.as_str(),
        current_provider.id.as_str(),
    ) {
        (None, "codex", "openai") => None,
        (requested, _, _) => requested.or(inherited_api_key_ref.clone()),
    };
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

    if target_agent_id == "codex" && provider_id == "openai" {
        if requested_api_key_ref.is_some() {
            return Err(StackError::InvalidParam {
                field: "api-key-ref",
                reason: "Codex OpenAI uses Codex-native auth; do not pass --api-key-ref".to_owned(),
            });
        }
        return Ok((
            AgentProviderConfig {
                id: provider_id,
                model: None,
                api_key_ref: None,
                custom: None,
            },
            Vec::new(),
            Vec::new(),
        ));
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
        if !env.iter().any(|name| name == env_ref) {
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
                    headers: vec![HttpHeaderRef {
                        name: "Authorization".to_owned(),
                        value_ref: "LINEAR_API_KEY".to_owned(),
                    }],
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

        let plan = plan_agent_switch(
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
    fn switch_preserves_mcp_runtime_config() {
        let mut config = valid_config();
        let expected_mcp = mcp_config();
        config.mcp = expected_mcp.clone();
        let registry = RegistryCatalog::load_embedded().expect("registry loads");

        let plan = plan_agent_switch(
            &config,
            &registry,
            AgentSwitchRequest {
                target_agent: "cursor".to_owned(),
                provider_id: None,
                api_key_ref: None,
            },
        )
        .expect("switch planned");

        assert_eq!(plan.target_agent_id, "cursor");
        assert_eq!(plan.config.mcp, expected_mcp);
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

        let plan = plan_agent_switch(
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

        let plan = plan_agent_switch(
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

        let error = plan_agent_switch(
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
    fn codex_openai_reuse_drops_inherited_api_key_ref() {
        let mut config = valid_config();
        config.agent.provider = Some(AgentProviderConfig {
            id: "openai".to_owned(),
            model: Some("openai/gpt-5.5".to_owned()),
            api_key_ref: Some("OPENAI_API_KEY".to_owned()),
            custom: None,
        });
        let registry = RegistryCatalog::load_embedded().expect("registry loads");

        let plan = plan_agent_switch(
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
                api_key_ref: None,
            }
        );
        assert_eq!(
            plan.config
                .agent
                .provider
                .as_ref()
                .and_then(|provider| provider.api_key_ref.as_ref()),
            None
        );
        assert!(
            !plan
                .config
                .agent
                .env
                .iter()
                .any(|env| env == "OPENAI_API_KEY")
        );
    }

    #[test]
    fn codex_openai_reuse_rejects_explicit_api_key_ref() {
        let mut config = valid_config();
        config.agent.provider = Some(AgentProviderConfig {
            id: "openai".to_owned(),
            model: Some("openai/gpt-5.5".to_owned()),
            api_key_ref: Some("OPENAI_API_KEY".to_owned()),
            custom: None,
        });
        let registry = RegistryCatalog::load_embedded().expect("registry loads");

        let error = plan_agent_switch(
            &config,
            &registry,
            AgentSwitchRequest {
                target_agent: "codex".to_owned(),
                provider_id: None,
                api_key_ref: Some("OPENAI_API_KEY".to_owned()),
            },
        )
        .expect_err("explicit key should be rejected for Codex OpenAI");

        assert!(
            error
                .to_string()
                .contains("Codex OpenAI uses Codex-native auth")
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

        let plan = plan_agent_switch(
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

        let error = plan_agent_switch(
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
}
