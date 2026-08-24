//! Reference client fold for the init `signal` stream, and the derivation a real client ports.
//!
//! This oracle panics on an unknown category id, applicability source, or signal name because it
//! pins the exact current contract; a ported client MUST instead ignore unrecognized values, per
//! the forward-compat rule in api.md, so the stream can gain variants.

use serde_json::{Map, Value, json};

use super::super::*;
use crate::cli::init::prompt::ALL_HOSTED_PROMPT_KINDS;
use crate::cli::init::state_signal::{ApplicabilitySource, InitCategory};

/// Canonical wire order, and a topological order of the dependency table below, which is what
/// lets `derive_snapshot` resolve `blocked_on` in a single forward pass.
const CATEGORY_ORDER: [InitCategory; CATEGORY_COUNT] = [
    InitCategory::Agent,
    InitCategory::Provider,
    InitCategory::Model,
    InitCategory::Mode,
    InitCategory::Effort,
    InitCategory::Workspace,
    InitCategory::NativeConfig,
    InitCategory::Mcp,
    InitCategory::Skills,
    InitCategory::Deps,
];

const CATEGORY_COUNT: usize = 10;

const STATUS_NOT_APPLICABLE: &str = "not_applicable";
const STATUS_FAILED: &str = "failed";
const STATUS_AWAITING_INPUT: &str = "awaiting_input";
const STATUS_SETTLED: &str = "settled";
const STATUS_BLOCKED: &str = "blocked";
const STATUS_READY: &str = "ready";

fn dependency(category: InitCategory) -> Option<InitCategory> {
    match category {
        InitCategory::Agent => None,
        InitCategory::Provider => Some(InitCategory::Agent),
        InitCategory::Model => Some(InitCategory::Provider),
        InitCategory::Mode => Some(InitCategory::Model),
        InitCategory::Effort => Some(InitCategory::Model),
        InitCategory::Workspace => None,
        InitCategory::NativeConfig => None,
        InitCategory::Mcp => Some(InitCategory::Agent),
        InitCategory::Skills => Some(InitCategory::Agent),
        InitCategory::Deps => None,
    }
}

fn slot(category: InitCategory) -> usize {
    match category {
        InitCategory::Agent => 0,
        InitCategory::Provider => 1,
        InitCategory::Model => 2,
        InitCategory::Mode => 3,
        InitCategory::Effort => 4,
        InitCategory::Workspace => 5,
        InitCategory::NativeConfig => 6,
        InitCategory::Mcp => 7,
        InitCategory::Skills => 8,
        InitCategory::Deps => 9,
    }
}

fn category_from_id(id: &str) -> InitCategory {
    CATEGORY_ORDER
        .into_iter()
        .find(|category| category.id() == id)
        .unwrap_or_else(|| panic!("unknown category id `{id}` in signal stream"))
}

fn source_from_wire(source: &str) -> ApplicabilitySource {
    match source {
        "args" => ApplicabilitySource::Args,
        "registry" => ApplicabilitySource::Registry,
        "probe" => ApplicabilitySource::Probe,
        "discovery" => ApplicabilitySource::Discovery,
        "discovery_unavailable" => ApplicabilitySource::DiscoveryUnavailable,
        other => panic!("unknown applicability source `{other}` in signal stream"),
    }
}

#[derive(Clone, PartialEq, Eq)]
enum CategoryOutcome {
    Settled {
        value: Option<String>,
        provisional: bool,
    },
    Failed {
        code: String,
    },
}

#[derive(Clone)]
struct CategoryEntry {
    applicable: bool,
    applicability_source: Option<ApplicabilitySource>,
    applicability_reason: Option<String>,
    outcome: Option<CategoryOutcome>,
}

impl Default for CategoryEntry {
    fn default() -> Self {
        Self {
            applicable: true,
            applicability_source: None,
            applicability_reason: None,
            outcome: None,
        }
    }
}

#[derive(Default)]
struct CategoryMap([CategoryEntry; CATEGORY_COUNT]);

impl CategoryMap {
    fn entry(&self, category: InitCategory) -> &CategoryEntry {
        &self.0[slot(category)]
    }

    fn entry_mut(&mut self, category: InitCategory) -> &mut CategoryEntry {
        &mut self.0[slot(category)]
    }

    fn set_applicability(
        &mut self,
        category: InitCategory,
        applicable: bool,
        source: ApplicabilitySource,
        reason: Option<String>,
    ) {
        let entry = self.entry_mut(category);
        if source == ApplicabilitySource::Registry
            && matches!(
                entry.applicability_source,
                Some(
                    ApplicabilitySource::Probe
                        | ApplicabilitySource::Discovery
                        | ApplicabilitySource::DiscoveryUnavailable
                )
            )
        {
            return;
        }
        if !applicable {
            match entry.outcome {
                Some(CategoryOutcome::Settled {
                    provisional: true, ..
                }) if matches!(
                    source,
                    ApplicabilitySource::Probe | ApplicabilitySource::Discovery
                ) =>
                {
                    entry.outcome = None
                }
                Some(_) => return,
                None => {}
            }
        }
        entry.applicable = applicable;
        entry.applicability_source = Some(source);
        entry.applicability_reason = reason;
    }

    fn settle(&mut self, category: InitCategory, value: Option<String>) {
        self.entry_mut(category).outcome = Some(CategoryOutcome::Settled {
            value,
            provisional: false,
        });
    }

    fn settle_provisional(&mut self, category: InitCategory, value: String) {
        self.entry_mut(category).outcome = Some(CategoryOutcome::Settled {
            value: Some(value),
            provisional: true,
        });
    }

    fn fail(&mut self, category: InitCategory, code: String) {
        self.entry_mut(category).outcome = Some(CategoryOutcome::Failed { code });
    }

