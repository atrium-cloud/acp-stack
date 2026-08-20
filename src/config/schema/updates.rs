//! Self-update schema types for the `acp-stack` binary itself.

use super::*;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdatesConfig {
    #[serde(default = "default_stack_update_config")]
    pub acp_stack: StackUpdateConfig,
}

impl Default for UpdatesConfig {
    fn default() -> Self {
        Self {
            acp_stack: default_stack_update_config(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StackUpdateConfig {
    #[serde(default = "default_stack_update_policy")]
    pub policy: StackUpdatePolicy,
    #[serde(default = "default_stack_update_frequency")]
    pub frequency: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum StackUpdatePolicy {
    Compatible,
    SecurityCritical,
    Manual,
}

fn default_stack_update_config() -> StackUpdateConfig {
    StackUpdateConfig {
        policy: default_stack_update_policy(),
        frequency: default_stack_update_frequency(),
    }
}

fn default_stack_update_policy() -> StackUpdatePolicy {
    DEFAULT_STACK_UPDATE_POLICY
}

fn default_stack_update_frequency() -> String {
    DEFAULT_STACK_UPDATE_FREQUENCY.to_owned()
}
