use crate::config::Config;
use crate::error::{Result, StackError};
use crate::runtime::install::agent_registry::{RegistryCatalog, RegistryEntry};
use crate::secrets::SecretStore;

use super::provider::{pending_custom_provider_credential, pending_provider_credential_reason};
use super::{InitArgs, prompt, prompts_enabled};

/// What `acps init` should do with the post-init testflight phase. Resolved
/// from the operator's flags + TTY state + agent registry support so the
/// outer flow can render a clear log line for every path, and the test suite
/// can assert each case without exercising the real ACP bridge.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// A hosting backend answered no because it will run the testflight itself
    /// after setup, which it marks with the `deferred` flag on its answer.
    SkipDeferred,
    /// Selected agent isn't headless-compatible; the testflight would fail
    /// at spawn. Surface the skip so the operator isn't surprised.
    SkipUnsupported,
    /// A configured custom provider's api-key ref has not arrived yet (hosted
    /// init defers it to a managed credential push), so the real prompt has no
    /// credential to spend.
    SkipCredentialPending {
        provider_id: String,
        api_key_ref: String,
    },
}

impl TestflightDecision {
    /// Stable label for the recorded step payload. `Debug` would embed quotes
    /// from the credential-pending variant's fields and break that JSON.
    pub(super) fn label(&self) -> &'static str {
        match self {
            TestflightDecision::Run => "Run",
            TestflightDecision::SkipExplicit => "SkipExplicit",
            TestflightDecision::SkipNonInteractive => "SkipNonInteractive",
            TestflightDecision::SkipDeclined => "SkipDeclined",
            TestflightDecision::SkipDeferred => "SkipDeferred",
            TestflightDecision::SkipUnsupported => "SkipUnsupported",
            TestflightDecision::SkipCredentialPending { .. } => "SkipCredentialPending",
        }
    }

    /// The line the finalize step prints for a decision that did not run the
    /// testflight. `None` for `Run`, which prints its own progress instead.
    pub(super) fn skip_message(&self) -> Option<String> {
        let message = match self {
            TestflightDecision::Run => return None,
            TestflightDecision::SkipExplicit => {
                "testflight: skipped (--skip-testflight)".to_owned()
            }
            TestflightDecision::SkipNonInteractive => {
                "testflight: skipped (non-interactive run; pass --testflight to opt in)".to_owned()
            }
            TestflightDecision::SkipDeclined => {
                "testflight: skipped (declined at prompt)".to_owned()
            }
            TestflightDecision::SkipDeferred => {
                "testflight: deferred (runs after setup)".to_owned()
            }
            TestflightDecision::SkipUnsupported => {
                "testflight: skipped (agent does not support headless testflight)".to_owned()
            }
            TestflightDecision::SkipCredentialPending {
                provider_id,
                api_key_ref,
            } => format!(
                "testflight: skipped (provider `{provider_id}` credential `{api_key_ref}` is pending a managed push)"
            ),
        };
        Some(message)
    }
}

pub(super) fn resolve_testflight_decision(
    args: &InitArgs,
    config: &Config,
    registry: &RegistryCatalog,
    secrets: &SecretStore,
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
    // The testflight sends a real prompt, so it needs a resolvable provider
    // credential. A hosted init defers a custom provider's ref to a managed
    // push after init, which would surface here as an opaque spawn failure.
    if let Some((provider_id, api_key_ref)) = pending_custom_provider_credential(config, secrets) {
        if args.testflight {
            return Err(StackError::InvalidParam {
                field: "testflight",
                reason: pending_provider_credential_reason(&provider_id, &api_key_ref),
            });
        }
        return Ok(Some(TestflightDecision::SkipCredentialPending {
            provider_id,
            api_key_ref,
        }));
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
    let answer = confirm_testflight_credit_warning(interactive, entry)?;
    if answer.value {
        return Ok(Some(TestflightDecision::Run));
    }
    if answer.deferred {
        return Ok(Some(TestflightDecision::SkipDeferred));
    }
    Ok(Some(TestflightDecision::SkipDeclined))
}

fn confirm_testflight_credit_warning(
    interactive: bool,
    entry: &RegistryEntry,
) -> Result<prompt::ConfirmAnswer> {
    print_testflight_credit_warning(entry);
    prompt::confirm_with_deferral(
        prompt::HostedPromptKind::TestflightConfirm,
        interactive,
        "run testflight now?",
        false,
    )
}

fn print_testflight_credit_warning(entry: &RegistryEntry) {
    println!("---");
    println!(
        "init testflight will start `{}` and send a real prompt to the configured provider.",
        entry.name
    );
    println!("this may consume provider credits.");
}
