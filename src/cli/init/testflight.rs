use crate::config::Config;
use crate::error::{Result, StackError};
use crate::runtime::install::agent_registry::{RegistryCatalog, RegistryEntry};

use super::{InitArgs, prompt, prompts_enabled};

/// What `acps init` should do with the post-init testflight phase. Resolved
/// from the operator's flags + TTY state + agent registry support so the
/// outer flow can render a clear log line for every path, and the test suite
/// can assert each case without exercising the real ACP bridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TestflightDecision {
    /// All preconditions met and the operator opted in (explicit flag or
    /// interactive yes).
    Run,
    /// Operator passed `--skip-testflight`.
    SkipExplicit,
    /// Non-interactive run and `--testflight` was not passed.
    SkipNonInteractive,
    /// Interactive run and the operator answered no at the credit-warning
    /// prompt.
    SkipDeclined,
    /// Selected agent isn't headless-compatible; the testflight would fail
    /// at spawn. Surface the skip so the operator isn't surprised.
    SkipUnsupported,
}

pub(super) fn resolve_testflight_decision(
    args: &InitArgs,
    config: &Config,
    registry: &RegistryCatalog,
) -> Result<Option<TestflightDecision>> {
    if args.skip_testflight {
        return Ok(Some(TestflightDecision::SkipExplicit));
    }
    let interactive = prompts_enabled(args);
    let Some(entry) = registry.lookup(&config.agent.id) else {
        // Operator's `[agent].id` doesn't match the registry (e.g., escape
        // hatch). No registry entry means we don't know the testflight
        // capabilities, so don't auto-run. Surface as a separate state only
        // if the operator explicitly asked.
        if args.testflight {
            return Err(StackError::AgentRegistryMissing {
                id: config.agent.id.clone(),
            });
        }
        return Ok(None);
    };
    if !entry.headless_compatible {
        if args.testflight {
            return Err(StackError::AgentUnsupported {
                name: entry.name.clone(),
            });
        }
        return Ok(Some(TestflightDecision::SkipUnsupported));
    }
    if args.testflight {
        if !args.handoff_json {
            print_testflight_credit_warning(entry);
        }
        return Ok(Some(TestflightDecision::Run));
    }
    if !interactive {
        return Ok(Some(TestflightDecision::SkipNonInteractive));
    }
    if confirm_testflight_credit_warning(interactive, entry)? {
        Ok(Some(TestflightDecision::Run))
    } else {
        Ok(Some(TestflightDecision::SkipDeclined))
    }
}

fn confirm_testflight_credit_warning(interactive: bool, entry: &RegistryEntry) -> Result<bool> {
    print_testflight_credit_warning(entry);
    prompt::confirm(interactive, "run testflight now?", false)
}

fn print_testflight_credit_warning(entry: &RegistryEntry) {
    println!("---");
    println!(
        "init testflight will start `{}` and send a real prompt to the configured provider.",
        entry.name
    );
    println!("this may consume provider credits.");
}
