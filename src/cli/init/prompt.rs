//! Shared interactive prompt helpers for `acps init`, built on `cliclack`.
//!
//! Every helper takes an `interactive: bool` and checks it FIRST. When it is
//! false (a `--non-interactive` run or a non-TTY stdin) the helper returns its
//! skip/default value WITHOUT touching `cliclack`, so the documented
//! non-interactive contract holds and the wizard is never driven without a
//! terminal. The caller computes `interactive` once via `prompts_enabled` in
//! `init.rs` (`is_terminal() && !args.non_interactive`).
//!
//! Esc/cancel surfaces from `cliclack::interact()` as
//! `io::ErrorKind::Interrupted`; this is a deliberate init abort. Optional
//! prompts must expose an explicit Skip item instead of treating cancellation as
//! a hidden skip.
//!
//! `--format json` is rejected for `init`, and `--handoff-json` disables prompts
//! before these helpers run, so terminal UI never collides with structured
//! output.

use std::cell::RefCell;
use std::io;
use std::sync::Arc;

use crate::error::{Result, StackError};

#[derive(Debug, Clone)]
pub(super) struct HostedPromptItem {
    pub(super) label: String,
    pub(super) hint: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HostedPromptStyle {
    Select,
    SearchableSelect,
    Multiselect,
    Confirm,
    Text,
    Password,
}

#[derive(Debug, Clone)]
pub(super) struct HostedPromptRequest {
    pub(super) style: HostedPromptStyle,
    pub(super) prompt: String,
    pub(super) required: bool,
    pub(super) default: Option<bool>,
    pub(super) items: Vec<HostedPromptItem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum HostedPromptOutcome<T> {
    Handled(T),
    Unhandled,
}

pub(super) trait HostedPromptDriver: Send + Sync {
    fn select(&self, request: HostedPromptRequest) -> Result<HostedPromptOutcome<Option<usize>>>;
    fn multiselect(&self, request: HostedPromptRequest) -> Result<HostedPromptOutcome<Vec<usize>>>;
    fn confirm(&self, request: HostedPromptRequest) -> Result<HostedPromptOutcome<bool>>;
    fn text(&self, request: HostedPromptRequest) -> Result<HostedPromptOutcome<Option<String>>>;
    fn password(&self, request: HostedPromptRequest)
    -> Result<HostedPromptOutcome<Option<String>>>;
    fn progress(&self, message: String);
    fn result(&self, payload: serde_json::Value);
}

thread_local! {
    static HOSTED_DRIVER: RefCell<Option<Arc<dyn HostedPromptDriver>>> = RefCell::new(None);
}

pub(super) fn with_hosted_driver<T>(
    driver: Arc<dyn HostedPromptDriver>,
    work: impl FnOnce() -> T,
) -> T {
    struct DriverReset(Option<Arc<dyn HostedPromptDriver>>);

    impl Drop for DriverReset {
        fn drop(&mut self) {
            HOSTED_DRIVER.with(|slot| {
                *slot.borrow_mut() = self.0.take();
            });
        }
    }

    let previous = HOSTED_DRIVER.with(|slot| slot.borrow_mut().replace(driver));
    let _reset = DriverReset(previous);
    work()
}

pub(super) fn hosted_driver_active() -> bool {
    HOSTED_DRIVER.with(|slot| slot.borrow().is_some())
}

pub(super) fn emit_progress(message: impl Into<String>) {
    if let Some(driver) = HOSTED_DRIVER.with(|slot| slot.borrow().clone()) {
        driver.progress(message.into());
    }
}

pub(super) fn emit_result(payload: serde_json::Value) {
    if let Some(driver) = HOSTED_DRIVER.with(|slot| slot.borrow().clone()) {
        driver.result(payload);
    }
}

fn map_interact_error(source: io::Error) -> StackError {
    StackError::StdinRead { source }
}

fn cancelled() -> StackError {
    StackError::InvalidParam {
        field: "init",
        reason: "cancelled by operator".to_owned(),
    }
}

/// Single-choice picker. Returns the chosen value, or `None` when not
/// interactive or when there is nothing to choose.
pub(super) fn select<T: Clone + Eq>(
    interactive: bool,
    prompt: &str,
    items: &[(T, String, String)],
) -> Result<Option<T>> {
    select_inner(interactive, prompt, items, false)
}

pub(super) fn searchable_select<T: Clone + Eq>(
    interactive: bool,
    prompt: &str,
    items: &[(T, String, String)],
) -> Result<Option<T>> {
    select_inner(interactive, prompt, items, true)
}

pub(super) fn multiselect<T: Clone + Eq>(
    interactive: bool,
    prompt: &str,
    items: &[(T, String, String)],
) -> Result<Vec<T>> {
    if let Some(driver) = HOSTED_DRIVER.with(|slot| slot.borrow().clone()) {
        let request = hosted_request(HostedPromptStyle::Multiselect, prompt, false, None, items);
        return match driver.multiselect(request)? {
            HostedPromptOutcome::Handled(indices) => indices
                .into_iter()
                .map(|index| {
                    items.get(index).map(|(value, _, _)| value.clone()).ok_or(
                        StackError::InvalidParam {
                            field: "init",
                            reason: format!("hosted init selected invalid item index {index}"),
                        },
                    )
                })
                .collect(),
            HostedPromptOutcome::Unhandled => Ok(Vec::new()),
        };
    }
    if !interactive || items.is_empty() {
        return Ok(Vec::new());
    }
    let mut builder = cliclack::multiselect::<T>(prompt)
        .required(false)
        .max_rows(12);
    for (value, label, hint) in items {
        builder = builder.item(value.clone(), label, hint);
    }
    match builder.interact() {
        Ok(values) => Ok(values),
        Err(error) if error.kind() == io::ErrorKind::Interrupted => Err(cancelled()),
        Err(error) => Err(map_interact_error(error)),
    }
}

fn select_inner<T: Clone + Eq>(
    interactive: bool,
    prompt: &str,
    items: &[(T, String, String)],
    searchable: bool,
) -> Result<Option<T>> {
    if let Some(driver) = HOSTED_DRIVER.with(|slot| slot.borrow().clone()) {
        let request = hosted_request(
            if searchable {
                HostedPromptStyle::SearchableSelect
            } else {
                HostedPromptStyle::Select
            },
            prompt,
            false,
            None,
            items,
        );
        return match driver.select(request)? {
            HostedPromptOutcome::Handled(Some(index)) => items
                .get(index)
                .map(|(value, _, _)| Some(value.clone()))
                .ok_or(StackError::InvalidParam {
                    field: "init",
                    reason: format!("hosted init selected invalid item index {index}"),
                }),
            HostedPromptOutcome::Handled(None) | HostedPromptOutcome::Unhandled => Ok(None),
        };
    }
    if !interactive || items.is_empty() {
        return Ok(None);
    }
    let mut builder = cliclack::select::<T>(prompt);
    if searchable {
        builder = builder.filter_mode().max_rows(12);
    }
    for (value, label, hint) in items {
        builder = builder.item(value.clone(), label, hint);
    }
    match builder.interact() {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == io::ErrorKind::Interrupted => Err(cancelled()),
        Err(error) => Err(map_interact_error(error)),
    }
}

/// Yes/no confirm. Returns `default` when not interactive, so the caller picks
/// the right polarity (`false` for opt-in prompts, `true` for default-yes ones).
pub(super) fn confirm(interactive: bool, prompt: &str, default: bool) -> Result<bool> {
    if let Some(driver) = HOSTED_DRIVER.with(|slot| slot.borrow().clone()) {
        let request = HostedPromptRequest {
            style: HostedPromptStyle::Confirm,
            prompt: prompt.to_owned(),
            required: true,
            default: Some(default),
            items: Vec::new(),
        };
        return match driver.confirm(request)? {
            HostedPromptOutcome::Handled(value) => Ok(value),
            HostedPromptOutcome::Unhandled => Ok(default),
        };
    }
    if !interactive {
        return Ok(default);
    }
    match cliclack::confirm(prompt).initial_value(default).interact() {
        Ok(value) => Ok(value),
        Err(error) if error.kind() == io::ErrorKind::Interrupted => Err(cancelled()),
        Err(error) => Err(map_interact_error(error)),
    }
}

/// Free-text line. `required` re-prompts on empty input. Returns `None` when
/// not interactive; the caller decides whether `None` is a skip or a hard error
/// for its field.
pub(super) fn text(interactive: bool, prompt: &str, required: bool) -> Result<Option<String>> {
    if let Some(driver) = HOSTED_DRIVER.with(|slot| slot.borrow().clone()) {
        let request = HostedPromptRequest {
            style: HostedPromptStyle::Text,
            prompt: prompt.to_owned(),
            required,
            default: None,
            items: Vec::new(),
        };
        return match driver.text(request)? {
            HostedPromptOutcome::Handled(value) => Ok(value),
            HostedPromptOutcome::Unhandled => Ok(None),
        };
    }
    if !interactive {
        return Ok(None);
    }
    let result: io::Result<String> = cliclack::input(prompt).required(required).interact();
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == io::ErrorKind::Interrupted => Err(cancelled()),
        Err(error) => Err(map_interact_error(error)),
    }
}

/// Run `work` while showing an animated spinner with `message`. The spinner
/// stops with a success line on `Ok` and an error line on `Err`. Only call this
/// on the interactive path — cliclack writes the spinner to the terminal.
pub(super) fn with_spinner<T>(message: &str, work: impl FnOnce() -> Result<T>) -> Result<T> {
    if hosted_driver_active() {
        emit_progress(message.to_owned());
        return work();
    }
    let spinner = cliclack::spinner();
    spinner.start(message);
    match work() {
        Ok(value) => {
            spinner.stop(message);
            Ok(value)
        }
        Err(error) => {
            spinner.error(message);
            Err(error)
        }
    }
}

/// Masked secret entry. Returns `None` when not interactive.
pub(super) fn password(interactive: bool, prompt: &str) -> Result<Option<String>> {
    if let Some(driver) = HOSTED_DRIVER.with(|slot| slot.borrow().clone()) {
        let request = HostedPromptRequest {
            style: HostedPromptStyle::Password,
            prompt: prompt.to_owned(),
            required: true,
            default: None,
            items: Vec::new(),
        };
        return match driver.password(request)? {
            HostedPromptOutcome::Handled(value) => Ok(value),
            HostedPromptOutcome::Unhandled => Ok(None),
        };
    }
    if !interactive {
        return Ok(None);
    }
    match cliclack::password(prompt).mask('•').interact() {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == io::ErrorKind::Interrupted => Err(cancelled()),
        Err(error) => Err(map_interact_error(error)),
    }
}

fn hosted_request<T>(
    style: HostedPromptStyle,
    prompt: &str,
    required: bool,
    default: Option<bool>,
    items: &[(T, String, String)],
) -> HostedPromptRequest {
    HostedPromptRequest {
        style,
        prompt: prompt.to_owned(),
        required,
        default,
        items: items
            .iter()
            .map(|(_, label, hint)| HostedPromptItem {
                label: label.clone(),
                hint: hint.clone(),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The load-bearing invariant: non-interactive helpers return the skip /
    // default value WITHOUT touching stdin or cliclack.

    #[test]
    fn select_returns_none_when_not_interactive() {
        let items = [(1u8, "one".to_owned(), String::new())];
        assert_eq!(select(false, "pick", &items).expect("select"), None);
    }

    #[test]
    fn multiselect_returns_empty_when_not_interactive() {
        let items = [(1u8, "one".to_owned(), String::new())];
        assert_eq!(
            multiselect(false, "pick", &items).expect("multiselect"),
            Vec::<u8>::new()
        );
    }

    #[test]
    fn confirm_returns_default_when_not_interactive() {
        assert!(confirm(false, "ok?", true).expect("confirm true"));
        assert!(!confirm(false, "ok?", false).expect("confirm false"));
    }

    #[test]
    fn text_returns_none_when_not_interactive() {
        assert_eq!(text(false, "name", true).expect("text"), None);
    }

    #[test]
    fn password_returns_none_when_not_interactive() {
        assert_eq!(password(false, "secret").expect("password"), None);
    }
}
