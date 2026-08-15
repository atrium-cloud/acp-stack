//! Machine-readable state signals emitted by the init flow.
//!
//! The wizard runs on one thread and knows things no observer can reconstruct
//! from the progress text: which durable step it is inside, which categories
//! the registry says this agent even has, and what the live capability probe
//! contradicted. These signals carry exactly that, so a hosted driver can hold
//! the category map without parsing prose. Off-hosted runs never build one —
//! `prompt::emit_state_signal` takes a closure and drops it when no driver is
//! installed.
//!
//! Step kinds ride as the `init_runner::step_kind` constants verbatim rather
//! than a parallel enum, so the vocabulary cannot drift from what is persisted
//! in `init_steps.kind`.

use crate::runtime::init_runner::{StepDisposition, step_kind};

/// The nine things init can settle. `id` is shared wire surface with hosted
/// clients (it is the `id` of a category in the `state` frame), so a rename is
/// a wire break.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum InitCategory {
    Agent,
    Provider,
    Model,
    Mode,
    Workspace,
    NativeConfig,
    Mcp,
    Skills,
    Deps,
}

impl InitCategory {
    pub(super) fn id(self) -> &'static str {
        match self {
            InitCategory::Agent => "agent",
            InitCategory::Provider => "provider",
            InitCategory::Model => "model",
            InitCategory::Mode => "mode",
            InitCategory::Workspace => "workspace",
            InitCategory::NativeConfig => "native_config",
            InitCategory::Mcp => "mcp",
            InitCategory::Skills => "skills",
            InitCategory::Deps => "deps",
        }
    }
}

/// What decided a category's applicability. The ordering is authority, not
/// chronology: a live `Probe` or `Discovery` verdict overrides whatever the
/// registry claimed, because the installed harness is the ground truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ApplicabilitySource {
    Args,
    Registry,
    Probe,
    /// `session/new` config_options corrections, ranked above the registry:
    /// the installed harness is what a session will actually accept.
    Discovery,
    /// The live check could not be made at all — a provisional session the
    /// harness would not complete. It still outranks the registry, since the
    /// lane demonstrably cannot be driven this run, but it is the absence of
    /// evidence rather than evidence of absence, so it may not withdraw a lane
    /// the config already holds a value for.
    DiscoveryUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum InitStateSignal {
    StepStarted {
        kind: &'static str,
    },
    StepFinished {
        kind: &'static str,
        disposition: StepDisposition,
        /// `StackError::error_code` returns a borrowed `&str`, so the signal
        /// owns its copy rather than tying the signal's lifetime to the error.
        error_code: Option<String>,
    },
    CategoryApplicability {
        category: InitCategory,
        applicable: bool,
        source: ApplicabilitySource,
        reason: Option<String>,
    },
    CategorySettled {
        category: InitCategory,
        value: Option<String>,
    },
    /// A value that was already in config when the run started, reported so a
    /// resumed or fully declared run does not show its harness lanes as
    /// configured-with-nothing. Indistinguishable from `CategorySettled` on the
    /// wire; it differs only in that live probe or discovery evidence may still
    /// withdraw the lane.
    CategoryProvisionallySettled {
        category: InitCategory,
        value: String,
    },
    CategoryFailed {
        category: InitCategory,
        code: String,
    },
}

/// The category a step's failure badges. Steps with no category (auth, edge
/// artifacts, headless config, testflight, init_complete) surface only as
/// `current_step`.
pub(super) fn category_for_step_kind(kind: &str) -> Option<InitCategory> {
    match kind {
        step_kind::AGENT_INSTALL => Some(InitCategory::Agent),
        step_kind::NATIVE_CONFIG_IMPORT => Some(InitCategory::NativeConfig),
        step_kind::AGENT_SKILLS_INSTALL => Some(InitCategory::Skills),
        step_kind::WORKSPACE_MATERIALIZE => Some(InitCategory::Workspace),
        step_kind::DEPS_APPLY => Some(InitCategory::Deps),
        step_kind::MCP_CONFIGURE => Some(InitCategory::Mcp),
        step_kind::PROVIDER_CONFIGURE => Some(InitCategory::Provider),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn category_ids_are_the_canonical_wire_strings() {
        let ordered = [
            (InitCategory::Agent, "agent"),
            (InitCategory::Provider, "provider"),
            (InitCategory::Model, "model"),
            (InitCategory::Mode, "mode"),
            (InitCategory::Workspace, "workspace"),
            (InitCategory::NativeConfig, "native_config"),
            (InitCategory::Mcp, "mcp"),
            (InitCategory::Skills, "skills"),
            (InitCategory::Deps, "deps"),
        ];
        for (category, id) in ordered {
            assert_eq!(category.id(), id);
        }
        let unique = ordered
            .iter()
            .map(|(_, id)| *id)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(unique.len(), ordered.len(), "category ids must be unique");
    }

    #[test]
    fn step_kinds_map_to_the_category_they_settle() {
        let mapped = [
            (step_kind::AGENT_INSTALL, InitCategory::Agent),
            (step_kind::NATIVE_CONFIG_IMPORT, InitCategory::NativeConfig),
            (step_kind::AGENT_SKILLS_INSTALL, InitCategory::Skills),
            (step_kind::WORKSPACE_MATERIALIZE, InitCategory::Workspace),
            (step_kind::DEPS_APPLY, InitCategory::Deps),
            (step_kind::MCP_CONFIGURE, InitCategory::Mcp),
            (step_kind::PROVIDER_CONFIGURE, InitCategory::Provider),
        ];
        for (kind, category) in mapped {
            assert_eq!(category_for_step_kind(kind), Some(category), "kind {kind}");
        }
    }

    #[test]
    fn steps_without_a_category_surface_only_as_current_step() {
        for kind in [
            step_kind::CONFIG_VALIDATE,
            step_kind::STATE_INIT,
            step_kind::SECRETS_INIT,
            step_kind::CAPABILITY_PROBE,
            step_kind::AGENT_HEADLESS_CONFIG,
            step_kind::EDGE_ARTIFACTS,
            step_kind::INIT_COMPLETE,
            step_kind::TESTFLIGHT,
        ] {
            assert_eq!(category_for_step_kind(kind), None, "kind {kind}");
        }
        assert_eq!(category_for_step_kind("not_a_step"), None);
    }
}
