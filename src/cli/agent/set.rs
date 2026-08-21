use std::path::{Path, PathBuf};

use crate::config::{
    self, AgentConfig, AgentCustomProviderConfig, AgentProviderConfig, Config, CustomProviderApi,
    DEFAULT_CUSTOM_MODEL_CONTEXT, DEFAULT_CUSTOM_MODEL_OUTPUT_MAX_TOKENS,
};
use crate::error::{Result, StackError};
use crate::fs_util::{acquire_agent_config_mutation_file_lock, atomic_write_owner_only, home_dir};
use crate::runtime::agent::acp_bridge::{
    AgentSessionConfigCategory, session_config_id_for_value, session_model_selection_for_value,
};
use crate::runtime::agent::agent_headless_config::provision_agent_headless_config_transition;
use crate::runtime::agent::model_discovery::{
    fetch_session_config, model_value_is_explicit_without_discovery, resolve_advertised_model_value,
};
use crate::runtime::agent::provider_keys::{
    CLAUDE_CODE_AGENT_ID, KILO_AGENT_ID, agent_provider_id_for_provider_id,
    claude_code_profile_for_provider_id, env_refs_for_agent_id, env_var_for_agent_provider_id,
    provider_ids_for_env_refs, required_env_refs_for_agent_provider_id,
};
use crate::runtime::agent::provider_model_catalog::refresh_provider_models_best_effort_blocking;
use crate::runtime::install::agent_registry::{RegistryCatalog, RegistryEntry};
use crate::secrets::SecretStore;

use super::AgentSetArgs;
use super::install::operator_registry_override;

pub(super) fn run_agent_set(args: AgentSetArgs) -> Result<()> {
    let home = home_dir()?;
    let config_path = config::default_config_path()?;
    let _mutation = acquire_agent_config_mutation_file_lock(&config_path)?;
    let config = Config::load_from_path(&config_path)?;
    let registry = RegistryCatalog::load_with_override(&operator_registry_override(&home))?;
    let entry = registry.lookup_required(&config.agent.id)?;
    if let Some(mode) = args.mode.clone() {
        return run_agent_mode_set(config, config_path, &home, args, entry, mode);
    }
    let Some(provider_id) = args.provider.clone() else {
        return run_agent_model_set(config, config_path, &home, args, entry);
    };
    if args.custom_provider {
        return run_agent_custom_provider_set(config, config_path, &home, args, entry, provider_id);
    }
    Err(StackError::InvalidParam {
        field: "provider",
        reason: "mapped providers are selected with `acps agent provider use <provider>`"
            .to_owned(),
    })
}
fn run_agent_custom_provider_set(
    mut config: Config,
    config_path: PathBuf,
    home: &Path,
    args: AgentSetArgs,
    entry: &RegistryEntry,
    provider_id: String,
) -> Result<()> {
    let previous_config = config.clone();
    if !entry.allow_custom_provider {
        return Err(StackError::InvalidParam {
            field: "custom-provider",
            reason: format!("{} does not support custom provider setup", entry.name),
        });
    }
    if !entry.allow_custom_model {
        return Err(StackError::InvalidParam {
            field: "custom-provider",
            reason: format!("{} does not support custom model setup", entry.name),
        });
    }
    let provider_name = required_custom_arg("provider-name", args.provider_name)?;
    let base_url = required_custom_arg("base-url", args.base_url)?;
    let api_key_ref = required_custom_arg("api-key-ref", args.api_key_ref)?;
    let model = required_custom_arg("model", args.model)?;
    let model_name = args.model_name.unwrap_or_else(|| model.clone());
    let api = parse_custom_provider_api(
        args.provider_api.as_deref(),
        default_custom_provider_api(&config.agent.id),
    )?;
    validate_custom_provider_api_for_agent(&config.agent.id, api, "provider-api")?;
    let context = parse_custom_token_limit(
        "context",
        args.context.as_deref(),
        DEFAULT_CUSTOM_MODEL_CONTEXT,
    )?;
    let output_max_tokens = parse_custom_token_limit(
        "output-max-tokens",
        args.output_max_tokens.as_deref(),
        DEFAULT_CUSTOM_MODEL_OUTPUT_MAX_TOKENS,
    )?;

    if !crate::config::agent_env_declares(&config.agent.env, &api_key_ref) {
        config.agent.env.push(api_key_ref.clone());
    }
    config.agent.model = None;
    config.agent.provider = Some(AgentProviderConfig {
        id: provider_id,
        model: Some(model),
        api_key_ref: Some(api_key_ref.clone()),
        custom: Some(AgentCustomProviderConfig {
            name: provider_name,
            base_url,
            api,
            model_name: Some(model_name),
            context,
            output_max_tokens,
        }),
    });
    config.agent.providers = None;

    let canonical = config.to_canonical_toml()?;
    let config = config::load_config_from_str(&canonical)?;
    let provisioned = provision_agent_headless_config_transition(&previous_config, &config, home)?;
    atomic_write_owner_only(&config_path, canonical.as_bytes())?;

    print_agent_set_agent(&config);
    println!(
        "provider: {}",
        config.agent.provider.as_ref().expect("provider set").id
    );
    println!(
        "model: {}",
        config
            .agent
            .provider
            .as_ref()
            .and_then(|provider| provider.model.as_deref())
            .unwrap_or("")
    );
    println!("api_key_ref: {api_key_ref}");
    for item in provisioned {
        println!("{}: {}", item.label, item.path.display());
    }
    print_agent_set_effective_notice_for(Some(&config.agent.id));
    Ok(())
}

