//! Live provider model catalogs, fetched from the provider's OpenAI-compatible `GET /models`
//! (`models_url` in `data/providers.toml`) and cached on disk, because adapter-based agents cannot
//! discover a third-party provider's models over ACP. Fetches are event-driven, and every failure
//! degrades to the previous cache entry or no catalog at all.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};

use crate::config::{AgentConfig, AgentProviderConfig, Config};
use crate::dev_gates::{PROVIDER_MODELS_BASE_ENV, fixture_string};
use crate::error::{Result, StackError};
use crate::fs_util::{atomic_write_owner_only, create_dir_owner_only};
use crate::runtime::agent::acp_bridge::ENDPOINT_VERDICT_AGENT_IDS;
use crate::runtime::agent::provider_keys::{
    ProviderEnvMapping, endpoint_lanes_for_provider_id, env_var_for_agent_provider_id,
    models_url_for_provider_id, resolve_agent_environment,
};
use crate::secrets::SecretStore;

const PROVIDER_MODEL_CACHE_FILE: &str = "provider-models.json";
/// Version 2 added per-model reasoning metadata; older files read as empty and refetch.
const PROVIDER_MODEL_CACHE_VERSION: u32 = 2;
/// Bounded so an unreachable provider never stalls `acps agent set`/`init`.
const PROVIDER_MODELS_FETCH_TIMEOUT: Duration = Duration::from_secs(10);
/// Tighter than the catalog fetch because the endpoint probe runs on the Agent start path.
const PROVIDER_ENDPOINT_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// Listing answers that refuse the key for that endpoint, as an empty list does.
const PROVIDER_ENDPOINT_REFUSALS: [StatusCode; 2] =
    [StatusCode::UNAUTHORIZED, StatusCode::FORBIDDEN];
/// A cache entry younger than this satisfies a refresh without a network call.
const PROVIDER_MODELS_CACHE_TTL: Duration = Duration::from_secs(300);
/// Bounds how often a down provider is retried, so polling clients cannot re-hammer it.
const PROVIDER_MODELS_FAILURE_BACKOFF: Duration = Duration::from_secs(30);

/// One model as reported by the provider's listing endpoint.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderModel {
    /// Model id the harness accepts verbatim (`agent.provider.model`).
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Reasoning-effort values the provider reports for this model (OpenRouter's
    /// `reasoning.supported_efforts`), in the provider's order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub efforts: Vec<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CachedProviderModels {
    /// Last successful fetch; an outage writes the markers below instead, never this.
    fetched_at: u64,
    models: Vec<ProviderModel>,
    #[serde(default)]
    last_attempt_at: Option<u64>,
    #[serde(default)]
    last_error: Option<String>,
    /// Set by the endpoint probe when the key's account is offered nothing here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    unavailable_at: Option<u64>,
}

impl CachedProviderModels {
    fn is_usable(&self) -> bool {
        self.unavailable_at.is_none() && !self.models.is_empty()
    }

    /// A usable or unavailable answer younger than the cache TTL, which the endpoint probe reuses.
    fn has_fresh_verdict(&self) -> bool {
        let usable_at = self.is_usable().then_some(self.fetched_at);
        let ttl = PROVIDER_MODELS_CACHE_TTL.as_secs();
        (self.unavailable_at.or(usable_at)).is_some_and(|at| now_secs().saturating_sub(at) < ttl)
    }
}

#[derive(Debug, Default, Serialize)]
struct ProviderModelCacheFile {
    version: u32,
    #[serde(default)]
    providers: BTreeMap<String, CachedProviderModels>,
}

/// Cache-file envelope. Entries stay raw so one malformed row can be skipped without dropping the
/// others.
#[derive(Debug, Deserialize)]
struct ProviderModelCacheEnvelope {
    version: u32,
    #[serde(default)]
    providers: BTreeMap<String, Value>,
}

pub fn cache_path(home: &Path) -> PathBuf {
    home.join(".config")
        .join("acp-stack")
        .join(PROVIDER_MODEL_CACHE_FILE)
}

/// Read the cached catalog for one provider; a corrupt or missing file reads as `None`.
pub fn cached_models(home: &Path, provider_id: &str) -> Option<Vec<ProviderModel>> {
    let path = cache_path(home);
    let file = read_cache_file(&path)?;
    file.providers
        .get(provider_id)
        .map(|entry| entry.models.clone())
        .filter(|models| !models.is_empty())
}

/// Reasoning-effort values cached for one model; `None` when the model is unknown or reports none.
pub fn cached_model_efforts(
    home: &Path,
    provider_id: &str,
    model_value: &str,
) -> Option<Vec<String>> {
    cached_models(home, provider_id)?
        .into_iter()
        .find(|model| model.value == model_value)
        .map(|model| model.efforts)
        .filter(|efforts| !efforts.is_empty())
}

