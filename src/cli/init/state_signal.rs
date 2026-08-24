//! Machine-readable state signals emitted by the init flow, forwarded by a hosted
//! driver as `signal` events. Step kinds ride as the `init_runner::step_kind`
//! constants verbatim so the vocabulary cannot drift from `init_steps.kind`.

use serde_json::{Map, Value};

use crate::runtime::init_runner::StepDisposition;
#[cfg(test)]
use crate::runtime::init_runner::step_kind;

/// The things init can settle. `id` is shared wire surface, so a rename is a break.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum InitCategory {
    Agent,
    Provider,
    Model,
    Mode,
    Effort,
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
            InitCategory::Effort => "effort",
            InitCategory::Workspace => "workspace",
            InitCategory::NativeConfig => "native_config",
            InitCategory::Mcp => "mcp",
            InitCategory::Skills => "skills",
            InitCategory::Deps => "deps",
        }
    }
}

/// What decided a category's applicability. The ordering is authority, not chronology:
/// a live `Probe` or `Discovery` verdict overrides whatever the registry claimed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ApplicabilitySource {
    Args,
    Registry,
    Probe,
    /// `session/new` config_options corrections, ranked above the registry.
    Discovery,
    /// The live check could not be made at all. Outranks the registry, but as absence
    /// of evidence it may not withdraw a lane the config already holds a value for.
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
    /// A value already in config when the run started. Differs from `CategorySettled`
    /// only in that live probe or discovery evidence may still withdraw the lane.
    CategoryProvisionallySettled {
        category: InitCategory,
        value: String,
    },
    CategoryFailed {
        category: InitCategory,
        code: String,
    },
}

/// The category a step's failure badges; steps with no category surface only as
/// `current_step`.
#[cfg(test)]
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

impl ApplicabilitySource {
    /// Wire token for the `source` field of a `category_applicability` signal; the
    /// client folds authority off this string, so a rename is a wire break.
    fn wire(self) -> &'static str {
        match self {
            ApplicabilitySource::Args => "args",
            ApplicabilitySource::Registry => "registry",
            ApplicabilitySource::Probe => "probe",
            ApplicabilitySource::Discovery => "discovery",
            ApplicabilitySource::DiscoveryUnavailable => "discovery_unavailable",
        }
    }
}

impl InitStateSignal {
    /// The `signal` event payload. Each signal rides the wire verbatim; folding these
    /// facts into a rendered category view is the client's job, not the instance's.
    pub(super) fn wire_payload(&self) -> Map<String, Value> {
        let mut payload = Map::new();
        match self {
            InitStateSignal::StepStarted { kind } => {
                payload.insert(
                    "signal".to_owned(),
                    Value::String("step_started".to_owned()),
                );
                payload.insert("step".to_owned(), Value::String((*kind).to_owned()));
            }
            InitStateSignal::StepFinished {
                kind,
                disposition,
                error_code,
            } => {
                payload.insert(
                    "signal".to_owned(),
                    Value::String("step_finished".to_owned()),
                );
                payload.insert("step".to_owned(), Value::String((*kind).to_owned()));
                payload.insert(
                    "disposition".to_owned(),
                    Value::String(
                        match disposition {
                            StepDisposition::Executed => "executed",
                            StepDisposition::Background => "background",
                            StepDisposition::Skipped => "skipped",
                        }
                        .to_owned(),
                    ),
                );
                if let Some(code) = error_code {
                    payload.insert("error_code".to_owned(), Value::String(code.clone()));
                }
            }
            InitStateSignal::CategoryApplicability {
                category,
                applicable,
                source,
                reason,
            } => {
                payload.insert(
                    "signal".to_owned(),
                    Value::String("category_applicability".to_owned()),
                );
                payload.insert(
                    "category".to_owned(),
                    Value::String(category.id().to_owned()),
                );
                payload.insert("applicable".to_owned(), Value::Bool(*applicable));
                payload.insert("source".to_owned(), Value::String(source.wire().to_owned()));
                if let Some(reason) = reason {
                    payload.insert("reason".to_owned(), Value::String(reason.clone()));
                }
            }
            InitStateSignal::CategorySettled { category, value } => {
                payload.insert(
                    "signal".to_owned(),
                    Value::String("category_settled".to_owned()),
                );
                payload.insert(
                    "category".to_owned(),
                    Value::String(category.id().to_owned()),
                );
                // Settled-with-nothing is distinct from an unsettled lane, so a null
                // value rides rather than being omitted.
                payload.insert(
                    "value".to_owned(),
                    value.clone().map_or(Value::Null, Value::String),
                );
            }
            InitStateSignal::CategoryProvisionallySettled { category, value } => {
                payload.insert(
                    "signal".to_owned(),
                    Value::String("category_provisionally_settled".to_owned()),
                );
                payload.insert(
                    "category".to_owned(),
                    Value::String(category.id().to_owned()),
                );
                payload.insert("value".to_owned(), Value::String(value.clone()));
            }
            InitStateSignal::CategoryFailed { category, code } => {
                payload.insert(
                    "signal".to_owned(),
                    Value::String("category_failed".to_owned()),
                );
                payload.insert(
                    "category".to_owned(),
                    Value::String(category.id().to_owned()),
                );
                payload.insert("code".to_owned(), Value::String(code.clone()));
            }
        }
        payload
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
            (InitCategory::Effort, "effort"),
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

    #[test]
    fn step_finished_dispositions_use_the_canonical_wire_strings() {
        for (disposition, wire) in [
            (StepDisposition::Executed, "executed"),
            (StepDisposition::Background, "background"),
            (StepDisposition::Skipped, "skipped"),
        ] {
            let payload = InitStateSignal::StepFinished {
                kind: step_kind::DEPS_APPLY,
                disposition,
                error_code: None,
            }
            .wire_payload();
            assert_eq!(payload["signal"], "step_finished");
            assert_eq!(payload["step"], "deps_apply");
            assert_eq!(payload["disposition"], wire);
        }
    }
}