pub(in crate::cli) fn required_custom_arg(
    field: &'static str,
    value: Option<String>,
) -> Result<String> {
    value
        .filter(|value| !value.trim().is_empty() && value.trim().len() == value.len())
        .ok_or_else(|| StackError::InvalidParam {
            field,
            reason: format!("--{field} is required for --custom-provider"),
        })
}

pub(in crate::cli) fn default_custom_provider_api(agent_id: &str) -> CustomProviderApi {
    if agent_id == "codex" {
        CustomProviderApi::Responses
    } else if agent_id == CLAUDE_CODE_AGENT_ID {
        CustomProviderApi::AnthropicMessages
    } else {
        CustomProviderApi::ChatCompletions
    }
}

pub(in crate::cli) fn parse_custom_provider_api(
    value: Option<&str>,
    default: CustomProviderApi,
) -> Result<CustomProviderApi> {
    match value {
        None => Ok(default),
        Some("chat-completions") => Ok(CustomProviderApi::ChatCompletions),
        Some("responses") => Ok(CustomProviderApi::Responses),
        Some("anthropic-messages") => Ok(CustomProviderApi::AnthropicMessages),
        Some(_) => Err(StackError::InvalidParam {
            field: "provider-api",
            reason: "must be `chat-completions`, `responses`, or `anthropic-messages`".to_owned(),
        }),
    }
}

pub(in crate::cli) fn validate_custom_provider_api_for_agent(
    agent_id: &str,
    api: CustomProviderApi,
    field: &'static str,
) -> Result<()> {
    if agent_id == "codex" && api != CustomProviderApi::Responses {
        return Err(StackError::InvalidParam {
            field,
            reason: "Codex custom providers only support responses".to_owned(),
        });
    }
    if agent_id == CLAUDE_CODE_AGENT_ID && api != CustomProviderApi::AnthropicMessages {
        return Err(StackError::InvalidParam {
            field,
            reason: "Claude Code custom providers only support anthropic-messages".to_owned(),
        });
    }
    if agent_id != CLAUDE_CODE_AGENT_ID && api == CustomProviderApi::AnthropicMessages {
        return Err(StackError::InvalidParam {
            field,
            reason: "anthropic-messages custom providers only support Claude Code".to_owned(),
        });
    }
    Ok(())
}