/// Cached catalog only when younger than [`PROVIDER_MODELS_CACHE_TTL`].
fn fresh_cached_models(home: &Path, provider_id: &str) -> Option<Vec<ProviderModel>> {
    let path = cache_path(home);
    let file = read_cache_file(&path)?;
    let entry = file.providers.get(provider_id)?;
    let age = now_secs().saturating_sub(entry.fetched_at);
    (age < PROVIDER_MODELS_CACHE_TTL.as_secs() && !entry.models.is_empty())
        .then(|| entry.models.clone())
}

/// The stored failure reason while it is still inside the backoff window.
fn recent_failure_reason(home: &Path, provider_id: &str) -> Option<String> {
    let path = cache_path(home);
    let file = read_cache_file(&path)?;
    let entry = file.providers.get(provider_id)?;
    let attempted_at = entry.last_attempt_at?;
    let age = now_secs().saturating_sub(attempted_at);
    (age < PROVIDER_MODELS_FAILURE_BACKOFF.as_secs()).then(|| entry.last_error.clone())?
}

/// Persist a fetch failure without touching the cached catalog or `fetched_at`, so a
/// stale-but-usable entry survives provider outages.
fn record_fetch_failure(home: &Path, provider_id: &str, reason: &str) -> Result<()> {
    let path = cache_path(home);
    let mut file = read_cache_file(&path).unwrap_or_default();
    file.version = PROVIDER_MODEL_CACHE_VERSION;
    let entry = file
        .providers
        .entry(provider_id.to_owned())
        .or_insert_with(|| CachedProviderModels {
            fetched_at: 0,
            models: Vec::new(),
            last_attempt_at: None,
            last_error: None,
            unavailable_at: None,
        });
    entry.last_attempt_at = Some(now_secs());
    entry.last_error = Some(reason.to_owned());
    write_cache_file(&path, provider_id, &file)
}

/// Fetch the provider's live model list and persist it. `Ok(None)` means no `models_url` is
/// declared; errors leave the previous cache entry untouched for the caller to fall back on.
pub async fn refresh_provider_models(
    home: &Path,
    config: &Config,
) -> Result<Option<Vec<ProviderModel>>> {
    let Some(provider) = config.agent.provider.as_ref() else {
        return Ok(None);
    };
    if provider.custom.is_some() {
        return Ok(None);
    }
    let Some(declared_url) = models_url_for_provider_id(&provider.id) else {
        return Ok(None);
    };
    if let Some(fresh) = fresh_cached_models(home, &provider.id) {
        return Ok(Some(fresh));
    }
    // Short-circuit before anything that opens the secret store: a down provider must not pay that
    // cost on every poll.
    if let Some(reason) = recent_failure_reason(home, &provider.id) {
        return Err(catalog_error(&provider.id, reason));
    }
    let models_url = resolve_models_url(&provider.id, declared_url);

    match fetch_provider_models(home, config, &provider.id, &models_url).await {
        Ok(models) => {
            write_cache_entry(home, &provider.id, &models)?;
            Ok(Some(models))
        }
        Err(error) => {
            let reason = failure_reason(&error);
            // Best-effort: losing the marker only means the next poll retries sooner.
            if let Err(write_error) = record_fetch_failure(home, &provider.id, &reason) {
                tracing::warn!(
                    error = %crate::error::report(&write_error),
                    provider = %provider.id,
                    "provider model catalog failure marker not recorded; the next poll retries sooner"
                );
            }
            Err(error)
        }
    }
}

/// Drop one provider's cache entry so the next read refetches; a credential apply or clear changes
/// what the listing answers, and a stale entry would serve the old key's catalog. A row with
/// endpoints drops the entries of all its ids, endpoint verdicts included.
pub fn invalidate_provider_models(home: &Path, provider_id: &str) -> Result<()> {
    let path = cache_path(home);
    let Some(mut file) = read_cache_file(&path) else {
        return Ok(());
    };
    let lanes = endpoint_lanes_for_provider_id(provider_id);
    let in_row = |id: &str| id == provider_id || lanes.iter().any(|lane| lane.contains_id(id));
    let held = file.providers.len();
    file.providers.retain(|id, _| !in_row(id));
    if file.providers.len() == held {
        return Ok(());
    }
    write_cache_file(&path, provider_id, &file)
}

/// Probe each endpoint of the configured provider's row with its key, concurrently, reusing a
/// verdict younger than the cache TTL. An agent whose launch env reads no verdict, or a provider
/// without endpoints, makes no call.
pub async fn probe_provider_endpoints(
    home: &Path,
    agent: &AgentConfig,
    env: &HashMap<String, String>,
) {
    if !ENDPOINT_VERDICT_AGENT_IDS.contains(&agent.id.as_str()) {
        return;
    }
    let Some((provider, lanes)) = endpoint_row(agent) else {
        return;
    };
    // Without a key every listing answers unauthenticated, which says nothing about an account.
    let key_ref = configured_api_key_ref(agent, &provider.id);
    let Some(api_key) = key_ref.and_then(|key_ref| env.get(key_ref)) else {
        return;
    };
    let cache = read_cache_file(&cache_path(home)).unwrap_or_default();
    let probes = lanes.iter().filter_map(|lane| {
        let endpoint_id = lane.primary_id();
        let entry = cache.providers.get(endpoint_id);
        if entry.is_some_and(CachedProviderModels::has_fresh_verdict) {
            return None;
        }
        let models_url = resolve_models_url(endpoint_id, lane.models_url.as_deref()?);
        Some(probe_endpoint(home, endpoint_id, models_url, api_key))
    });
    futures::future::join_all(probes).await;
}

