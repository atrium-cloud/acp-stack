//! Live provider model catalogs.
//!
//! Adapter-based agents (Claude Code, Codex) cannot discover a third-party
//! provider's models over ACP: their adapters only relay the harness's own
//! catalog. This module fetches the provider's OpenAI-compatible `GET /models`
//! endpoint (declared as `models_url` in `data/providers.toml`) with the
//! operator's stored API key and caches the result on disk, so provisioning
//! and the `/v1/models` API can serve real model slugs without hardcoding any.
//!
//! Fetches are event-driven (provider/model changes and catalog reads), not
//! scheduled. Every failure degrades: callers keep the previous cache entry or
//! proceed without a catalog.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

use crate::config::Config;
use crate::dev_gates::{PROVIDER_MODELS_BASE_ENV, fixture_string};
use crate::error::{Result, StackError};
use crate::fs_util::{atomic_write_owner_only, create_dir_owner_only};
use crate::runtime::agent::provider_keys::{
    env_var_for_agent_provider_id, models_url_for_provider_id, resolve_agent_environment,
};
use crate::secrets::SecretStore;

const PROVIDER_MODEL_CACHE_FILE: &str = "provider-models.json";
const PROVIDER_MODEL_CACHE_VERSION: u32 = 1;
/// Bounded so an unreachable provider never stalls `acps agent set`/`init`.
const PROVIDER_MODELS_FETCH_TIMEOUT: Duration = Duration::from_secs(10);
/// A cache entry younger than this satisfies a refresh without a network
/// call, so a polling `/v1/models` client cannot hammer the provider.
const PROVIDER_MODELS_CACHE_TTL: Duration = Duration::from_secs(300);
/// Bounds how often a down provider is retried: without it, every polling
/// `/v1/models` request would stall up to the fetch timeout and re-hammer
/// the provider.
const PROVIDER_MODELS_FAILURE_BACKOFF: Duration = Duration::from_secs(30);

/// One model as reported by the provider's listing endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderModel {
    /// Model id the harness accepts verbatim (`agent.provider.model`).
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CachedProviderModels {
    /// Last *successful* fetch; failure markers live in the fields below so
    /// an outage never rewrites the catalog's freshness.
    fetched_at: u64,
    models: Vec<ProviderModel>,
    #[serde(default)]
    last_attempt_at: Option<u64>,
    #[serde(default)]
    last_error: Option<String>,
}

#[derive(Debug, Default, Serialize)]
struct ProviderModelCacheFile {
    version: u32,
    #[serde(default)]
    providers: BTreeMap<String, CachedProviderModels>,
}

/// Deserialization envelope for the cache file. Entries are raw values so one
/// provider's malformed row can be skipped without dropping the others.
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

/// Read the cached catalog for one provider. Corrupt or missing cache files
/// read as `None`; a cache problem must never break provisioning.
pub fn cached_models(home: &Path, provider_id: &str) -> Option<Vec<ProviderModel>> {
    let path = cache_path(home);
    let file = read_cache_file(&path)?;
    file.providers
        .get(provider_id)
        .map(|entry| entry.models.clone())
        .filter(|models| !models.is_empty())
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

/// Stored failure reason while it is still inside the backoff window, so a
/// refresh can skip a retry that would almost certainly fail again.
fn recent_failure_reason(home: &Path, provider_id: &str) -> Option<String> {
    let path = cache_path(home);
    let file = read_cache_file(&path)?;
    let entry = file.providers.get(provider_id)?;
    let attempted_at = entry.last_attempt_at?;
    let age = now_secs().saturating_sub(attempted_at);
    (age < PROVIDER_MODELS_FAILURE_BACKOFF.as_secs()).then(|| entry.last_error.clone())?
}

/// Persist a fetch failure without touching the cached catalog or
/// `fetched_at`: a stale-but-usable entry must survive provider outages.
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
        });
    entry.last_attempt_at = Some(now_secs());
    entry.last_error = Some(reason.to_owned());
    write_cache_file(&path, provider_id, &file)
}

/// Fetch the provider's live model list and persist it in the cache.
///
/// `Ok(None)` means the configured provider declares no `models_url` (custom
/// providers included) — there is nothing to fetch. Errors are returned so the
/// caller owns the fallback decision; on error the previous cache entry is
/// left untouched.
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
    // Short-circuit before API-key resolution: the backoff path must not
    // touch the secret store on every poll while the provider is down.
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
            // Store the inner reason, not the full Display: the short-circuit
            // above re-wraps it in `catalog_error`, and a stored Display would
            // double the "model catalog fetch failed" prefix. Key-resolution
            // failures are recorded too — the marker self-heals within the
            // backoff window once the operator fixes the key.
            let reason = match &error {
                StackError::ProviderModelCatalog { reason, .. } => reason.clone(),
                _ => error.to_string(),
            };
            // Best-effort: losing the marker only means the next poll retries
            // sooner, which the fetch timeout still bounds.
            if let Err(write_error) = record_fetch_failure(home, &provider.id, &reason) {
                tracing::warn!(error = %write_error, "provider model catalog failure marker not recorded");
            }
            Err(error)
        }
    }
}