pub(in crate::cli) fn parse_custom_token_limit(
    field: &'static str,
    value: Option<&str>,
    default: u64,
) -> Result<u64> {
    let Some(value) = value else {
        return Ok(default);
    };
    if value.contains(',') {
        return Err(StackError::InvalidParam {
            field,
            reason: "must be a plain integer without commas".to_owned(),
        });
    }
    let parsed = value.parse::<u64>().map_err(|_| StackError::InvalidParam {
        field,
        reason: "must be a positive integer".to_owned(),
    })?;
    if parsed == 0 {
        return Err(StackError::InvalidParam {
            field,
            reason: "must be greater than 0".to_owned(),
        });
    }
    Ok(parsed)
}

fn reject_custom_provider_args(args: &AgentSetArgs) -> Result<()> {
    if args.custom_provider
        || args.provider_name.is_some()
        || args.base_url.is_some()
        || args.provider_api.is_some()
        || args.model_name.is_some()
        || args.context.is_some()
        || args.output_max_tokens.is_some()
    {
        return Err(StackError::InvalidParam {
            field: "custom-provider",
            reason: "custom provider flags require --custom-provider".to_owned(),
        });
    }
    Ok(())
}

/// Env refs the model-set preflight seeds into `[agent].env` before spawning
/// the provisional discovery session, so the session can resolve its
/// credentials. A mapped/custom provider derives them from the provider
/// mapping. A `set_provider = false` agent (e.g. kilo) always seeds the
/// mapped default key, even when `[agent].env` declares a provider-native
/// credential like `OPENROUTER_API_KEY`: Kilo requires `KILO_API_KEY` present
/// in the process env regardless of the active provider, so an unseeded key
/// would break the session. When such a credential is declared, the preflight
/// records an empty placeholder for the seeded key (see
/// `record_empty_key_placeholders_for_provider_native_env`); otherwise a
/// missing secret surfaces as a clear "secret not found" error from the
/// discovery session instead of a session that stalls until the stale-prompt
/// sweeper.
fn model_set_required_env_refs(agent: &AgentConfig) -> Vec<String> {
    if let Some(provider) = agent.provider.as_ref() {
        provider
            .api_key_ref
            .as_deref()
            .map(|api_key_ref| {
                required_env_refs_for_agent_provider_id(&agent.id, &provider.id, Some(api_key_ref))
            })
            .unwrap_or_default()
    } else {
        env_refs_for_agent_id(&agent.id)
            .into_iter()
            .map(str::to_owned)
            .collect()
    }
}

/// Records an empty placeholder for each mapped key the operator never
/// stored, when `[agent].env` declares a recognized provider-native
/// credential. That declaration shows the operator authenticates through the
/// provider's own key, but Kilo requires its `KILO_API_KEY` variable present
/// in the process env regardless of the active provider, so the mapped ref
/// must still resolve. Recording the placeholder at init (and at `agent set
/// --model`, for credentials declared after init) spares the operator a
/// separate `acps secrets set <REF> --value ""`. When no provider-native
/// credential is declared, missing refs are left alone so a genuinely absent
/// key surfaces as a clear "secret not found" error. Mapped providers are
/// excluded: they demand their real key as before. Kilo-only: it is the one
/// agent verified to require the var present while accepting an empty value
/// (kimi rejects an empty key at launch; others are unverified). At the
/// `agent set --model` call site recording happens before model validation,
/// so a failed model set can leave the placeholder behind — harmless (it only
/// materializes an empty var) and visible in `acps secrets list`. Returns the
/// ref names recorded, for the operator notice.
pub(in crate::cli) fn record_empty_key_placeholders_for_provider_native_env(
    store: &mut SecretStore,
    agent: &AgentConfig,
) -> Result<Vec<String>> {
    if agent.provider.is_some() || agent.id != KILO_AGENT_ID {
        return Ok(Vec::new());
    }
    let mapped_refs: Vec<String> = env_refs_for_agent_id(&agent.id)
        .into_iter()
        .map(str::to_owned)
        .collect();
    // Judge intent from what the operator declared, excluding the mapped refs
    // themselves: the mapped key is a recognized credential too, so counting
    // it would pass the gate even with no provider-native credential present.
    // Compare by var name so a templated declaration (`OPENROUTER_API_KEY=${X}`)
    // is recognized as well as a bare one.
    if mapped_refs.is_empty()
        || provider_ids_for_env_refs(
            agent
                .env
                .iter()
                .filter(|entry| {
                    !mapped_refs
                        .iter()
                        .any(|mapped| crate::config::env_entry_var_name(entry) == *mapped)
                })
                .map(|entry| crate::config::env_entry_var_name(entry)),
        )
        .is_empty()
    {
        return Ok(Vec::new());
    }
    let missing: Vec<String> = mapped_refs
        .into_iter()
        .filter(|env_ref| !store.contains(env_ref))
        .collect();
    if missing.is_empty() {
        return Ok(Vec::new());
    }
    store.set_many(missing.iter().map(|name| (name.as_str(), "")))?;
    Ok(missing)
}