/// One keyed listing: models mark the endpoint usable, and an empty list, 401 or 403 marks it
/// unavailable. Any other answer is a fetch failure that leaves the earlier verdict in place.
async fn probe_endpoint(home: &Path, endpoint_id: &str, models_url: String, api_key: &str) {
    let timeout = PROVIDER_ENDPOINT_PROBE_TIMEOUT;
    let listed = match send_models_request(endpoint_id, &models_url, api_key, timeout).await {
        Ok(response) if PROVIDER_ENDPOINT_REFUSALS.contains(&response.status()) => Ok(Vec::new()),
        Ok(response) => match read_models_payload(endpoint_id, response).await {
            Ok(body) if body["data"].as_array().is_some_and(Vec::is_empty) => Ok(Vec::new()),
            body => body.and_then(|body| parse_models_response(endpoint_id, &body)),
        },
        Err(error) => Err(error),
    };
    let recorded = match listed {
        Ok(models) => record_listing(home, endpoint_id, models),
        Err(error) => {
            tracing::warn!(error = %crate::error::report(&error), provider = %endpoint_id, "provider endpoint probe inconclusive");
            record_fetch_failure(home, endpoint_id, &failure_reason(&error))
        }
    };
    if let Err(error) = recorded {
        tracing::warn!(error = %crate::error::report(&error), provider = %endpoint_id, "provider endpoint verdict not recorded");
    }
}

/// Models replace the endpoint's entry, clearing its unavailable mark; an empty list sets the mark.
fn record_listing(home: &Path, endpoint_id: &str, models: Vec<ProviderModel>) -> Result<()> {
    let path = cache_path(home);
    let mut file = read_cache_file(&path).unwrap_or_default();
    file.version = PROVIDER_MODEL_CACHE_VERSION;
    let entry = file.providers.entry(endpoint_id.to_owned()).or_default();
    if models.is_empty() {
        entry.unavailable_at = Some(now_secs());
    } else {
        *entry = CachedProviderModels::default();
        (entry.fetched_at, entry.models) = (now_secs(), models);
    }
    write_cache_file(&path, endpoint_id, &file)
}

/// Endpoint ids of the configured provider's row to leave out of the agent's model list: each
/// marked unavailable while another endpoint of the row is usable. An endpoint the configured
/// model or subagent model runs on stays. A mark holds until a keyed listing with models or an
/// invalidation clears it.
pub fn hidden_provider_endpoints(home: &Path, agent: &AgentConfig) -> Vec<&'static str> {
    let Some((provider, lanes)) = endpoint_row(agent) else {
        return Vec::new();
    };
    let cache = read_cache_file(&cache_path(home)).unwrap_or_default();
    let (mut marked, mut usable) = (Vec::new(), false);
    for &lane in &lanes {
        match cache.providers.get(lane.primary_id()) {
            Some(entry) if entry.unavailable_at.is_some() => marked.push(lane.primary_id()),
            Some(entry) => usable |= entry.is_usable(),
            None => {}
        }
    }
    let mut configured = vec![(provider, vec![&agent.model, &provider.model])];
    let subagent = agent.subagent.as_ref().filter(|config| !config.disabled);
    if let Some(other) = subagent.and_then(|subagent| subagent.provider.as_ref()) {
        configured.push((other, vec![&other.model]));
    }
    let lane_of = |id: &str| lanes.iter().copied().find(|lane| lane.contains_id(id));
    for (owner, models) in configured {
        let Some(owner_lane) = lane_of(&owner.id) else {
            continue;
        };
        // A model's `<id>/` prefix names its endpoint; a bare model, or none, runs on its provider's.
        let mut kept: Vec<_> = (models.into_iter().flatten())
            .map(|model| lane_of(model.split_once('/').unwrap_or_default().0).unwrap_or(owner_lane))
            .collect();
        if kept.is_empty() {
            kept.push(owner_lane);
        }
        marked.retain(|endpoint_id| kept.iter().all(|lane| lane.primary_id() != *endpoint_id));
    }
    if usable { marked } else { Vec::new() }
}

/// The configured mapped provider and the endpoint lanes of its row that share its key env var.
/// A lane with another key would receive the configured key, so it is never probed or hidden.
fn endpoint_row(
    agent: &AgentConfig,
) -> Option<(&AgentProviderConfig, Vec<&'static ProviderEnvMapping>)> {
    let provider = agent.provider.as_ref()?;
    let key = |id: &str| env_var_for_agent_provider_id(&agent.id, id);
    let row = endpoint_lanes_for_provider_id(&provider.id).iter();
    let lanes: Vec<_> = row
        .filter(|lane| key(lane.primary_id()) == key(&provider.id))
        .collect();
    (provider.custom.is_none() && !lanes.is_empty()).then_some((provider, lanes))
}

