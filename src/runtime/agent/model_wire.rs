//! Per-model wire-shape mapping for providers whose model listings carry no
//! wire metadata (see `data/endpoints.toml`'s header for provenance and the
//! refresh ritual).
//!
//! The mapping is embedded data, not Rust control flow, mirroring
//! `provider_keys`: runtime code only parses, validates, and queries it.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use serde::Deserialize;

use crate::error::{Result, StackError};
use crate::runtime::agent::provider_keys::provider_id_is_known;

const EMBEDDED_ENDPOINTS: &str = include_str!("../../../data/endpoints.toml");

static MODEL_WIRE_MAPPING: LazyLock<ModelWireMapping> = LazyLock::new(|| {
    ModelWireMapping::from_toml(EMBEDDED_ENDPOINTS).expect("valid model wire mapping")
});

/// Agent-neutral wire a model speaks on a given provider. Hermes transports
/// and other agent vocabularies are derived from this, never stored here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelWire {
    ChatCompletions,
    AnthropicMessages,
    Responses,
    Google,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelWireMapping {
    providers: BTreeMap<String, ProviderWireTable>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProviderWireTable {
    models: BTreeMap<String, ModelWire>,
}

#[derive(Debug, Deserialize)]
struct RawProviderWireTable {
    // Parsed (and thereby validated as a known wire) but not stored: the
    // provisioning fallback lives in providers.toml, so keeping a second copy
    // here would invite drift.
    #[allow(dead_code)]
    default: ModelWire,
    #[serde(default)]
    models: BTreeMap<String, ModelWire>,
}

impl ModelWireMapping {
    pub fn load_embedded() -> &'static Self {
        &MODEL_WIRE_MAPPING
    }

    pub fn from_toml(body: &str) -> Result<Self> {
        let raw: BTreeMap<String, RawProviderWireTable> =
            toml::from_str(body).map_err(|source| StackError::RegistryLoad {
                reason: format!("model wire mapping TOML is invalid: {source}"),
            })?;
        let mut providers = BTreeMap::new();
        for (provider_id, table) in raw {
            if !provider_id_is_known(&provider_id) {
                return Err(StackError::RegistryLoad {
                    reason: format!(
                        "model wire mapping references unknown provider `{provider_id}`"
                    ),
                });
            }
            for model_id in table.models.keys() {
                if model_id.trim().is_empty() {
                    return Err(StackError::RegistryLoad {
                        reason: format!(
                            "model wire mapping for provider `{provider_id}` has an empty model id"
                        ),
                    });
                }
                if model_id.trim() != model_id {
                    return Err(StackError::RegistryLoad {
                        reason: format!(
                            "model wire mapping model id `{model_id}` has surrounding whitespace"
                        ),
                    });
                }
            }
            providers.insert(
                provider_id,
                ProviderWireTable {
                    models: table.models,
                },
            );
        }
        Ok(Self { providers })
    }

    /// The wire `model_id` speaks on `provider_id`. `None` means "no entry"
    /// — an unlisted model, or a provider with no table at all — so the
    /// caller falls back to the provider-level default.
    pub fn model_wire(&self, provider_id: &str, model_id: &str) -> Option<ModelWire> {
        self.providers
            .get(provider_id)?
            .models
            .get(model_id)
            .copied()
    }
}

/// The wire `model_id` speaks on `provider_id`, per the embedded mapping.
pub fn model_wire(provider_id: &str, model_id: &str) -> Option<ModelWire> {
    ModelWireMapping::load_embedded().model_wire(provider_id, model_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_mapping_loads_and_validates() {
        let mapping = ModelWireMapping::from_toml(EMBEDDED_ENDPOINTS).expect("mapping parses");

        assert!(!mapping.providers.is_empty());
    }

    #[test]
    fn model_wire_lookups_are_data_driven() {
        assert_eq!(
            model_wire("opencode", "claude-opus-5"),
            Some(ModelWire::AnthropicMessages)
        );
        assert_eq!(
            model_wire("opencode", "gpt-5.5"),
            Some(ModelWire::Responses)
        );
        assert_eq!(
            model_wire("opencode", "gemini-3-flash"),
            Some(ModelWire::Google)
        );
        assert_eq!(
            model_wire("opencode-go", "minimax-m3"),
            Some(ModelWire::AnthropicMessages)
        );
        assert_eq!(
            model_wire("opencode-go", "grok-4.5"),
            Some(ModelWire::Responses)
        );
        // Unlisted models (the default wire needs no entry) and unknown
        // providers both resolve to None so the caller falls back.
        assert_eq!(model_wire("opencode", "glm-5.2"), None);
        assert_eq!(model_wire("opencode", "brand-new-model"), None);
        assert_eq!(model_wire("openai", "gpt-5.5"), None);
    }

    #[test]
    fn invalid_mapping_rejects_unknown_wire_value() {
        let err = ModelWireMapping::from_toml(
            r#"
[opencode]
default = "chat_completions"

[opencode.models]
"some-model" = "carrier_pigeon"
"#,
        )
        .expect_err("unknown wire value fails");

        assert!(
            err.to_string()
                .contains("model wire mapping TOML is invalid")
        );
    }

    #[test]
    fn invalid_mapping_rejects_unknown_provider_id() {
        let err = ModelWireMapping::from_toml(
            r#"
[not-a-provider]
default = "chat_completions"

[not-a-provider.models]
"some-model" = "responses"
"#,
        )
        .expect_err("unknown provider id fails");

        assert!(
            err.to_string()
                .contains("unknown provider `not-a-provider`")
        );
    }

    #[test]
    fn invalid_mapping_rejects_empty_model_id() {
        let err = ModelWireMapping::from_toml(
            r#"
[opencode]
default = "chat_completions"

[opencode.models]
"" = "responses"
"#,
        )
        .expect_err("empty model id fails");

        assert!(err.to_string().contains("empty model id"));
    }
}