    fn fail_step_category(&mut self, category: InitCategory, code: String) {
        let already_blamed = self.0.iter().any(|entry| {
            matches!(&entry.outcome, Some(CategoryOutcome::Failed { code: blamed }) if *blamed == code)
        });
        if already_blamed || !self.entry(category).applicable {
            return;
        }
        self.fail(category, code);
    }

    fn settle_unresolved(&mut self, category: InitCategory) {
        if self.entry(category).outcome.is_none() {
            self.settle(category, None);
        }
    }

    fn settle_remaining(&mut self) {
        for entry in &mut self.0 {
            if entry.applicable && entry.outcome.is_none() {
                entry.outcome = Some(CategoryOutcome::Settled {
                    value: None,
                    provisional: false,
                });
            }
        }
    }
}

/// Map a pending prompt's wire `kind` to the category awaiting it; the client derives this from
/// `pending_input` rather than receiving it.
pub(super) fn awaiting_category(pending_kind: Option<&str>) -> Option<InitCategory> {
    let kind = pending_kind?;
    ALL_HOSTED_PROMPT_KINDS
        .iter()
        .find(|prompt_kind| prompt_kind.as_str() == kind)
        .and_then(|prompt_kind| prompt_kind.category())
}

/// Fold the ordered raw `signal` payloads into the category view.
pub(super) fn fold_state(signals: &[Value], awaiting: Option<InitCategory>) -> Value {
    let mut map = CategoryMap::default();
    let mut current_step: Option<String> = None;

    for signal in signals {
        let name = signal["signal"].as_str().unwrap_or_default();
        match name {
            "step_started" => {
                current_step = signal["step"].as_str().map(str::to_owned);
            }
            "step_finished" => {
                let step = signal["step"].as_str().unwrap_or_default();
                let error_code = signal["error_code"].as_str().map(str::to_owned);
                if let Some(category) = category_for_step_kind(step) {
                    match &error_code {
                        Some(code) => map.fail_step_category(category, code.clone()),
                        None => map.settle_unresolved(category),
                    }
                }
                if step == step_kind::INIT_COMPLETE && error_code.is_none() {
                    map.settle_remaining();
                }
            }
            "category_applicability" => {
                let category = category_from_id(signal["category"].as_str().unwrap_or_default());
                let applicable = signal["applicable"].as_bool().unwrap_or(true);
                let source = source_from_wire(signal["source"].as_str().unwrap_or_default());
                let reason = signal["reason"].as_str().map(str::to_owned);
                map.set_applicability(category, applicable, source, reason);
            }
            "category_settled" => {
                let category = category_from_id(signal["category"].as_str().unwrap_or_default());
                let value = signal["value"].as_str().map(str::to_owned);
                map.settle(category, value);
            }
            "category_provisionally_settled" => {
                let category = category_from_id(signal["category"].as_str().unwrap_or_default());
                let value = signal["value"].as_str().unwrap_or_default().to_owned();
                map.settle_provisional(category, value);
            }
            "category_failed" => {
                let category = category_from_id(signal["category"].as_str().unwrap_or_default());
                let code = signal["code"].as_str().unwrap_or_default().to_owned();
                map.fail(category, code);
            }
            other => panic!("unknown signal `{other}` in stream"),
        }
    }

    derive_snapshot(&map, current_step.as_deref(), awaiting)
}

fn derive_snapshot(
    categories: &CategoryMap,
    current_step: Option<&str>,
    awaiting: Option<InitCategory>,
) -> Value {
    let mut derived: Vec<(InitCategory, Map<String, Value>)> = Vec::with_capacity(CATEGORY_COUNT);
    for category in CATEGORY_ORDER {
        let entry = categories.entry(category);
        let mut state = category_object(category);
        if let Some(CategoryOutcome::Failed { code }) = &entry.outcome {
            state.insert("status".to_owned(), json!(STATUS_FAILED));
            state.insert("code".to_owned(), json!(code));
        } else if !entry.applicable {
            state.insert("status".to_owned(), json!(STATUS_NOT_APPLICABLE));
            if let Some(reason) = &entry.applicability_reason {
                state.insert("reason".to_owned(), json!(reason));
            }
        } else if awaiting == Some(category) {
            state.insert("status".to_owned(), json!(STATUS_AWAITING_INPUT));
        } else if let Some(CategoryOutcome::Settled { value, .. }) = &entry.outcome {
            state.insert("status".to_owned(), json!(STATUS_SETTLED));
            if let Some(value) = value {
                state.insert("value".to_owned(), json!(value));
            }
        } else if let Some(blocker) =
            dependency(category).filter(|blocker| !resolved(&derived, *blocker))
        {
            state.insert("status".to_owned(), json!(STATUS_BLOCKED));
            state.insert("blocked_on".to_owned(), json!(blocker.id()));
        } else {
            state.insert("status".to_owned(), json!(STATUS_READY));
        }
        derived.push((category, state));
    }

    let categories: Vec<Value> = derived
        .into_iter()
        .map(|(_, object)| Value::Object(object))
        .collect();
    json!({
        "current_step": current_step.map_or(Value::Null, |step| json!(step)),
        "categories": categories,
    })
}

fn category_object(category: InitCategory) -> Map<String, Value> {
    let mut object = Map::new();
    object.insert("id".to_owned(), json!(category.id()));
    object
}

fn resolved(derived: &[(InitCategory, Map<String, Value>)], dependency: InitCategory) -> bool {
    derived.iter().any(|(category, state)| {
        *category == dependency
            && matches!(
                state.get("status").and_then(Value::as_str),
                Some(STATUS_SETTLED) | Some(STATUS_NOT_APPLICABLE)
            )
    })
}
