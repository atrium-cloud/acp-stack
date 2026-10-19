//! The provider check: one real turn with the configured model at each discovery close, so a
//! hosted client learns the credential works end to end before init commits, and can replace it
//! within the same run when it does not.

use std::path::Path;

use crate::config::Config;
use crate::error::Result;
use crate::runtime::agent::agent_headless_config::provision_agent_headless_config;
use crate::runtime::install::agent_registry::RegistryCatalog;

use super::InitArgs;
use super::prompt::{self, DiscoveryWait};
use super::state_signal::{InitStateSignal, ProviderCheckOutcome};

/// Whether this run checks the provider at its discovery close. Only a hosted client can close the
/// phase, so a run without one never parks for it.
pub(super) fn provider_check_active(args: &InitArgs) -> bool {
    args.provider_check && prompt::hosted_driver_active()
}

/// The discovery close wait. With the provider check active the run parks even when no prompt
/// opened the phase, every close runs the check, and a failed check parks again, so `Closed` means
/// the check passed or was skipped.
pub(super) fn await_discovery_close_checked(
    args: &InitArgs,
    home: &Path,
    registry: &RegistryCatalog,
    config: &Config,
) -> Result<DiscoveryWait> {
    if !provider_check_active(args) {
        return prompt::await_discovery_close();
    }
    loop {
        match prompt::park_for_discovery_close()? {
            DiscoveryWait::Closed => {
                if run_provider_check(home, registry, config)? != ProviderCheckOutcome::Failed {
                    return Ok(DiscoveryWait::Closed);
                }
            }
            revised @ DiscoveryWait::Revised(_) => return Ok(revised),
        }
    }
}

/// Re-provision the native config from the store first: a credential deposited mid-session
/// changes the store but not the files the agent reads at start.
fn run_provider_check(
    home: &Path,
    registry: &RegistryCatalog,
    config: &Config,
) -> Result<ProviderCheckOutcome> {
    provision_agent_headless_config(config, home)?;
    let turn = crate::cli::agent::run_provider_check_turn(home, config, registry);
    let outcome = if turn.passed() {
        ProviderCheckOutcome::Passed
    } else if turn.unsupported() {
        ProviderCheckOutcome::Skipped
    } else {
        ProviderCheckOutcome::Failed
    };
    prompt::emit_progress(format!("provider check: {}", turn.code));
    prompt::emit_state_signal(|| InitStateSignal::ProviderCheck {
        outcome,
        code: turn.code,
        status_code: turn.status_code,
        reason_category: turn.reason_category,
    });
    Ok(outcome)
}
