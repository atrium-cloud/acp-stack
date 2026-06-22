//! models.dev-backed prompt modality checks.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent_client_protocol::schema::{
    ContentBlock, EmbeddedResourceResource, ImageContent, ResourceLink,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::config::AgentConfig;
use crate::error::{Result, StackError};
use crate::fs_util::{atomic_write_owner_only, create_dir_owner_only};

const MODELS_DEV_MODELS_JSON_URL: &str = "https://models.dev/models.json";
/// models.dev changes often, but prompt submission must not fetch on every turn.
const MODELS_DEV_CATALOG_REFRESH_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
/// Avoid repeated network attempts after a failed refresh while keeping recovery timely.
const MODELS_DEV_CATALOG_FETCH_RETRY_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// Prompt validation runs inline with HTTP submission, so catalog fetches must be bounded tightly.
const MODELS_DEV_CATALOG_FETCH_TIMEOUT: Duration = Duration::from_secs(5);
const MODELS_DEV_CATALOG_CACHE_VERSION: u32 = 1;
const MODELS_DEV_CATALOG_CACHE_FILE: &str = "models-dev-models.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelSupport {
    Supported,
    Unsupported,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptInputModality {
    Image,
    Audio,
    Video,
}

impl PromptInputModality {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Audio => "audio",
            Self::Video => "video",
        }
    }

    fn from_models_dev(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "image" => Some(Self::Image),
            "audio" => Some(Self::Audio),
            "video" => Some(Self::Video),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub struct AgentModelCatalogManager {
    state: Mutex<AgentModelCatalogState>,
    models_url: String,
    cache_path: PathBuf,
    http_client: reqwest::Client,
}

#[derive(Debug, Default)]
struct AgentModelCatalogState {
    catalog: Option<ModelsDevCatalog>,
    fetched_at: Option<u64>,
    loaded_from_disk: bool,
    last_failed_refresh_attempt_at: Option<u64>,
}

impl AgentModelCatalogManager {
    pub fn new(cache_path: PathBuf) -> Self {
        Self::new_with_models_url(MODELS_DEV_MODELS_JSON_URL, cache_path)
    }

    fn new_with_models_url(models_url: impl Into<String>, cache_path: PathBuf) -> Self {
        Self {
            state: Mutex::new(AgentModelCatalogState::default()),
            models_url: models_url.into(),
            cache_path,
            http_client: reqwest::Client::new(),
        }
    }

    pub fn cache_path_for_state_path(state_path: &Path) -> PathBuf {
        state_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(MODELS_DEV_CATALOG_CACHE_FILE)
    }

    pub async fn ensure_prompt_supported(
        &self,
        model_id: Option<&str>,
        blocks: &[ContentBlock],
    ) -> Result<()> {
        let required_modalities = prompt_required_modalities(blocks);
        if required_modalities.is_empty() {
            return Ok(());
        }
        let Some(model_id) = model_id.map(str::trim).filter(|model| !model.is_empty()) else {
            return Ok(());
        };

        let mut state = self.state.lock().await;
        self.load_disk_cache_if_needed(&mut state);
        self.refresh_catalog_if_needed(&mut state).await;
        let Some(catalog) = current_catalog(&state) else {
            return Ok(());
        };

        for modality in required_modalities {
            if catalog.support(model_id, modality) == ModelSupport::Unsupported {
                return Err(StackError::PromptUnsupportedModality {
                    model: model_id.to_owned(),
                    modality: modality.as_str().to_owned(),
                });
            }
        }
        Ok(())
    }

    fn load_disk_cache_if_needed(&self, state: &mut AgentModelCatalogState) {
        if state.loaded_from_disk {
            return;
        }
        match read_cached_models_dev_catalog(&self.models_url, &self.cache_path) {
            Ok(Some(cached)) => {
                state.catalog = Some(cached.catalog);
                state.fetched_at = Some(cached.fetched_at);
                state.last_failed_refresh_attempt_at = cached.last_failed_refresh_attempt_at;
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(error = %error, "failed to read cached models.dev catalog");
            }
        }
        state.loaded_from_disk = true;
    }

    async fn refresh_catalog_if_needed(&self, state: &mut AgentModelCatalogState) {
        if !catalog_should_refresh(state.fetched_at)
            || !catalog_refresh_should_retry(state.last_failed_refresh_attempt_at)
        {
            return;
        }

        match fetch_models_dev_catalog(&self.http_client, &self.models_url).await {
            Ok(fetched) => {
                if let Err(error) =
                    write_cached_models_dev_catalog(&fetched, &self.models_url, &self.cache_path)
                {
                    tracing::warn!(error = %error, "failed to write cached models.dev catalog");
                }
                state.catalog = Some(fetched.catalog);
                state.fetched_at = Some(fetched.fetched_at);
                state.last_failed_refresh_attempt_at = None;
            }
            Err(error) => {
                tracing::warn!(error = %error, "failed to refresh models.dev catalog");
                let failed_at = now_secs();
                if let Err(error) = record_failed_models_dev_catalog_refresh(
                    &self.models_url,
                    failed_at,
                    &self.cache_path,
                ) {
                    tracing::warn!(
                        error = %error,
                        "failed to write models.dev refresh attempt"
                    );
                }
                state.last_failed_refresh_attempt_at = Some(failed_at);
            }
        }
    }
}

pub fn selected_agent_model(agent: &AgentConfig) -> Option<&str> {
    agent.model.as_deref().or_else(|| {
        agent
            .provider
            .as_ref()
            .and_then(|provider| provider.model.as_deref())
    })
}

pub fn prompt_required_modalities(blocks: &[ContentBlock]) -> BTreeSet<PromptInputModality> {
    let mut modalities = BTreeSet::new();
    for block in blocks {
        match block {
            ContentBlock::Image(ImageContent { .. }) => {
                modalities.insert(PromptInputModality::Image);
            }
            ContentBlock::Audio(_) => {
                modalities.insert(PromptInputModality::Audio);
            }
            ContentBlock::Resource(resource) => {
                let EmbeddedResourceResource::BlobResourceContents(blob) = &resource.resource
                else {
                    continue;
                };
                if let Some(modality) = blob.mime_type.as_deref().and_then(modality_from_mime_type)
                {
                    modalities.insert(modality);
                }
            }
            ContentBlock::Text(_) | ContentBlock::ResourceLink(ResourceLink { .. }) => {}
            _ => {}
        }
    }
    modalities
}

fn modality_from_mime_type(mime_type: &str) -> Option<PromptInputModality> {
    let media_type = mime_type
        .split_once(';')
        .map_or(mime_type, |(media_type, _)| media_type)
        .trim()
        .to_ascii_lowercase();
    if media_type.starts_with("image/") {
        Some(PromptInputModality::Image)
    } else if media_type.starts_with("audio/") {
        Some(PromptInputModality::Audio)
    } else if media_type.starts_with("video/") {
        Some(PromptInputModality::Video)
    } else {
        None
    }
}

fn current_catalog(state: &AgentModelCatalogState) -> Option<&ModelsDevCatalog> {
    if catalog_should_refresh(state.fetched_at) {
        return None;
    }
    state.catalog.as_ref()
}

fn catalog_should_refresh(fetched_at: Option<u64>) -> bool {
    match fetched_at {
        Some(fetched_at) => {
            now_secs().saturating_sub(fetched_at) >= MODELS_DEV_CATALOG_REFRESH_INTERVAL.as_secs()
        }
        None => true,
    }
}

fn catalog_refresh_should_retry(last_failed_attempt_at: Option<u64>) -> bool {
    match last_failed_attempt_at {
        Some(attempt_at) => {
            now_secs().saturating_sub(attempt_at)
                >= MODELS_DEV_CATALOG_FETCH_RETRY_INTERVAL.as_secs()
        }
        None => true,
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[derive(Debug)]
struct FetchedModelsDevCatalog {
    catalog: ModelsDevCatalog,
    models_json: Value,
    fetched_at: u64,
}

async fn fetch_models_dev_catalog(
    client: &reqwest::Client,
    models_url: &str,
) -> std::result::Result<FetchedModelsDevCatalog, String> {
    let response = client
        .get(models_url)
        .timeout(MODELS_DEV_CATALOG_FETCH_TIMEOUT)
        .send()
        .await
        .map_err(|error| format!("request models.dev catalog: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "models.dev catalog returned HTTP {}",
            response.status()
        ));
    }
    let value = response
        .json::<Value>()
        .await
        .map_err(|error| format!("parse models.dev catalog JSON: {error}"))?;
    let catalog = ModelsDevCatalog::from_value(value.clone())
        .map_err(|error| format!("parse models.dev catalog schema: {error}"))?;
    Ok(FetchedModelsDevCatalog {
        catalog,
        models_json: value,
        fetched_at: now_secs(),
    })
}

#[derive(Debug)]
struct CachedModelsDevCatalog {
    catalog: ModelsDevCatalog,
    fetched_at: u64,
    last_failed_refresh_attempt_at: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CachedModelsDevCatalogFile {
    version: u32,
    source_url: String,
    fetched_at: Option<u64>,
    last_failed_refresh_attempt_at: Option<u64>,
    models: Option<Value>,
}

fn read_cached_models_dev_catalog(
    models_url: &str,
    path: &Path,
) -> std::result::Result<Option<CachedModelsDevCatalog>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    let file: CachedModelsDevCatalogFile =
        serde_json::from_str(&text).map_err(|error| format!("parse cache JSON: {error}"))?;
    if file.version != MODELS_DEV_CATALOG_CACHE_VERSION || file.source_url != models_url {
        return Ok(None);
    }
    let Some(fetched_at) = file.fetched_at else {
        return Ok(None);
    };
    let Some(models) = file.models else {
        return Ok(None);
    };
    let catalog = ModelsDevCatalog::from_value(models)
        .map_err(|error| format!("parse cached models.dev catalog: {error}"))?;
    Ok(Some(CachedModelsDevCatalog {
        catalog,
        fetched_at,
        last_failed_refresh_attempt_at: file.last_failed_refresh_attempt_at,
    }))
}

fn write_cached_models_dev_catalog(
    fetched: &FetchedModelsDevCatalog,
    models_url: &str,
    path: &Path,
) -> std::result::Result<(), String> {
    if let Some(parent) = path.parent() {
        create_dir_owner_only(parent).map_err(|error| error.to_string())?;
    }
    let file = CachedModelsDevCatalogFile {
        version: MODELS_DEV_CATALOG_CACHE_VERSION,
        source_url: models_url.to_owned(),
        fetched_at: Some(fetched.fetched_at),
        last_failed_refresh_attempt_at: None,
        models: Some(fetched.models_json.clone()),
    };
    let json = serde_json::to_vec_pretty(&file).map_err(|error| error.to_string())?;
    atomic_write_owner_only(path, &json).map_err(|error| error.to_string())
}

fn record_failed_models_dev_catalog_refresh(
    models_url: &str,
    failed_at: u64,
    path: &Path,
) -> std::result::Result<(), String> {
    if let Some(parent) = path.parent() {
        create_dir_owner_only(parent).map_err(|error| error.to_string())?;
    }
    let mut file = match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str::<CachedModelsDevCatalogFile>(&text)
            .unwrap_or_else(|_| empty_cache_file(models_url)),
        Err(_) => empty_cache_file(models_url),
    };
    if file.version != MODELS_DEV_CATALOG_CACHE_VERSION || file.source_url != models_url {
        file = empty_cache_file(models_url);
    }
    file.last_failed_refresh_attempt_at = Some(failed_at);
    let json = serde_json::to_vec_pretty(&file).map_err(|error| error.to_string())?;
    atomic_write_owner_only(path, &json).map_err(|error| error.to_string())
}

fn empty_cache_file(models_url: &str) -> CachedModelsDevCatalogFile {
    CachedModelsDevCatalogFile {
        version: MODELS_DEV_CATALOG_CACHE_VERSION,
        source_url: models_url.to_owned(),
        fetched_at: None,
        last_failed_refresh_attempt_at: None,
        models: None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ModelsDevCatalogEntry {
    modalities: PromptInputModalities,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct PromptInputModalities {
    known: bool,
    image: bool,
    audio: bool,
    video: bool,
}

impl PromptInputModalities {
    fn mark_known(&mut self) {
        self.known = true;
    }

    fn insert(&mut self, modality: PromptInputModality) {
        match modality {
            PromptInputModality::Image => self.image = true,
            PromptInputModality::Audio => self.audio = true,
            PromptInputModality::Video => self.video = true,
        }
    }

    fn contains(self, modality: PromptInputModality) -> bool {
        match modality {
            PromptInputModality::Image => self.image,
            PromptInputModality::Audio => self.audio,
            PromptInputModality::Video => self.video,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct ModelsDevCatalog {
    by_id: HashMap<String, ModelsDevCatalogEntry>,
    by_lowercase_id: HashMap<String, Vec<ModelsDevCatalogEntry>>,
    by_slug: HashMap<String, Vec<ModelsDevCatalogEntry>>,
    by_lowercase_slug: HashMap<String, Vec<ModelsDevCatalogEntry>>,
}

impl ModelsDevCatalog {
    fn from_value(value: Value) -> serde_json::Result<Self> {
        let raw_models = raw_models_from_value(value)?;
        let mut catalog = Self::default();

        for (key, raw_model) in raw_models {
            let id = raw_model.id.as_deref().unwrap_or(&key).trim().to_owned();
            if id.is_empty() {
                continue;
            }
            let entry = ModelsDevCatalogEntry {
                modalities: raw_model.prompt_input_modalities(),
            };
            catalog.by_id.insert(id.clone(), entry);
            catalog
                .by_lowercase_id
                .entry(id.to_ascii_lowercase())
                .or_default()
                .push(entry);
            catalog
                .by_slug
                .entry(model_slug(&id).to_owned())
                .or_default()
                .push(entry);
            catalog
                .by_lowercase_slug
                .entry(model_slug(&id).to_ascii_lowercase())
                .or_default()
                .push(entry);
        }

        Ok(catalog)
    }

    fn support(&self, model_id: &str, modality: PromptInputModality) -> ModelSupport {
        if let Some(entry) = self.by_id.get(model_id) {
            return support_from_entry(*entry, modality);
        }
        if let Some(entries) = self.by_lowercase_id.get(&model_id.to_ascii_lowercase()) {
            return support_from_consensus(entries, modality);
        }

        let slug = model_slug(model_id);
        if let Some(entries) = self.by_slug.get(slug) {
            return support_from_consensus(entries, modality);
        }
        self.by_lowercase_slug
            .get(&slug.to_ascii_lowercase())
            .map_or(ModelSupport::Unknown, |entries| {
                support_from_consensus(entries, modality)
            })
    }
}

fn raw_models_from_value(value: Value) -> serde_json::Result<Vec<(String, ModelsDevModel)>> {
    match value {
        Value::Object(mut object) => {
            if let Some(models) = object.remove("models") {
                return raw_models_map(models);
            }
            if let Some(data) = object.remove("data") {
                return raw_models_array(data);
            }
            raw_models_map(Value::Object(object))
        }
        other => raw_models_map(other),
    }
}

fn raw_models_map(value: Value) -> serde_json::Result<Vec<(String, ModelsDevModel)>> {
    let raw_models: HashMap<String, ModelsDevModel> = serde_json::from_value(value)?;
    Ok(raw_models.into_iter().collect())
}

fn raw_models_array(value: Value) -> serde_json::Result<Vec<(String, ModelsDevModel)>> {
    let raw_models: Vec<ModelsDevModel> = serde_json::from_value(value)?;
    Ok(raw_models
        .into_iter()
        .enumerate()
        .map(|(index, model)| (index.to_string(), model))
        .collect())
}

fn model_slug(model_id: &str) -> &str {
    model_id.rsplit_once('/').map_or(model_id, |(_, slug)| slug)
}

fn support_from_entry(entry: ModelsDevCatalogEntry, modality: PromptInputModality) -> ModelSupport {
    if !entry.modalities.known {
        return ModelSupport::Unknown;
    }
    if entry.modalities.contains(modality) {
        ModelSupport::Supported
    } else {
        ModelSupport::Unsupported
    }
}

fn support_from_consensus(
    entries: &[ModelsDevCatalogEntry],
    modality: PromptInputModality,
) -> ModelSupport {
    let supported = entries
        .iter()
        .filter(|entry| entry.modalities.known && entry.modalities.contains(modality))
        .count();
    let unsupported = entries
        .iter()
        .filter(|entry| entry.modalities.known && !entry.modalities.contains(modality))
        .count();
    match supported.cmp(&unsupported) {
        std::cmp::Ordering::Greater => ModelSupport::Supported,
        std::cmp::Ordering::Less => ModelSupport::Unsupported,
        std::cmp::Ordering::Equal => ModelSupport::Unknown,
    }
}

#[derive(Debug, Deserialize)]
struct ModelsDevModel {
    id: Option<String>,
    #[serde(default)]
    modalities: ModelsDevModalities,
    #[serde(default)]
    architecture: Option<ModelsDevArchitecture>,
}

impl ModelsDevModel {
    fn prompt_input_modalities(&self) -> PromptInputModalities {
        let mut out = PromptInputModalities::default();
        if !self.modalities.input.is_empty() {
            out.mark_known();
        }
        for modality in &self.modalities.input {
            if let Some(modality) = PromptInputModality::from_models_dev(modality) {
                out.insert(modality);
            }
        }
        if let Some(architecture) = &self.architecture {
            if !architecture.input_modalities.is_empty() {
                out.mark_known();
            }
            for modality in &architecture.input_modalities {
                if let Some(modality) = PromptInputModality::from_models_dev(modality) {
                    out.insert(modality);
                }
            }
        }
        out
    }
}

#[derive(Debug, Default, Deserialize)]
struct ModelsDevModalities {
    #[serde(default)]
    input: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ModelsDevArchitecture {
    #[serde(default)]
    input_modalities: Vec<String>,
}

#[cfg(test)]
mod tests {
    use agent_client_protocol::schema::{
        AudioContent, BlobResourceContents, EmbeddedResource, TextContent,
    };
    use serde_json::json;

    use super::*;

    fn catalog_from_models(models: Value) -> ModelsDevCatalog {
        ModelsDevCatalog::from_value(models).expect("catalog parses")
    }

    fn model(id: &str, input: &[&str]) -> Value {
        json!({
            "id": id,
            "modalities": {
                "input": input,
            },
        })
    }

    #[test]
    fn exact_full_match_reports_modality_support() {
        let catalog = catalog_from_models(json!({
            "moonshotai/kimi-k2.7-code": model("moonshotai/kimi-k2.7-code", &["text", "image"]),
        }));

        assert_eq!(
            catalog.support("moonshotai/kimi-k2.7-code", PromptInputModality::Image),
            ModelSupport::Supported
        );
        assert_eq!(
            catalog.support("moonshotai/kimi-k2.7-code", PromptInputModality::Audio),
            ModelSupport::Unsupported
        );
    }

    #[test]
    fn lowercase_full_match_uses_consensus() {
        let catalog = catalog_from_models(json!({
            "minimax/MiniMax-M3": model("minimax/MiniMax-M3", &["text", "image", "video"]),
        }));

        assert_eq!(
            catalog.support("minimax/minimax-m3", PromptInputModality::Video),
            ModelSupport::Supported
        );
    }

    #[test]
    fn slug_fallback_uses_consensus() {
        let catalog = catalog_from_models(json!({
            "provider-a/shared": model("provider-a/shared", &["text", "image"]),
            "provider-b/shared": model("provider-b/shared", &["text", "image"]),
            "provider-c/shared": model("provider-c/shared", &["text"]),
        }));

        assert_eq!(
            catalog.support("unlisted/shared", PromptInputModality::Image),
            ModelSupport::Supported
        );
    }

    #[test]
    fn slug_fallback_tie_is_unknown() {
        let catalog = catalog_from_models(json!({
            "provider-a/shared": model("provider-a/shared", &["text", "image"]),
            "provider-b/shared": model("provider-b/shared", &["text"]),
        }));

        assert_eq!(
            catalog.support("unlisted/shared", PromptInputModality::Image),
            ModelSupport::Unknown
        );
    }

    #[test]
    fn model_without_input_modalities_is_unknown() {
        let catalog = catalog_from_models(json!({
            "provider/no-data": {
                "id": "provider/no-data"
            }
        }));

        assert_eq!(
            catalog.support("provider/no-data", PromptInputModality::Image),
            ModelSupport::Unknown
        );
    }

    #[test]
    fn slug_fallback_ignores_entries_without_input_modalities() {
        let catalog = catalog_from_models(json!({
            "provider-a/shared": model("provider-a/shared", &["text"]),
            "provider-b/shared": {
                "id": "provider-b/shared"
            },
        }));

        assert_eq!(
            catalog.support("unlisted/shared", PromptInputModality::Image),
            ModelSupport::Unsupported
        );
    }

    #[test]
    fn catalog_accepts_catalog_and_architecture_shapes() {
        let catalog = catalog_from_models(json!({
            "models": {
                "provider/video-model": {
                    "id": "provider/video-model",
                    "architecture": {
                        "input_modalities": ["text", "video"]
                    }
                }
            }
        }));

        assert_eq!(
            catalog.support("provider/video-model", PromptInputModality::Video),
            ModelSupport::Supported
        );
    }

    #[test]
    fn prompt_modalities_include_explicit_and_blob_media() {
        let blocks = vec![
            ContentBlock::Text(TextContent::new("hello")),
            ContentBlock::Image(ImageContent::new("aW1hZ2U=", "image/png")),
            ContentBlock::Audio(AudioContent::new("YXVkaW8=", "audio/mpeg")),
            ContentBlock::Resource(EmbeddedResource::new(
                EmbeddedResourceResource::BlobResourceContents(
                    BlobResourceContents::new("dmlkZW8=", "file:///clip.mp4")
                        .mime_type("video/mp4"),
                ),
            )),
        ];

        let modalities = prompt_required_modalities(&blocks);
        assert!(modalities.contains(&PromptInputModality::Image));
        assert!(modalities.contains(&PromptInputModality::Audio));
        assert!(modalities.contains(&PromptInputModality::Video));
    }

    #[test]
    fn prompt_modalities_ignore_pdf_resource_links_and_unknown_blobs() {
        let blocks = vec![
            ContentBlock::ResourceLink(
                ResourceLink::new("doc", "file:///doc.pdf").mime_type("application/pdf"),
            ),
            ContentBlock::Resource(EmbeddedResource::new(
                EmbeddedResourceResource::BlobResourceContents(
                    BlobResourceContents::new("cGRm", "file:///doc.pdf")
                        .mime_type("application/pdf"),
                ),
            )),
            ContentBlock::Resource(EmbeddedResource::new(
                EmbeddedResourceResource::BlobResourceContents(
                    BlobResourceContents::new("ZGF0YQ==", "file:///data.bin")
                        .mime_type("application/octet-stream"),
                ),
            )),
        ];

        assert!(prompt_required_modalities(&blocks).is_empty());
    }
}