/// Resolve the API key, fetch the provider's listing endpoint, and parse the
/// payload. Split from [`refresh_provider_models`] so every failure mode is
/// recorded against the cache in one place.
async fn fetch_provider_models(
    home: &Path,
    config: &Config,
    provider_id: &str,
    models_url: &str,
) -> Result<Vec<ProviderModel>> {
    let api_key = resolve_provider_api_key(home, config, provider_id)?;
    let client = reqwest::Client::new();
    let response = client
        .get(models_url)
        .bearer_auth(api_key)
        .timeout(PROVIDER_MODELS_FETCH_TIMEOUT)
        .send()
        .await
        .map_err(|error| catalog_error(provider_id, format!("request failed: {error}")))?;
    if !response.status().is_success() {
        return Err(catalog_error(
            provider_id,
            format!("endpoint returned HTTP {}", response.status()),
        ));
    }
    let body = response
        .json::<Value>()
        .await
        .map_err(|error| catalog_error(provider_id, format!("invalid JSON: {error}")))?;
    parse_models_response(provider_id, &body)
}

/// Best-effort refresh for provisioning flows: log and continue on failure.
pub async fn refresh_provider_models_best_effort(home: &Path, config: &Config) {
    if let Err(error) = refresh_provider_models(home, config).await {
        tracing::warn!(error = %error, "provider model catalog refresh skipped");
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
            tracing::warn!(error = %error, "provider model catalog refresh skipped");
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

/// Honors the `ACP_STACK_PROVIDER_MODELS_BASE` dev gate so tests can point
/// fetches at a local server: `{base}/{provider_id}/models`.
fn resolve_models_url(provider_id: &str, declared: &str) -> String {
    match fixture_string(PROVIDER_MODELS_BASE_ENV) {
        Some(base) => format!("{}/{provider_id}/models", base.trim_end_matches('/')),
        None => declared.to_owned(),
    }
}

fn resolve_provider_api_key(home: &Path, config: &Config, provider_id: &str) -> Result<String> {
    let api_key_ref = config
        .agent
        .provider
        .as_ref()
        .and_then(|provider| provider.api_key_ref.as_deref())
        .or_else(|| env_var_for_agent_provider_id(&config.agent.id, provider_id))
        .ok_or_else(|| catalog_error(provider_id, "no API key reference configured".to_owned()))?;
    // No secret-free shortcut here: every provider that declares a
    // `models_url` carries its key through the secret store, so
    // `resolve_agent_environment_without_secrets` could only ever yield an
    // empty environment and a spurious missing-key error.
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

/// Parse an OpenAI-shaped `GET /models` payload: `data[].id` required,
/// `data[].name`/`display_name` optional. Entries without a string `id` are
/// skipped so one malformed row cannot poison the whole catalog; an empty
/// list after filtering is an error so a good cache entry is never clobbered
/// by a degenerate response.
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
        models.push(ProviderModel {
            value: id.to_owned(),
            display_name,
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
    // Salvage entry-by-entry: one provider's malformed row must not drop
    // every other provider's cached catalog on the next write.
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
    // A fresh entry drops the failure markers: a successful fetch clears the
    // backoff recorded by `record_fetch_failure`.
    file.providers.insert(
        provider_id.to_owned(),
        CachedProviderModels {
            fetched_at: now_secs(),
            models: models.to_vec(),
            last_attempt_at: None,
            last_error: None,
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
                },
                ProviderModel {
                    value: "kimi-k2.7-code".to_owned(),
                    display_name: None,
                },
            ]
        );
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
        }];
        let moonshot = vec![ProviderModel {
            value: "kimi-k3".to_owned(),
            display_name: None,
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
                "version": 1,
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
                "version": 1,
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
        }];
        write_cache_entry(home.path(), "zai", &zai_models).expect("write zai");
        assert_eq!(
            cached_models(home.path(), "openrouter"),
            Some(vec![ProviderModel {
                value: "openai/gpt-5.5".to_owned(),
                display_name: None,
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
    fn successful_write_clears_failure_marker() {
        let home = temp_home();
        record_fetch_failure(home.path(), "openrouter", "boom").expect("record failure");
        assert_eq!(
            recent_failure_reason(home.path(), "openrouter"),
            Some("boom".to_owned())
        );
        let models = vec![ProviderModel {
            value: "openai/gpt-5.5".to_owned(),
            display_name: None,
        }];
        write_cache_entry(home.path(), "openrouter", &models).expect("write cache");
        assert_eq!(recent_failure_reason(home.path(), "openrouter"), None);
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

    #[tokio::test]
    async fn backoff_short_circuits_before_network() {
        let home = temp_home();
        record_fetch_failure(home.path(), "openrouter", "boom").expect("record failure");
        let config = codex_openrouter_config();
        let error = refresh_provider_models(home.path(), &config)
            .await
            .expect_err("backoff must fail with the stored reason");
        // The stored reason proves the short-circuit fired: a real attempt
        // would fail key resolution first (this temp home has no secret
        // store), never reaching the network.
        assert!(
            error.to_string().contains("boom"),
            "expected stored failure reason, got: {error}"
        );
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
