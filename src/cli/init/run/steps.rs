use super::*;

/// [`finalize_with_error`] where a value would otherwise be returned. It never
/// yields one: it returns the error it was handed, or the store failure that
/// preempted it.
pub(super) fn finalize_failure<T>(
    store: &StateStore,
    run: &crate::state::InitRunRecord,
    error: StackError,
) -> Result<T> {
    let finalized = finalize_with_error(store, run, error);
    Err(finalized
        .err()
        .unwrap_or_else(|| StackError::InitRunCorrupted {
            reason: format!("init run {} finalized without an error", run.id),
        }))
}

pub(super) fn signal_category_failed(category: InitCategory, error: &StackError) {
    prompt::emit_state_signal(|| InitStateSignal::CategoryFailed {
        category,
        code: error.error_code().to_owned(),
    });
}

fn signal_step_started(kind: &'static str) {
    prompt::emit_state_signal(|| InitStateSignal::StepStarted { kind });
}

fn signal_step_finished(kind: &'static str, result: &Result<StepDisposition>) {
    prompt::emit_state_signal(|| InitStateSignal::StepFinished {
        kind,
        // A failed step has no disposition of its own; the error_code is what
        // distinguishes it, so the executed/skipped axis reports the body ran.
        disposition: result
            .as_ref()
            .copied()
            .unwrap_or(StepDisposition::Executed),
        error_code: result
            .as_ref()
            .err()
            .map(|error| error.error_code().to_owned()),
    });
}

/// `init_runner::record_step` bracketed with state signals. The runtime
/// recorder stays ignorant of hosted concepts, so the bracketing lives here,
/// on the driver side; the call order below is the authority on step sequence,
/// never the ordinals.
pub(super) fn record_init_step(
    store: &StateStore,
    run: &crate::state::InitRunRecord,
    ordinal: i64,
    kind: &'static str,
    verify: impl FnOnce() -> Result<bool>,
    body: impl FnOnce() -> Result<StepOutcome>,
) -> Result<StepDisposition> {
    signal_step_started(kind);
    let result = record_step(store, run, ordinal, kind, verify, body);
    signal_step_finished(kind, &result);
    result
}

/// Signal-bracketed [`crate::runtime::init_runner::record_step_with_default_log_dir`].
#[allow(clippy::too_many_arguments)]
pub(super) fn record_init_step_with_default_log_dir(
    store: &StateStore,
    run: &crate::state::InitRunRecord,
    ordinal: i64,
    kind: &'static str,
    default_log_dir: Option<&str>,
    verify: impl FnOnce() -> Result<bool>,
    body: impl FnOnce() -> Result<StepOutcome>,
) -> Result<StepDisposition> {
    signal_step_started(kind);
    let result = crate::runtime::init_runner::record_step_with_default_log_dir(
        store,
        run,
        ordinal,
        kind,
        default_log_dir,
        verify,
        body,
    );
    signal_step_finished(kind, &result);
    result
}