/// Resolve the API key, fetch the listing endpoint, and parse the payload.
async fn fetch_provider_models(
    home: &Path,
    config: &Config,
    provider_id: &str,
    models_url: &str,
) -> Result<Vec<ProviderModel>> {
    let api_key = resolve_provider_api_key(home, config, provider_id)?;
    let body = fetch_models_payload(provider_id, models_url, &api_key).await?;
    parse_models_response(provider_id, &body)
}

async fn fetch_models_payload(provider_id: &str, models_url: &str, api_key: &str) -> Result<Value> {
    let timeout = PROVIDER_MODELS_FETCH_TIMEOUT;
    let response = send_models_request(provider_id, models_url, api_key, timeout).await?;
    read_models_payload(provider_id, response).await
}

async fn send_models_request(
    provider_id: &str,
    models_url: &str,
    api_key: &str,
    timeout: Duration,
) -> Result<reqwest::Response> {
    let client_build_failed =
        |reason: String| catalog_error(provider_id, format!("client build failed: {reason}"));
    let client = crate::http_client::provider_client_builder()
        .map_err(|error| client_build_failed(crate::error::report(&error)))?
        .build()
        .map_err(|error| client_build_failed(crate::error::report(&error)))?;
    client
        .get(models_url)
        .bearer_auth(api_key)
        .timeout(timeout)
        .send()
        .await
        .map_err(|error| {
            catalog_error(
                provider_id,
                format!("request failed: {}", crate::error::report(&error)),
            )
        })
}

async fn read_models_payload(provider_id: &str, response: reqwest::Response) -> Result<Value> {
    let status = response.status();
    if !status.is_success() {
        let body = crate::http_client::error_response_body(response).await;
        return Err(catalog_error(
            provider_id,
            format!("endpoint returned HTTP {status}: {body}"),
        ));
    }
    response.json::<Value>().await.map_err(|error| {
        catalog_error(
            provider_id,
            format!("invalid JSON: {}", crate::error::report(&error)),
        )
    })
}

/// Best-effort refresh for provisioning flows: log and continue on failure.
pub async fn refresh_provider_models_best_effort(home: &Path, config: &Config) {
    if let Err(error) = refresh_provider_models(home, config).await {
        tracing::warn!(error = %crate::error::report(&error), "provider model catalog refresh skipped");
    }
}

/// Blocking wrapper for sync CLI paths. Never call from async code.
pub fn refresh_provider_models_best_effort_blocking(home: &Path, config: &Config) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::warn!(error = %crate::error::report(&error), "provider model catalog refresh skipped");
            return;
        }
    };
    runtime.block_on(refresh_provider_models_best_effort(home, config));
}

fn catalog_error(provider_id: &str, reason: String) -> StackError {
    StackError::ProviderModelCatalog {
        provider: provider_id.to_owned(),
        reason,
    }
}

/// The inner reason, not the full Display: the backoff short-circuit re-wraps a stored reason in
/// `catalog_error`, and a stored Display would double the prefix.
fn failure_reason(error: &StackError) -> String {
    match error {
        StackError::ProviderModelCatalog { reason, .. } => reason.clone(),
        _ => error.to_string(),
    }
}

fn configured_api_key_ref<'a>(agent: &'a AgentConfig, provider_id: &str) -> Option<&'a str> {
    agent
        .provider
        .as_ref()
        .and_then(|provider| provider.api_key_ref.as_deref())
        .or_else(|| env_var_for_agent_provider_id(&agent.id, provider_id))
}

/// The `ACP_STACK_PROVIDER_MODELS_BASE` dev gate outranks the declared `models_url`.
fn resolve_models_url(provider_id: &str, declared: &str) -> String {
    match fixture_string(PROVIDER_MODELS_BASE_ENV) {
        Some(base) => format!("{}/{provider_id}/models", base.trim_end_matches('/')),
        None => declared.to_owned(),
    }
}

fn resolve_provider_api_key(home: &Path, config: &Config, provider_id: &str) -> Result<String> {
    let api_key_ref = configured_api_key_ref(&config.agent, provider_id)
        .ok_or_else(|| catalog_error(provider_id, "no API key reference configured".to_owned()))?;
    // No secret-free shortcut: every provider declaring a `models_url` carries its key through the
    // secret store, so the without-secrets path could only yield a spurious missing-key error.
    let store = SecretStore::open(home)?;
    let env = resolve_agent_environment(config, &store)?.env;
    env.get(api_key_ref)
        .filter(|value| !value.trim().is_empty())
        .cloned()
        .ok_or_else(|| {
            catalog_error(
                provider_id,
                format!("API key `{api_key_ref}` is not available"),
            )
        })
}

