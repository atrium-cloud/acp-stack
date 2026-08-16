mod common;

// A Cargo integration-test crate root owns `tests/`, not `tests/<crate>/`, so the
// group modules need explicit paths.
#[path = "state_events_metrics_tests/events.rs"]
mod events;
#[path = "state_events_metrics_tests/log_filter_matcher.rs"]
mod log_filter_matcher;
#[path = "state_events_metrics_tests/log_filter_query.rs"]
mod log_filter_query;
#[path = "state_events_metrics_tests/metrics.rs"]
mod metrics;