/// Seeds the agent's mapped key declarations into `[agent].env` when missing.
/// `agent set --model` does this for every `set_provider = false` agent; the
/// init and config-import paths call it for kilo only, the one agent verified
/// to require the variable present, so an imported or hand-edited kilo config
/// that omits the declaration is repaired instead of silently spawning the
/// harness without a variable it requires. Returns whether env changed.
pub(in crate::cli) fn seed_kilo_mapped_key_env_declaration(agent: &mut AgentConfig) -> bool {
    if agent.provider.is_some() || agent.id != KILO_AGENT_ID {
        return false;
    }
    let mut seeded = false;
    for env_ref in env_refs_for_agent_id(&agent.id) {
        if !crate::config::agent_env_declares(&agent.env, env_ref) {
            agent.env.push(env_ref.to_owned());
            seeded = true;
        }
    }
    seeded
}

fn run_agent_model_set(
    mut config: Config,
    config_path: PathBuf,
    home: &Path,
    args: AgentSetArgs,
    entry: &RegistryEntry,
) -> Result<()> {
    let previous_config = config.clone();
    reject_custom_provider_args(&args)?;
    if args.api_key_ref.is_some() {
        return Err(StackError::InvalidParam {
            field: "api-key-ref",
            reason: "--api-key-ref requires --custom-provider".to_owned(),
        });
    }
    if !entry.set_model {
        return Err(StackError::AgentConfigProvision {
            path: config_path,
            reason: format!(
                "{} does not support model configuration through `acps agent set`",
                entry.name
            ),
        });
    }
    if entry.set_provider && config.agent.provider.is_none() {
        return Err(StackError::InvalidParam {
            field: "provider",
            reason: format!(
                "select a mapped provider with `acps agent provider use` before setting a model for {}",
                entry.name
            ),
        });
    }
    let Some(model) = args.model else {
        return Err(StackError::InvalidParam {
            field: "model",
            reason: "pass --model <model-id>, --mode <mode>, or --custom-provider".to_owned(),
        });
    };

    let required_env_refs = model_set_required_env_refs(&config.agent);
    for env_ref in &required_env_refs {
        if !crate::config::agent_env_declares(&config.agent.env, env_ref) {
            config.agent.env.push(env_ref.clone());
        }
    }
    // Placeholder recording only applies to `set_provider = false` agents;
    // skip opening the store for mapped providers.
    let recorded_placeholders = if config.agent.provider.is_some() {
        Vec::new()
    } else {
        let mut store = SecretStore::open(home)?;
        record_empty_key_placeholders_for_provider_native_env(&mut store, &config.agent)?
    };
    let agent_provider_id = config
        .agent
        .provider
        .as_ref()
        .and_then(|provider| agent_provider_id_for_provider_id(&config.agent.id, &provider.id));
    let model = resolve_agent_model_value(home, &config, agent_provider_id, &model)?;
    if let Some(provider) = config.agent.provider.as_mut() {
        provider.model = Some(model);
        config.agent.model = None;
    } else {
        config.agent.model = Some(model);
    }

    let canonical = config.to_canonical_toml()?;
    let config = config::load_config_from_str(&canonical)?;
    let model_value = config
        .agent
        .provider
        .as_ref()
        .and_then(|provider| provider.model.as_deref())
        .or(config.agent.model.as_deref())
        .expect("agent model set");
    validate_agent_model_if_required(home, &config, model_value)?;
    refresh_provider_models_best_effort_blocking(home, &config);
    let provisioned = provision_agent_headless_config_transition(&previous_config, &config, home)?;
    atomic_write_owner_only(&config_path, canonical.as_bytes())?;

    print_agent_set_agent(&config);
    if let Some(provider) = config.agent.provider.as_ref() {
        println!("provider: {}", provider.id);
    }
    println!("model: {model_value}");
    // Print the refs the operator must have resolvable. For a mapped provider
    // that is the provider's required set; for a `set_provider = false` agent it
    // is the effective `[agent].env` (whatever the seed added or the operator
    // declared, e.g. a provider-native ref), matching `agent switch`.
    let displayed_env_refs = if config.agent.provider.is_some() {
        required_env_refs
    } else {
        config.agent.env.clone()
    };
    if !displayed_env_refs.is_empty() {
        println!("required_env_refs: {}", displayed_env_refs.join(", "));
    }
    for placeholder in &recorded_placeholders {
        println!(
            "recorded empty {placeholder} placeholder: the harness requires the variable \
             present; authentication uses the declared provider-native credential"
        );
    }
    for item in provisioned {
        println!("{}: {}", item.label, item.path.display());
    }
    print_agent_set_effective_notice_for(Some(&config.agent.id));
    Ok(())
}

