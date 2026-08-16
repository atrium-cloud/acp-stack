//! Agent, agent-array, provider, and agent-install schema types.

use super::*;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArrayConfig {
    #[serde(default)]
    pub enabled: bool,
    pub primary_target: String,
    #[serde(default)]
    pub targets: Vec<ArrayTargetConfig>,
}

impl ArrayConfig {
    pub fn from_agent(agent: AgentConfig) -> Self {
        let target_id = agent.id.clone();
        Self {
            enabled: false,
            primary_target: target_id.clone(),
            targets: vec![ArrayTargetConfig {
                id: target_id,
                agent,
            }],
        }
    }

    pub fn primary_target(&self) -> Option<&ArrayTargetConfig> {
        self.targets
            .iter()
            .find(|target| target.id == self.primary_target)
    }

    pub fn primary_target_mut(&mut self) -> Option<&mut ArrayTargetConfig> {
        self.targets
            .iter_mut()
            .find(|target| target.id == self.primary_target)
    }

    pub fn target(&self, target_id: &str) -> Option<&ArrayTargetConfig> {
        self.targets.iter().find(|target| target.id == target_id)
    }

    pub fn target_mut(&mut self, target_id: &str) -> Option<&mut ArrayTargetConfig> {
        self.targets
            .iter_mut()
            .find(|target| target.id == target_id)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArrayTargetConfig {
    pub id: String,
    pub agent: AgentConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    pub id: String,
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_sha256: Option<String>,
    pub restart: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Pin the harness to a specific GitHub Release tag (e.g. `"v0.42.0"`).
    /// Consulted by both install and managed update when the harness install
    /// path is `github_release`; updates target the pin instead of the latest
    /// release. Default (None) resolves the latest release.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_version: Option<String>,
    /// Adapter metadata is runtime-populated from the embedded registry,
    /// never operator-written. `skip_deserializing` rejects any operator
    /// who carried a `[agent.adapter]` block over from a pre-rework config.
    #[serde(default, skip_deserializing, skip_serializing)]
    pub adapter: Option<AgentAdapterConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<AgentProviderConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub providers: Option<AgentProvidersConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent: Option<AgentSubagentConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_update: Option<AgentAutoUpdateConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub install: Option<AgentInstallConfig>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentProvidersConfig {
    #[serde(default)]
    pub active: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub selected_aliases: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentAutoUpdateConfig {
    #[serde(default, skip_serializing_if = "is_false")]
    pub enabled: bool,
    pub frequency: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSubagentConfig {
    #[serde(default, skip_serializing_if = "is_false")]
    pub disabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<AgentProviderConfig>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentProviderConfig {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom: Option<AgentCustomProviderConfig>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentCustomProviderConfig {
    pub name: String,
    pub base_url: String,
    #[serde(default)]
    pub api: CustomProviderApi,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    #[serde(default = "default_custom_model_context")]
    pub context: u64,
    #[serde(default = "default_custom_model_output_max_tokens")]
    pub output_max_tokens: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CustomProviderApi {
    #[default]
    ChatCompletions,
    Responses,
    AnthropicMessages,
}

impl CustomProviderApi {
    pub fn as_pi_api(self) -> &'static str {
        match self {
            Self::ChatCompletions => "openai-completions",
            Self::Responses => "openai-responses",
            Self::AnthropicMessages => "anthropic-messages",
        }
    }

    pub fn as_codex_wire_api(self) -> &'static str {
        match self {
            Self::Responses => "responses",
            Self::ChatCompletions => "chat",
            Self::AnthropicMessages => "anthropic",
        }
    }
}

fn default_custom_model_context() -> u64 {
    DEFAULT_CUSTOM_MODEL_CONTEXT
}

fn default_custom_model_output_max_tokens() -> u64 {
    DEFAULT_CUSTOM_MODEL_OUTPUT_MAX_TOKENS
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentAdapterConfig {
    pub id: String,
    pub name: String,
    pub upstream_agent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
}

/// Operator-facing escape hatch for installing an agent whose entry is not
/// in the embedded registry (private fork, unreleased build, custom adapter).
/// The runtime resolves registry-listed agents from `data/agents.toml`
/// keyed off `[agent].id`; this struct is consulted only when the operator
/// explicitly writes `[agent.install]` to override that resolution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentInstallConfig {
    #[serde(rename = "type")]
    pub install_type: String,
    pub creates: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
}