/// Parse an OpenAI-shaped `GET /models` payload. A row without a string `id` is skipped, but an
/// empty list after filtering is an error so a degenerate response never clobbers a good cache.
fn parse_models_response(provider_id: &str, body: &Value) -> Result<Vec<ProviderModel>> {
    let entries = body
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| catalog_error(provider_id, "response has no `data` array".to_owned()))?;
    let mut models = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let Some(id) = entry.get("id").and_then(Value::as_str) else {
            tracing::warn!(provider = %provider_id, index, "skipping model entry without a string `id`");
            continue;
        };
        let display_name = entry
            .get("name")
            .or_else(|| entry.get("display_name"))
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty() && *name != id)
            .map(str::to_owned);
        let reasoning = entry.get("reasoning").filter(|value| value.is_object());
        let efforts = reasoning
            .and_then(|reasoning| reasoning.get("supported_efforts"))
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        models.push(ProviderModel {
            value: id.to_owned(),
            display_name,
            efforts,
        });
    }
    if models.is_empty() {
        return Err(catalog_error(
            provider_id,
            "endpoint returned an empty model list".to_owned(),
        ));
    }
    Ok(models)
}

fn read_cache_file(path: &Path) -> Option<ProviderModelCacheFile> {
    if !path.exists() {
        return None;
    }
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => {
            tracing::warn!(path = %path.display(), error = %error, "provider model cache unreadable");
            return None;
        }
    };
    let envelope: ProviderModelCacheEnvelope = match serde_json::from_str(&text) {
        Ok(envelope) => envelope,
        Err(error) => {
            tracing::warn!(path = %path.display(), error = %error, "provider model cache is corrupt");
            return None;
        }
    };
    if envelope.version != PROVIDER_MODEL_CACHE_VERSION {
        return None;
    }
    // Salvage entry-by-entry: one malformed row must not drop every other provider's catalog.
    let mut providers = BTreeMap::new();
    for (provider_id, raw_entry) in envelope.providers {
        match serde_json::from_value::<CachedProviderModels>(raw_entry) {
            Ok(entry) => {
                providers.insert(provider_id, entry);
            }
            Err(error) => {
                tracing::warn!(provider = %provider_id, error = %error, "skipping corrupt provider model cache entry");
            }
        }
    }
    Some(ProviderModelCacheFile {
        version: envelope.version,
        providers,
    })
}

fn write_cache_file(path: &Path, provider_id: &str, file: &ProviderModelCacheFile) -> Result<()> {
    let body = serde_json::to_string_pretty(file)
        .map_err(|error| catalog_error(provider_id, format!("failed to encode cache: {error}")))?;
    if let Some(parent) = path.parent() {
        create_dir_owner_only(parent)?;
    }
    atomic_write_owner_only(path, body.as_bytes())
}