fn run_agent_mode_set(
    mut config: Config,
    config_path: PathBuf,
    home: &Path,
    args: AgentSetArgs,
    entry: &RegistryEntry,
    mode: String,
) -> Result<()> {
    reject_custom_provider_args(&args)?;
    if args.provider.is_some() || args.model.is_some() || args.api_key_ref.is_some() {
        return Err(StackError::InvalidParam {
            field: "mode",
            reason: "--mode cannot be combined with --provider, --model, or --api-key-ref"
                .to_owned(),
        });
    }
    if !entry.set_mode {
        return Err(StackError::AgentConfigProvision {
            path: config_path,
            reason: format!(
                "{} does not support mode configuration through `acps agent set`",
                entry.name
            ),
        });
    }
    config.agent.mode = Some(mode);
    let canonical = config.to_canonical_toml()?;
    let config = config::load_config_from_str(&canonical)?;
    let mode = config.agent.mode.as_deref().expect("mode set");
    validate_agent_session_config_value(home, &config, AgentSessionConfigCategory::Mode, mode)?;
    atomic_write_owner_only(&config_path, canonical.as_bytes())?;
    print_agent_set_agent(&config);
    println!("mode: {mode}");
    print_agent_set_effective_notice_for(Some(&config.agent.id));
    Ok(())
}

fn print_agent_set_agent(config: &Config) {
    println!("agent: {}", config.agent.id);
}

/// Effective-notice variant aware of the configured agent. Most agents
/// read provider/model from their on-disk config at process start, so a
/// running agent must be restarted through `POST /v1/agent/restart`
/// before the new settings take effect. Goose is the exception: clients
/// can switch model live via ACP `session/set_config_option`. When
/// `agent_id` is provided we surface the correct guidance; passing
/// `None` keeps the generic "new sessions" message for paths where the
/// agent id is not known to the caller.
/// Whether a provider/credential/model change needs a supervised-agent process
/// restart to take effect. Goose reloads model changes live and applies other
/// changes on the next ACP session via `session/set_config_option`, so no
/// process restart is required. Cline and Kilo also apply model/mode over ACP
/// rather than from disk, but their credentials are env vars materialized at
/// process spawn, so a credential change still needs a restart. Every other
/// harness reads provider/model from disk at process start. Keeps the
/// machine-readable `restart_required` JSON field consistent with the
/// human-facing effective notice.
pub(in crate::cli) fn provider_change_requires_restart(agent_id: &str) -> bool {
    agent_id != "goose"
}

