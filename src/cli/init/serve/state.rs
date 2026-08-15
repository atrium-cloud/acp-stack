use super::*;

/// Canonical wire order of the category list. Clients render the array as
/// given, and the order is also load-bearing for `derive_snapshot`: it is a
/// topological order of the dependency table below, which is what lets the
/// derivation resolve `blocked_on` in a single pass.
const CATEGORY_ORDER: [InitCategory; CATEGORY_COUNT] = [
    InitCategory::Agent,
    InitCategory::Provider,
    InitCategory::Model,
    InitCategory::Mode,
    InitCategory::Workspace,
    InitCategory::NativeConfig,
    InitCategory::Mcp,
    InitCategory::Skills,
    InitCategory::Deps,
];

const CATEGORY_COUNT: usize = 9;

const STATUS_NOT_APPLICABLE: &str = "not_applicable";
const STATUS_FAILED: &str = "failed";
const STATUS_AWAITING_INPUT: &str = "awaiting_input";
const STATUS_SETTLED: &str = "settled";
const STATUS_BLOCKED: &str = "blocked";
const STATUS_READY: &str = "ready";

/// What a category is still waiting on before init can drive it. Declared as
/// data rather than derived by traversal: the edges are few, fixed, and each
/// one encodes a decision that would otherwise be invisible.
fn dependency(category: InitCategory) -> Option<InitCategory> {
    match category {
        InitCategory::Agent => None,
        InitCategory::Provider => Some(InitCategory::Agent),
        InitCategory::Model => Some(InitCategory::Provider),
        InitCategory::Mode => Some(InitCategory::Model),
        InitCategory::Workspace => None,
        // The native Agent config review runs before the agent is settled, so
        // it deliberately carries no Agent edge.
        InitCategory::NativeConfig => None,
        InitCategory::Mcp => Some(InitCategory::Agent),
        InitCategory::Skills => Some(InitCategory::Agent),
        InitCategory::Deps => None,
    }
}