fn write_cache_entry(home: &Path, provider_id: &str, models: &[ProviderModel]) -> Result<()> {
    let path = cache_path(home);
    let mut file = read_cache_file(&path).unwrap_or_default();
    file.version = PROVIDER_MODEL_CACHE_VERSION;
    // A fresh entry drops the failure markers and the unavailable mark, clearing the backoff.
    file.providers.insert(
        provider_id.to_owned(),
        CachedProviderModels {
            fetched_at: now_secs(),
            models: models.to_vec(),
            last_attempt_at: None,
            last_error: None,
            unavailable_at: None,
        },
    );
    write_cache_file(&path, provider_id, &file)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_home() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp home")
    }

    #[test]
    fn parses_openai_shape_with_and_without_names() {
        let body = json!({
            "data": [
                { "id": "kimi-k3", "name": "Kimi K3" },
                { "id": "kimi-k2.7-code" },
            ]
        });
        let models = parse_models_response("moonshotai", &body).expect("models");
        assert_eq!(
            models,
            vec![
                ProviderModel {
                    value: "kimi-k3".to_owned(),
                    display_name: Some("Kimi K3".to_owned()),
                    ..Default::default()
                },
                ProviderModel {
                    value: "kimi-k2.7-code".to_owned(),
                    display_name: None,
                    ..Default::default()
                },
            ]
        );
    }

    #[test]
    fn parses_openrouter_reasoning_metadata() {
        let body = json!({
            "data": [
                {
                    "id": "deepseek/deepseek-v4-flash",
                    "reasoning": {
                        "mandatory": false,
                        "supported_efforts": ["xhigh", "high", 7],
                        "default_effort": "high"
                    }
                },
                { "id": "anthropic/claude-sonnet-4.5", "reasoning": { "mandatory": false } },
                { "id": "meta-llama/llama-3.1-8b-instruct" },
                { "id": "odd/reasoning-shape", "reasoning": "yes" },
            ]
        });
        let models = parse_models_response("openrouter", &body).expect("models");
        assert_eq!(
            models[0].efforts,
            vec!["xhigh".to_owned(), "high".to_owned()]
        );
        assert!(models[1].efforts.is_empty());
        assert!(models[2].efforts.is_empty());
        assert!(models[3].efforts.is_empty());
    }

    #[test]
    fn cached_model_efforts_requires_a_known_model_with_values() {
        let home = temp_home();
        write_cache_entry(
            home.path(),
            "openrouter",
            &[
                ProviderModel {
                    value: "deepseek/deepseek-v4-flash".to_owned(),
                    efforts: vec!["xhigh".to_owned(), "high".to_owned()],
                    ..Default::default()
                },
                ProviderModel {
                    value: "meta-llama/llama-3.1-8b-instruct".to_owned(),
                    ..Default::default()
                },
            ],
        )
        .expect("write cache");
        assert_eq!(
            cached_model_efforts(home.path(), "openrouter", "deepseek/deepseek-v4-flash"),
            Some(vec!["xhigh".to_owned(), "high".to_owned()])
        );
        assert_eq!(
            cached_model_efforts(
                home.path(),
                "openrouter",
                "meta-llama/llama-3.1-8b-instruct"
            ),
            None
        );
        assert_eq!(
            cached_model_efforts(home.path(), "openrouter", "unknown/model"),
            None
        );
        assert_eq!(
            cached_model_efforts(home.path(), "moonshotai", "deepseek/deepseek-v4-flash"),
            None
        );
    }

    #[test]
    fn previous_cache_version_reads_as_empty() {
        let home = temp_home();
        let path = cache_path(home.path());
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(
            &path,
            br#"{
                "version": 1,
                "providers": {
                    "openrouter": {
                        "fetched_at": 100,
                        "models": [{ "value": "openai/gpt-5.5" }]
                    }
                }
            }"#,
        )
        .expect("write");
        assert_eq!(cached_models(home.path(), "openrouter"), None);
    }

    #[test]
    fn drops_display_name_matching_the_id() {
        let body = json!({ "data": [{ "id": "glm-5.2", "name": "glm-5.2" }] });
        let models = parse_models_response("zai", &body).expect("models");
        assert_eq!(models[0].display_name, None);
    }

    #[test]
    fn empty_data_is_an_error() {
        let body = json!({ "data": [] });
        assert!(parse_models_response("openrouter", &body).is_err());
    }

    #[test]
    fn missing_data_array_is_an_error() {
        assert!(parse_models_response("openrouter", &json!({ "models": [] })).is_err());
        assert!(parse_models_response("openrouter", &json!("nonsense")).is_err());
    }

    #[test]
    fn entry_without_id_is_skipped() {
        let body = json!({
            "data": [
                { "name": "mystery" },
                { "id": "kimi-k3" },
                { "id": 42 },
            ]
        });
        let models = parse_models_response("moonshotai", &body).expect("models");
        assert_eq!(
            models,
            vec![ProviderModel {
                value: "kimi-k3".to_owned(),
                display_name: None,
                ..Default::default()
            }]
        );
    }

    #[test]
    fn all_entries_invalid_is_an_error() {
        let body = json!({ "data": [{ "name": "mystery" }, { "id": 42 }] });
        assert!(parse_models_response("openrouter", &body).is_err());
    }

    #[test]
    fn cache_round_trips_per_provider() {
        let home = temp_home();
        let models = vec![ProviderModel {
            value: "deepseek/deepseek-v4-flash".to_owned(),
            display_name: Some("DeepSeek V4 Flash".to_owned()),
            ..Default::default()
        }];
        write_cache_entry(home.path(), "openrouter", &models).expect("write cache");
        assert_eq!(cached_models(home.path(), "openrouter"), Some(models));
        assert_eq!(cached_models(home.path(), "moonshotai"), None);
    }

    #[test]
    fn second_provider_write_preserves_the_first() {
        let home = temp_home();
        let openrouter = vec![ProviderModel {
            value: "openai/gpt-5.5".to_owned(),
            display_name: None,
            ..Default::default()
        }];
        let moonshot = vec![ProviderModel {
            value: "kimi-k3".to_owned(),
            display_name: None,
            ..Default::default()
        }];
        write_cache_entry(home.path(), "openrouter", &openrouter).expect("write openrouter");
        write_cache_entry(home.path(), "moonshotai", &moonshot).expect("write moonshot");
        assert_eq!(cached_models(home.path(), "openrouter"), Some(openrouter));
        assert_eq!(cached_models(home.path(), "moonshotai"), Some(moonshot));
    }

    #[test]
    fn corrupt_cache_reads_as_none() {
        let home = temp_home();
        let path = cache_path(home.path());
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, b"not json").expect("write");
        assert_eq!(cached_models(home.path(), "openrouter"), None);
    }

    #[test]
    fn version_mismatch_reads_as_none() {
        let home = temp_home();
        let path = cache_path(home.path());
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, br#"{"version": 99, "providers": {}}"#).expect("write");
        assert_eq!(cached_models(home.path(), "openrouter"), None);
    }

    #[test]
    fn corrupt_entry_drops_only_that_provider() {
        let home = temp_home();
        let path = cache_path(home.path());
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(
            &path,
            br#"{
                "version": 2,
                "providers": {
                    "openrouter": {
                        "fetched_at": 100,
                        "models": [{ "value": "openai/gpt-5.5" }]
                    },
                    "moonshotai": { "fetched_at": "not-a-timestamp" }
                }
            }"#,
        )
        .expect("write");
        assert_eq!(
            cached_models(home.path(), "openrouter"),
            Some(vec![ProviderModel {
                value: "openai/gpt-5.5".to_owned(),
                display_name: None,
                ..Default::default()
            }])
        );
        assert_eq!(cached_models(home.path(), "moonshotai"), None);
    }

    #[test]
    fn write_with_corrupt_sibling_preserves_valid_entries() {
        let home = temp_home();
        let path = cache_path(home.path());
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(
            &path,
            br#"{
                "version": 2,
                "providers": {
                    "openrouter": {
                        "fetched_at": 100,
                        "models": [{ "value": "openai/gpt-5.5" }]
                    },
                    "moonshotai": { "fetched_at": "not-a-timestamp" }
                }
            }"#,
        )
        .expect("write");
        let zai_models = vec![ProviderModel {
            value: "glm-5.2".to_owned(),
            display_name: None,
            ..Default::default()
        }];
        write_cache_entry(home.path(), "zai", &zai_models).expect("write zai");
        assert_eq!(
            cached_models(home.path(), "openrouter"),
            Some(vec![ProviderModel {
                value: "openai/gpt-5.5".to_owned(),
                display_name: None,
                ..Default::default()
            }])
        );
        assert_eq!(cached_models(home.path(), "zai"), Some(zai_models));
    }

    #[test]
    fn recorded_failure_preserves_models_and_round_trips() {
        let home = temp_home();
        let models = vec![ProviderModel {
            value: "openai/gpt-5.5".to_owned(),
            display_name: None,
            ..Default::default()
        }];
        write_cache_entry(home.path(), "openrouter", &models).expect("write cache");
        record_fetch_failure(home.path(), "openrouter", "boom").expect("record failure");
        assert_eq!(cached_models(home.path(), "openrouter"), Some(models));
        assert_eq!(
            recent_failure_reason(home.path(), "openrouter"),
            Some("boom".to_owned())
        );
    }

    #[test]
    fn successful_write_clears_failure_and_unavailable_markers() {
        let home = temp_home();
        record_listing(home.path(), "openrouter", Vec::new()).expect("mark unavailable");
        record_fetch_failure(home.path(), "openrouter", "boom").expect("record failure");
        assert_eq!(
            recent_failure_reason(home.path(), "openrouter"),
            Some("boom".to_owned())
        );
        let models = vec![ProviderModel {
            value: "openai/gpt-5.5".to_owned(),
            display_name: None,
            ..Default::default()
        }];
        write_cache_entry(home.path(), "openrouter", &models).expect("write cache");
        assert_eq!(recent_failure_reason(home.path(), "openrouter"), None);
        let file = read_cache_file(&cache_path(home.path())).expect("cache");
        assert!(file.providers["openrouter"].is_usable());
    }

    #[test]
    fn recorded_failure_survives_corrupt_cache_file() {
        let home = temp_home();
        let path = cache_path(home.path());
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, b"not json").expect("write");
        record_fetch_failure(home.path(), "openrouter", "boom").expect("record failure");
        assert_eq!(
            recent_failure_reason(home.path(), "openrouter"),
            Some("boom".to_owned())
        );
    }

    #[test]
    fn invalidate_removes_entry_and_failure_markers_only_for_that_provider() {
        let home = temp_home();
        let openrouter = vec![ProviderModel {
            value: "openai/gpt-5.5".to_owned(),
            display_name: None,
            ..Default::default()
        }];
        let moonshot = vec![ProviderModel {
            value: "kimi-k3".to_owned(),
            display_name: None,
            ..Default::default()
        }];
        write_cache_entry(home.path(), "openrouter", &openrouter).expect("write openrouter");
        record_fetch_failure(home.path(), "openrouter", "boom").expect("record failure");
        write_cache_entry(home.path(), "moonshotai", &moonshot).expect("write moonshot");

        invalidate_provider_models(home.path(), "openrouter").expect("invalidate");

        assert_eq!(cached_models(home.path(), "openrouter"), None);
        assert_eq!(recent_failure_reason(home.path(), "openrouter"), None);
        assert_eq!(cached_models(home.path(), "moonshotai"), Some(moonshot));
    }

    #[test]
    fn invalidate_tolerates_missing_unknown_and_corrupt_caches() {
        let home = temp_home();
        invalidate_provider_models(home.path(), "openrouter").expect("missing cache is fine");

        write_cache_entry(
            home.path(),
            "moonshotai",
            &[ProviderModel {
                value: "kimi-k3".to_owned(),
                display_name: None,
                ..Default::default()
            }],
        )
        .expect("write moonshot");
        invalidate_provider_models(home.path(), "openrouter").expect("unknown entry is fine");

        let path = cache_path(home.path());
        std::fs::write(&path, b"not json").expect("write");
        invalidate_provider_models(home.path(), "openrouter").expect("corrupt cache is fine");
    }

    #[tokio::test]
    async fn backoff_short_circuits_before_network() {
        let home = temp_home();
        record_fetch_failure(home.path(), "openrouter", "boom").expect("record failure");
        let config = codex_openrouter_config();
        let error = refresh_provider_models(home.path(), &config)
            .await
            .expect_err("backoff must fail with the stored reason");
        // The stored reason proves the short-circuit fired: a real attempt would fail key
        // resolution first, never reaching the network.
        assert!(
            error.to_string().contains("boom"),
            "expected stored failure reason, got: {error}"
        );
    }

    #[tokio::test]
    async fn non_success_status_keeps_the_redacted_bounded_body_in_the_reason() {
        use axum::http::StatusCode;
        use axum::routing::get;

        let router = axum::Router::new().route(
            "/api/v1/models",
            get(|| async {
                (
                    StatusCode::UNAUTHORIZED,
                    format!(
                        r#"{{"error":"invalid key sk-providercatalog0123456789"}} {}"#,
                        "w".repeat(2 * crate::redaction::HTTP_ERROR_BODY_MAX_BYTES)
                    ),
                )
            }),
        );
        let server = crate::http_client::test_server::spawn(router);
        let error = fetch_models_payload(
            "openrouter",
            &server.url("/api/v1/models"),
            "unused-test-key",
        )
        .await
        .expect_err("401 must fail");
        let StackError::ProviderModelCatalog { reason, .. } = &error else {
            panic!("unexpected error: {error}");
        };
        assert!(
            reason.starts_with("endpoint returned HTTP 401 Unauthorized: "),
            "{reason}"
        );
        assert!(reason.contains("invalid key [redacted]"), "{reason}");
        assert!(reason.contains("[truncated"), "{reason}");
    }

    // An empty list or a refusal is the vendor's answer for this key; any other failure decides nothing.
    #[tokio::test]
    async fn endpoint_probe_answers_classify_into_verdicts() {
        use axum::routing::get;

        let router = axum::Router::new()
            .route("/200", get(|| async { r#"{"data":[{"id":"glm-5.2"}]}"# }))
            .route("/empty", get(|| async { r#"{"data":[]}"# }))
            .route("/401", get(|| async { StatusCode::UNAUTHORIZED }))
            .route("/403", get(|| async { StatusCode::FORBIDDEN }))
            .route("/500", get(|| async { StatusCode::INTERNAL_SERVER_ERROR }));
        let server = crate::http_client::test_server::spawn(router);
        let home = temp_home();
        for (answer, verdict) in [
            ("200", "usable"),
            ("empty", "unavailable"),
            ("401", "unavailable"),
            ("403", "unavailable"),
            ("500", "unknown"),
        ] {
            let url = server.url(&format!("/{answer}"));
            probe_endpoint(home.path(), answer, url, "unused-test-key").await;
            let file = read_cache_file(&cache_path(home.path())).expect("cache written");
            let entry = &file.providers[answer];
            let recorded = match (entry.is_usable(), entry.unavailable_at, &entry.last_error) {
                (true, ..) => "usable",
                (_, Some(_), _) => "unavailable",
                (_, _, Some(_)) => "unknown",
                _ => "nothing",
            };
            assert_eq!(recorded, verdict, "{answer}");
        }
    }

    #[test]
    fn hidden_endpoints_need_a_usable_sibling_and_spare_the_configured_model() {
        let home = temp_home();
        let mut agent = codex_openrouter_config().agent;
        agent.id = "pi".to_owned();
        let provider = agent.provider.as_mut().expect("provider");
        provider.id = "opencode".to_owned();
        provider.model = None;
        let hidden = |agent: &AgentConfig| hidden_provider_endpoints(home.path(), agent);
        let listed = parse_models_response("opencode", &json!({ "data": [{ "id": "glm-5.2" }] }));
        record_listing(home.path(), "opencode", listed.expect("models")).expect("usable");
        record_listing(home.path(), "opencode-go", Vec::new()).expect("unavailable");
        assert_eq!(hidden(&agent), ["opencode-go"]);

        // With no model, the configured provider's own lane stays.
        agent.provider.as_mut().expect("provider").id = "opencode-go".to_owned();
        assert!(hidden(&agent).is_empty());
        agent.provider.as_mut().expect("provider").id = "opencode".to_owned();

        let provider = agent.provider.as_mut().expect("provider");
        provider.model = Some("opencode-go/glm-5.2".to_owned());
        assert!(hidden(&agent).is_empty());

        agent.provider.as_mut().expect("provider").model = None;
        record_listing(home.path(), "opencode", Vec::new()).expect("unavailable");
        assert!(hidden(&agent).is_empty());

        // Any id of the row drops every endpoint's entry.
        invalidate_provider_models(home.path(), "opencode-zen").expect("invalidate");
        assert_eq!(cached_models(home.path(), "opencode"), None);
    }

    fn codex_openrouter_config() -> Config {
        crate::config::load_config_from_str(
            r#"
[api]
bind = "127.0.0.1:7700"
public_url = "http://127.0.0.1:7700"
max_request_bytes = 104857600

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

[logging]
level = "info"
local_retention_days = 30

[logging.supabase]
enabled = false
url = "https://example.supabase.co"
api_key_ref = "SUPABASE_SECRET_KEY"
schema = "acp_stack"

[agent]
id = "codex"
name = "Codex"
command = "codex"
args = []
cwd = "/workspace"
env = []
restart = "on-crash"

[agent.provider]
id = "openrouter"
model = "deepseek/deepseek-v4-flash"
api_key_ref = "OPENROUTER_API_KEY"
"#,
        )
        .expect("config parses")
    }
}