pub(in crate::cli) fn print_agent_set_effective_notice_for(agent_id: Option<&str>) {
    match agent_id {
        Some("goose") => {
            println!(
                "model can be switched live via ACP session/set_config_option; \
                 other changes apply to new sessions"
            );
        }
        // Cline and Kilo apply model/mode over ACP, so there is no on-disk lane
        // to reload; a restart only re-materializes their env-var credentials.
        Some("cline") | Some("kilo") => {
            println!(
                "model and mode take effect on new sessions via ACP \
                 session/set_config_option; restart the supervised agent \
                 (`POST /v1/agent/restart`) to apply credential changes"
            );
        }
        Some(_) => {
            println!(
                "settings take effect on new sessions; restart the supervised \
                 agent (`POST /v1/agent/restart`) to reload from disk"
            );
        }
        None => println!("settings will take effect on new sessions"),
    }
}

pub(in crate::cli) fn default_api_key_ref_for_agent_provider(
    agent_id: &str,
    provider_id: &str,
) -> Option<String> {
    env_var_for_agent_provider_id(agent_id, provider_id).map(str::to_owned)
}

pub(in crate::cli) fn resolve_agent_model_value(
    home: &Path,
    config: &Config,
    provider_id: Option<&str>,
    model_id: &str,
) -> Result<String> {
    if agent_model_is_explicit_without_discovery(config) {
        return Ok(model_id.to_owned());
    }
    let response = read_agent_new_session_response(home, config)?;
    resolve_advertised_model_value(&response, provider_id, model_id)
}

fn validate_agent_model_if_required(home: &Path, config: &Config, model_value: &str) -> Result<()> {
    if agent_model_is_explicit_without_discovery(config) {
        return Ok(());
    }
    validate_agent_session_config_value(
        home,
        config,
        AgentSessionConfigCategory::Model,
        model_value,
    )
}

pub(in crate::cli) fn agent_model_is_explicit_without_discovery(config: &Config) -> bool {
    model_value_is_explicit_without_discovery(&config.agent)
}

pub(in crate::cli) fn model_values_for_cli_display(
    config: &Config,
    values: Vec<String>,
) -> Vec<String> {
    let Some(default_model) = claude_code_profile_default_model(config) else {
        return values;
    };
    let mut filtered = Vec::new();
    for value in values {
        if is_claude_code_builtin_model_alias(&value) {
            continue;
        }
        if !filtered.iter().any(|existing| existing == &value) {
            filtered.push(value);
        }
    }
    if !filtered.iter().any(|value| value == default_model) {
        filtered.insert(0, default_model.to_owned());
    }
    filtered
}

fn claude_code_profile_default_model(config: &Config) -> Option<&'static str> {
    if config.agent.id != CLAUDE_CODE_AGENT_ID {
        return None;
    }
    config
        .agent
        .provider
        .as_ref()
        .and_then(|provider| claude_code_profile_for_provider_id(&provider.id))
        .and_then(|profile| profile.default_model.as_deref())
        .filter(|model| !model.trim().is_empty())
}

fn is_claude_code_builtin_model_alias(value: &str) -> bool {
    matches!(
        value.trim(),
        "best" | "default" | "fable" | "opus" | "sonnet" | "haiku"
    )
}

pub(in crate::cli) fn validate_agent_session_config_value(
    home: &Path,
    config: &Config,
    category: AgentSessionConfigCategory,
    value: &str,
) -> Result<()> {
    let response = read_agent_new_session_response(home, config)?;
    match category {
        AgentSessionConfigCategory::Model => {
            session_model_selection_for_value(&response, value).map(|_| ())
        }
        AgentSessionConfigCategory::Mode => {
            session_config_id_for_value(response.config_options.as_deref(), category, value)
                .map(|_| ())
        }
    }
}

