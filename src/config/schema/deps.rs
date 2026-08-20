//! Declared dependency checks and their optional install actions.

use super::*;

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DependenciesConfig {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<DependencyEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub packages: Vec<DependencyEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runtimes: Vec<DependencyEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp: Vec<DependencyEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DependencyEntry {
    pub name: String,
    #[serde(default = "default_dependency_required")]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feature: Option<String>,
    /// Optional install action for `acps deps apply`. When absent, the
    /// command is "check-only" and `acps deps apply` will report it as
    /// not actionable rather than guessing a package manager. This
    /// keeps Dependency Apply narrowly scoped per the Phase 4 spec:
    /// no cross-distro reconciliation, no auto-derived package names —
    /// the operator declares each install action explicitly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install: Option<DependencyInstallAction>,
}

fn default_dependency_required() -> bool {
    true
}

/// Operator-declared install action for one dependency. Intentionally
/// minimal: a single shell snippet, an optional `creates` postcheck,
/// and a scope marker that distinguishes "runs as the runtime user"
/// from "needs OS-wide privilege" so the apply runner knows when to
/// escalate — and never silently downgrades privileged work to user
/// scope behind the operator's back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DependencyInstallAction {
    /// Shell snippet executed via `[workspace].default_shell -c`.
    /// Operator declares it verbatim — no apt/brew/yum derivation in
    /// the runtime.
    pub shell: String,
    /// PATH name that must resolve to an executable after `shell`
    /// completes. Defaults to the dependency entry's `name`. The apply
    /// runner records `available = true` only when this resolves
    /// post-install.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creates: Option<String>,
    /// `user` (default) runs as the runtime user; `system` declares
    /// the action needs OS-wide privilege (typically sudo). The
    /// runner emits a clear distinction in the audit log and a
    /// confirmation prompt for `system` scope so operators don't
    /// invoke `apt-get install` from a stale CLI invocation.
    #[serde(default)]
    pub scope: DependencyInstallScope,
    /// Optional timeout override in seconds. Defaults to 600s
    /// (10 minutes) — same cap as the agent installer. Bounded above at
    /// validation by `MAX_INSTALL_TIMEOUT_SECS`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum DependencyInstallScope {
    /// Runs as the runtime user. No privilege escalation. Suitable
    /// for npm globals under `~/.local/`, language toolchains in
    /// $HOME, etc.
    #[default]
    User,
    /// Action needs OS-wide privilege (system package manager, writes
    /// under /usr or /opt). Runs directly when the process is root,
    /// escalates through passwordless `sudo -n` when it isn't (never a
    /// password prompt), and otherwise refuses early with a clear
    /// "privilege required" outcome — the runner never falls back to
    /// user scope.
    System,
}
