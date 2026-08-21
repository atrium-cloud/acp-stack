//! Runtime-behavior schema types: permissions, mediated commands, the
//! stale-prompt sweeper, and the local daemon socket.

use super::*;

// CONSTANTS

/// Wire spellings of [`PermissionTimeoutAction`]. Validation matches against
/// these, and [`PermissionsConfig::effective_timeout_action`] parses them, so
/// both stay in step with the enum's derived `Serialize`.
pub(crate) const TIMEOUT_ACTION_DENY: &str = "deny";
pub(crate) const TIMEOUT_ACTION_APPROVE: &str = "approve";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PermissionsConfig {
    /// Mediation policy: `"auto"`, `"supervised"`, or `"locked"`.
    #[schemars(extend("enum" = ["auto", "supervised", "locked"]))]
    pub mode: String,
    #[serde(default)]
    pub review: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_timeout: Option<String>,
    /// Action when a pending permission request expires. Absent leaves the
    /// `"deny"` default.
    // A `String` on the wire, rejected at validate time like its sibling
    // `mode`, so `/v1/config/validate`/`import` keep the actionable
    // "must be one of deny, approve" message (a serde-level enum would fail
    // inside TOML parsing, whose envelope message is deliberately generic).
    // The published schema still gets the typed enum through `with`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<PermissionTimeoutAction>")]
    pub timeout_action: Option<String>,
}

impl Default for PermissionsConfig {
    fn default() -> Self {
        Self {
            mode: "auto".to_owned(),
            review: Vec::new(),
            deny: Vec::new(),
            request_timeout: None,
            timeout_action: None,
        }
    }
}

impl PermissionsConfig {
    pub fn effective_request_timeout(&self) -> std::time::Duration {
        let raw = self
            .request_timeout
            .as_deref()
            .unwrap_or(DEFAULT_PERMISSION_REQUEST_TIMEOUT);
        crate::config::validate::primitives::parse_duration_string(raw).unwrap_or_else(|| {
            crate::config::validate::primitives::parse_duration_string(
                DEFAULT_PERMISSION_REQUEST_TIMEOUT,
            )
            .unwrap_or(std::time::Duration::from_secs(300))
        })
    }

    pub fn effective_timeout_action(&self) -> PermissionTimeoutAction {
        // Unknown strings fall to the default only in unvalidated in-memory
        // configs; every load path validates first and rejects them.
        match self.timeout_action.as_deref() {
            Some(TIMEOUT_ACTION_APPROVE) => PermissionTimeoutAction::Approve,
            _ => DEFAULT_PERMISSION_TIMEOUT_ACTION,
        }
    }
}

/// Expiry action for a pending permission request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum PermissionTimeoutAction {
    Deny,
    Approve,
}

// COMMANDS

pub const DEFAULT_COMMAND_PROGRESS_INTERVAL: &str = "30s";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommandsConfig {
    pub default_timeout: String,
    pub cancel_grace: String,
    #[serde(default = "default_command_progress_interval")]
    pub progress_interval: String,
    #[serde(default)]
    pub env_allowlist: Vec<String>,
    pub max_output_bytes: u64,
}

fn default_command_progress_interval() -> String {
    DEFAULT_COMMAND_PROGRESS_INTERVAL.to_owned()
}

impl Default for CommandsConfig {
    fn default() -> Self {
        Self {
            default_timeout: "10m".to_owned(),
            cancel_grace: "5s".to_owned(),
            progress_interval: default_command_progress_interval(),
            env_allowlist: Vec::new(),
            max_output_bytes: 1_048_576,
        }
    }
}

// PROMPTS

/// Defaults for the stale-prompt sweeper. Tuned for an idle long-running
/// agent: 5 minutes without an ACP `session/update` is well past any
/// reasonable single-token latency, and a 30-second sweep cadence keeps
/// the worst-case "stuck and still listed as running" window bounded
/// without thrashing SQLite. Both values are operator-overridable through
/// `[prompts]` if a deployment streams tokens slowly enough to need a
/// larger threshold.
pub const DEFAULT_PROMPTS_STALE_THRESHOLD: &str = "5m";
pub const DEFAULT_PROMPTS_SWEEP_INTERVAL: &str = "30s";

/// Configuration for the stale-prompt sweeper background task. When no
/// ACP `session/update` notification has touched a `pending`/`running`
/// prompt row for `stale_threshold`, the sweeper flips it to terminal
/// `Stalled` so polling clients always see the row settle. The sweep
/// runs every `sweep_interval` from `acps serve`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PromptsConfig {
    pub stale_threshold: String,
    pub sweep_interval: String,
}

impl Default for PromptsConfig {
    fn default() -> Self {
        Self {
            stale_threshold: DEFAULT_PROMPTS_STALE_THRESHOLD.to_owned(),
            sweep_interval: DEFAULT_PROMPTS_SWEEP_INTERVAL.to_owned(),
        }
    }
}

impl PromptsConfig {
    /// Parsed `stale_threshold`. Falls back to the schema default rather
    /// than panicking — validation already rejected unparsable values at
    /// load time, so this guard only fires for programmatically
    /// constructed configs that bypass `validate_config`.
    pub fn effective_stale_threshold(&self) -> std::time::Duration {
        crate::config::validate::primitives::parse_duration_string(&self.stale_threshold)
            .unwrap_or_else(|| {
                crate::config::validate::primitives::parse_duration_string(
                    DEFAULT_PROMPTS_STALE_THRESHOLD,
                )
                .unwrap_or(std::time::Duration::from_secs(300))
            })
    }

    /// Parsed `sweep_interval`. See `effective_stale_threshold` for the
    /// fallback contract.
    pub fn effective_sweep_interval(&self) -> std::time::Duration {
        crate::config::validate::primitives::parse_duration_string(&self.sweep_interval)
            .unwrap_or_else(|| {
                crate::config::validate::primitives::parse_duration_string(
                    DEFAULT_PROMPTS_SWEEP_INTERVAL,
                )
                .unwrap_or(std::time::Duration::from_secs(30))
            })
    }
}

// LOCAL DAEMON SOCKET

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
pub enum LocalSessionAuth {
    #[serde(rename = "session-key")]
    #[default]
    SessionKey,
    #[serde(rename = "keyless")]
    Keyless,
}

impl LocalSessionAuth {
    pub fn is_default(value: &Self) -> bool {
        *value == Self::default()
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::SessionKey => "session-key",
            Self::Keyless => "keyless",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LocalConfig {
    /// Override path for the internal local Unix-domain socket. When unset the
    /// daemon binds `~/.local/share/acp-stack/acps-local.sock`. Override is
    /// intended for integration tests; deployed instances should leave it
    /// unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket_path: Option<String>,
    /// Controls whether local Unix-socket session-tier HTTP routes require an
    /// explicit session key. Public HTTP tiering is unaffected.
    #[serde(default, skip_serializing_if = "LocalSessionAuth::is_default")]
    pub session_auth: LocalSessionAuth,
}