fn read_agent_new_session_response(
    home: &Path,
    config: &Config,
) -> Result<agent_client_protocol::schema::v1::NewSessionResponse> {
    fetch_session_config(home, config)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(id: &str, env: &[&str]) -> AgentConfig {
        AgentConfig {
            id: id.to_owned(),
            name: id.to_owned(),
            command: id.to_owned(),
            args: Vec::new(),
            cwd: None,
            env: env.iter().map(|value| (*value).to_owned()).collect(),
            expected_sha256: None,
            restart: "on-crash".to_owned(),
            mode: None,
            model: None,
            harness_version: None,
            adapter: None,
            install: None,
            provider: None,
            providers: None,
            subagent: None,
            auto_update: None,
        }
    }

    #[test]
    fn set_provider_false_agent_seeds_mapped_key_when_env_empty() {
        assert_eq!(
            model_set_required_env_refs(&agent("kilo", &[])),
            vec!["KILO_API_KEY".to_owned()]
        );
    }

    #[test]
    fn set_provider_false_agent_seeds_mapped_key_despite_provider_native_env() {
        // A declared OPENROUTER_API_KEY must not suppress the mapped key:
        // Kilo requires KILO_API_KEY present in the process env even when the
        // active provider is not Kilo's gateway (an empty value is accepted).
        assert_eq!(
            model_set_required_env_refs(&agent("kilo", &["OPENROUTER_API_KEY"])),
            vec!["KILO_API_KEY".to_owned()]
        );
    }

    #[test]
    fn set_provider_false_agent_seeds_default_when_env_has_no_credential() {
        // A non-credential env (e.g. only a KILO_PROVIDER selector) still seeds
        // the default key so a missing credential surfaces as a clear error
        // rather than a stalled discovery session.
        assert_eq!(
            model_set_required_env_refs(&agent("kilo", &["KILO_PROVIDER"])),
            vec!["KILO_API_KEY".to_owned()]
        );
    }

    #[test]
    fn set_provider_false_agent_seeding_dedupes_declared_default_key() {
        // The mapped key is listed even when already declared; the seeding
        // loop in `run_agent_model_set` skips refs that `agent_env_declares`
        // matches, so the declared KILO_API_KEY is never duplicated.
        assert_eq!(
            model_set_required_env_refs(&agent("kilo", &["KILO_API_KEY"])),
            vec!["KILO_API_KEY".to_owned()]
        );
    }

    #[test]
    fn mapped_provider_uses_provider_required_refs_regardless_of_env() {
        // The mapped-provider branch is unchanged by the fix: it derives refs
        // from the provider mapping, not from whether [agent].env is populated.
        let mut mapped = agent("codex", &["OPENROUTER_API_KEY"]);
        mapped.provider = Some(AgentProviderConfig {
            id: "openrouter".to_owned(),
            model: None,
            api_key_ref: Some("OPENROUTER_API_KEY".to_owned()),
            custom: None,
        });
        assert!(model_set_required_env_refs(&mapped).contains(&"OPENROUTER_API_KEY".to_owned()));
    }

    fn mapped_provider(agent_id: &str, env: &[&str]) -> AgentConfig {
        let mut mapped = agent(agent_id, env);
        mapped.provider = Some(AgentProviderConfig {
            id: "openrouter".to_owned(),
            model: None,
            api_key_ref: Some("OPENROUTER_API_KEY".to_owned()),
            custom: None,
        });
        mapped
    }

    #[test]
    fn provider_native_env_records_empty_placeholder_for_missing_mapped_key() {
        // The env mirrors the post-seeding state: the mapped key itself is
        // already declared and must not count as the provider-native
        // credential that justifies the placeholder.
        let home = tempfile::TempDir::new().expect("tempdir");
        let mut store = SecretStore::open_or_create(home.path()).expect("create store");
        let recorded = record_empty_key_placeholders_for_provider_native_env(
            &mut store,
            &agent("kilo", &["OPENROUTER_API_KEY", "KILO_API_KEY"]),
        )
        .expect("placeholder recording");
        assert_eq!(recorded, vec!["KILO_API_KEY".to_owned()]);
        assert_eq!(store.get("KILO_API_KEY").expect("placeholder stored"), "");
    }

    #[test]
    fn provider_native_env_preserves_recorded_mapped_key() {
        let home = tempfile::TempDir::new().expect("tempdir");
        let mut store = SecretStore::open_or_create(home.path()).expect("create store");
        store.set("KILO_API_KEY", "real-key").expect("set secret");
        let recorded = record_empty_key_placeholders_for_provider_native_env(
            &mut store,
            &agent("kilo", &["OPENROUTER_API_KEY", "KILO_API_KEY"]),
        )
        .expect("placeholder recording");
        assert!(recorded.is_empty());
        assert_eq!(store.get("KILO_API_KEY").expect("key stored"), "real-key");
    }

    #[test]
    fn no_provider_native_env_leaves_missing_mapped_key_unrecorded() {
        // A KILO_PROVIDER selector alone is not a credential: the missing key
        // must surface as a clear "secret not found" error, not a placeholder.
        // The env mirrors the post-seeding state, so this also guards against
        // the seeded KILO_API_KEY itself tripping the credential gate.
        let home = tempfile::TempDir::new().expect("tempdir");
        let mut store = SecretStore::open_or_create(home.path()).expect("create store");
        let recorded = record_empty_key_placeholders_for_provider_native_env(
            &mut store,
            &agent("kilo", &["KILO_PROVIDER", "KILO_API_KEY"]),
        )
        .expect("placeholder recording");
        assert!(recorded.is_empty());
        assert!(!store.contains("KILO_API_KEY"));
    }

    #[test]
    fn placeholder_recording_is_kilo_only() {
        // Kimi rejects an empty key at launch, so a declared provider-native
        // credential must not auto-record an empty KIMI_API_KEY.
        let home = tempfile::TempDir::new().expect("tempdir");
        let mut store = SecretStore::open_or_create(home.path()).expect("create store");
        let recorded = record_empty_key_placeholders_for_provider_native_env(
            &mut store,
            &agent("kimi", &["OPENROUTER_API_KEY", "KIMI_API_KEY"]),
        )
        .expect("placeholder recording");
        assert!(recorded.is_empty());
        assert!(!store.contains("KIMI_API_KEY"));
    }

    #[test]
    fn mapped_provider_never_records_placeholders() {
        let home = tempfile::TempDir::new().expect("tempdir");
        let mut store = SecretStore::open_or_create(home.path()).expect("create store");
        let recorded = record_empty_key_placeholders_for_provider_native_env(
            &mut store,
            &mapped_provider("codex", &["OPENROUTER_API_KEY"]),
        )
        .expect("placeholder recording");
        assert!(recorded.is_empty());
        assert!(!store.contains("OPENROUTER_API_KEY"));
    }

    #[test]
    fn templated_provider_native_declaration_records_placeholder() {
        // A templated credential declaration (`VAR=${REF}` form, possible in
        // imported configs) is still a recognized provider-native credential.
        let home = tempfile::TempDir::new().expect("tempdir");
        let mut store = SecretStore::open_or_create(home.path()).expect("create store");
        let recorded = record_empty_key_placeholders_for_provider_native_env(
            &mut store,
            &agent("kilo", &["OPENROUTER_API_KEY=${MY_OR_KEY}", "KILO_API_KEY"]),
        )
        .expect("placeholder recording");
        assert_eq!(recorded, vec!["KILO_API_KEY".to_owned()]);
        assert_eq!(store.get("KILO_API_KEY").expect("placeholder stored"), "");
    }

    #[test]
    fn seed_declaration_adds_missing_kilo_mapped_key_once() {
        let mut kilo = agent("kilo", &["OPENROUTER_API_KEY"]);
        assert!(seed_kilo_mapped_key_env_declaration(&mut kilo));
        assert_eq!(
            kilo.env,
            vec!["OPENROUTER_API_KEY".to_owned(), "KILO_API_KEY".to_owned()]
        );
        // Already-declared is a no-op.
        assert!(!seed_kilo_mapped_key_env_declaration(&mut kilo));
        assert_eq!(kilo.env.len(), 2);
    }

    #[test]
    fn seed_declaration_is_kilo_only() {
        let mut kimi = agent("kimi", &[]);
        assert!(!seed_kilo_mapped_key_env_declaration(&mut kimi));
        assert!(kimi.env.is_empty());
        let mut mapped = mapped_provider("kilo", &[]);
        assert!(!seed_kilo_mapped_key_env_declaration(&mut mapped));
        assert!(mapped.env.is_empty());
    }
}
