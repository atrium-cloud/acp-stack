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
//! Init is text-only by construction: `--format json` is rejected for `init`
//! before `run_init` runs, so this terminal UI never collides with structured
//! output.

use std::io;

use crate::error::{Result, StackError};

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

fn select_inner<T: Clone + Eq>(
    interactive: bool,
    prompt: &str,
    items: &[(T, String, String)],
    searchable: bool,
) -> Result<Option<T>> {
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
    if !interactive {
        return Ok(None);
    }
    match cliclack::password(prompt).mask('•').interact() {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == io::ErrorKind::Interrupted => Err(cancelled()),
        Err(error) => Err(map_interact_error(error)),
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