/// Slot of a category in [`CategoryMap`], matching [`CATEGORY_ORDER`]. Written
/// as a match rather than a search so the map stays index-addressed with no
/// `Option` to unwrap on the hot path.
fn slot(category: InitCategory) -> usize {
    match category {
        InitCategory::Agent => 0,
        InitCategory::Provider => 1,
        InitCategory::Model => 2,
        InitCategory::Mode => 3,
        InitCategory::Workspace => 4,
        InitCategory::NativeConfig => 5,
        InitCategory::Mcp => 6,
        InitCategory::Skills => 7,
        InitCategory::Deps => 8,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CategoryOutcome {
    Settled {
        value: Option<String>,
        /// Whether this settlement was read off the config already on disk
        /// rather than written by this run. Both report identically on the
        /// wire; the difference is only who may take them back, which
        /// [`CategoryMap::set_applicability`] rules on.
        provisional: bool,
    },
    Failed {
        code: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CategoryEntry {
    /// Applicable until something says otherwise: a lane this agent does not
    /// have is the exception, and the derivations that know are emitted well
    /// after the session exists.
    applicable: bool,
    applicability_source: Option<ApplicabilitySource>,
    /// Why the lane is absent, carried only for an inapplicable verdict. Held
    /// beside the verdict so a hidden category can say what hid it instead of
    /// the client having to infer it from the agent's registry row.
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

#[derive(Debug, Default)]
pub(super) struct CategoryMap([CategoryEntry; CATEGORY_COUNT]);

impl CategoryMap {
    fn entry(&self, category: InitCategory) -> &CategoryEntry {
        &self.0[slot(category)]
    }

    fn entry_mut(&mut self, category: InitCategory) -> &mut CategoryEntry {
        &mut self.0[slot(category)]
    }

    /// Record an applicability verdict. Anything that spoke to the installed
    /// harness — `Probe`, `Discovery`, and `DiscoveryUnavailable` — is the
    /// harness talking, so a later registry claim never takes it back; every
    /// other source applies in arrival order.
    ///
    /// Retraction is three-tier. A failure and a settlement this run wrote are
    /// never taken back: the lane demonstrably applied, whatever a late verdict
    /// says. A provisional settlement — a value that was already in config when
    /// the run started — is only a report, so a `Probe` or `Discovery` verdict
    /// that finds the lane gone withdraws it, and the stale value goes with it.
    /// `DiscoveryUnavailable` withdraws nothing that produced an outcome: a
    /// check that could not run is no evidence about a lane the config holds a
    /// value for. It still rules on a lane with no outcome, which is the run
    /// that never had the value in the first place.
    pub(super) fn set_applicability(
        &mut self,
        category: InitCategory,
        applicable: bool,
        source: ApplicabilitySource,
        reason: Option<String>,
    ) {
        let entry = self.entry_mut(category);
        // Authority is settled before the outcome is touched: a source with
        // nothing to say here must not clear a provisional settlement on its
        // way out.
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

    pub(super) fn settle(&mut self, category: InitCategory, value: Option<String>) {
        self.entry_mut(category).outcome = Some(CategoryOutcome::Settled {
            value,
            provisional: false,
        });
    }

    /// Report a value that was in config before this run touched it. It reads
    /// as settled — the lane is configured, and for a resumed or fully declared
    /// run no write site will ever fire to say so — but it rests on the disk,
    /// not on anything this run observed, so a live probe or discovery verdict
    /// may still withdraw it. A write site that later drives the lane settles it
    /// again through [`Self::settle`], which promotes it to final.
    pub(super) fn settle_provisional(&mut self, category: InitCategory, value: String) {
        self.entry_mut(category).outcome = Some(CategoryOutcome::Settled {
            value: Some(value),
            provisional: true,
        });
    }

    pub(super) fn fail(&mut self, category: InitCategory, code: String) {
        self.entry_mut(category).outcome = Some(CategoryOutcome::Failed { code });
    }

    /// Badge a category on the strength of its step failing. This is
    /// unattributed blame — `provider_configure` alone owns the provider,
    /// model, and mode lanes, so the step only knows that something under it
    /// broke — which is why two cases are left alone. A category already failed
    /// with this same code means an inner lane claimed the failure before it
    /// propagated, and the step is watching its own error pass by. A lane this
    /// run does not have never ran, and `failed` outranks `not_applicable`, so
    /// badging it would invent a broken lane out of the terminal error frame.
    /// Everything else is badged, over a settlement included: a step that broke
    /// after its lane settled (an MCP write failing behind a settled probe) did
    /// break that lane.
    pub(super) fn fail_step_category(&mut self, category: InitCategory, code: String) {
        let already_blamed = self.0.iter().any(|entry| {
            matches!(&entry.outcome, Some(CategoryOutcome::Failed { code: blamed }) if *blamed == code)
        });
        if already_blamed || !self.entry(category).applicable {
            return;
        }
        self.fail(category, code);
    }

    /// Settle a category on the strength of its step finishing. An explicit
    /// signal already carries what was written, so it wins: the step itself
    /// only knows that it ran.
    pub(super) fn settle_unresolved(&mut self, category: InitCategory) {
        if self.entry(category).outcome.is_none() {
            self.settle(category, None);
        }
    }

    /// Terminal sweep. After init completes nothing may still derive as
    /// `ready`, so every applicable category that never produced an outcome —
    /// deps candidates the operator declined, skills with nothing selected,
    /// steps that never ran — settles with no value.
    pub(super) fn settle_remaining(&mut self) {
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

/// The `state` frame body, also embedded in `hello` and the REST status.
/// Serialize-only and built from derived structs, so its key order is this
/// declaration order on every build.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct StateSnapshot {
    current_step: Option<&'static str>,
    categories: Vec<CategoryStateWire>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct CategoryStateWire {
    id: &'static str,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    blocked_on: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<String>,
    /// Carried only by `not_applicable`: the one status whose explanation the
    /// client cannot derive from the rest of the snapshot.
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

impl CategoryStateWire {
    fn new(category: InitCategory, status: &'static str) -> Self {
        Self {
            id: category.id(),
            status,
            blocked_on: None,
            value: None,
            code: None,
            reason: None,
        }
    }
}

impl StateSnapshot {
    /// The `state` event payload. Split from the `Serialize` impl so the two
    /// snapshot fields land as top-level envelope keys rather than a nested
    /// object; only the category list can fail to encode.
    pub(super) fn payload(&self) -> std::result::Result<Map<String, Value>, FrameError> {
        let mut payload = Map::new();
        payload.insert(
            "current_step".to_owned(),
            match self.current_step {
                Some(step) => Value::String(step.to_owned()),
                None => Value::Null,
            },
        );
        payload.insert(
            "categories".to_owned(),
            serde_json::to_value(&self.categories)
                .map_err(|source| FrameError::Encode { source })?,
        );
        Ok(payload)
    }
}

/// The only place a category status is computed. Precedence is fixed: `failed`
/// (a lane broke) beats `not_applicable` (this run does not have the lane)
/// beats `awaiting_input` (a prompt is on the wire for it right now) beats
/// `settled` beats `blocked` beats `ready`. Failure leads because a lane that
/// broke did run: an inapplicability verdict that arrived before the failure
/// must not hide it.
///
/// `awaiting_input` is derived from the pending prompt rather than stored, so
/// at most one category can ever hold it: there is one pending-input slot and
/// one wizard thread.
pub(super) fn derive_snapshot(
    categories: &CategoryMap,
    current_step: Option<&'static str>,
    pending_kind: Option<HostedPromptKind>,
) -> StateSnapshot {
    let awaiting = pending_kind.and_then(HostedPromptKind::category);
    let mut derived: Vec<CategoryStateWire> = Vec::with_capacity(CATEGORY_COUNT);
    for category in CATEGORY_ORDER {
        let entry = categories.entry(category);
        let state = if let Some(CategoryOutcome::Failed { code }) = &entry.outcome {
            CategoryStateWire {
                code: Some(code.clone()),
                ..CategoryStateWire::new(category, STATUS_FAILED)
            }
        } else if !entry.applicable {
            CategoryStateWire {
                reason: entry.applicability_reason.clone(),
                ..CategoryStateWire::new(category, STATUS_NOT_APPLICABLE)
            }
        } else if awaiting == Some(category) {
            CategoryStateWire::new(category, STATUS_AWAITING_INPUT)
        } else if let Some(CategoryOutcome::Settled { value, .. }) = &entry.outcome {
            CategoryStateWire {
                value: value.clone(),
                ..CategoryStateWire::new(category, STATUS_SETTLED)
            }
        } else if let Some(blocker) =
            dependency(category).filter(|blocker| !resolved(&derived, *blocker))
        {
            CategoryStateWire {
                blocked_on: Some(blocker.id()),
                ..CategoryStateWire::new(category, STATUS_BLOCKED)
            }
        } else {
            CategoryStateWire::new(category, STATUS_READY)
        };
        derived.push(state);
    }
    StateSnapshot {
        current_step,
        categories: derived,
    }
}

/// Whether a dependency has stopped standing in the way. Reads the statuses
/// derived so far in this pass, which is sound because [`CATEGORY_ORDER`] is
/// topological; a dependency that had not been derived yet would read as
/// unresolved, keeping the dependent blocked rather than panicking.
fn resolved(derived: &[CategoryStateWire], dependency: InitCategory) -> bool {
    derived.iter().any(|state| {
        state.id == dependency.id()
            && (state.status == STATUS_SETTLED || state.status == STATUS_NOT_APPLICABLE)
    })
}
