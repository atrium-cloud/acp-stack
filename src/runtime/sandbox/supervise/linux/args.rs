//! Argument parsing for the supervisor and the provider monitor, plus the
//! runtime-only `--sync-fd` injection into the wrapper argv.

use super::*;

pub(super) fn parse_args(raw_args: Vec<String>) -> Result<SuperviseOptions> {
    let mut diag_fd: Option<i32> = None;
    let mut provider: Vec<String> = Vec::new();
    let mut provider_timeout: Option<Duration> = None;
    let mut provider_stderr: Option<SandboxProviderStderr> = None;
    let mut child_command: Vec<String> = Vec::new();
    let mut iter = raw_args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--diag-fd" => {
                let value = next_value(&mut iter, "--diag-fd")?;
                diag_fd = Some(
                    value
                        .parse::<i32>()
                        .map_err(|_| StackError::SandboxFailed {
                            reason: format!("--diag-fd expects an fd number, got `{value}`"),
                        })?,
                );
            }
            "--provider-timeout" => {
                let value = next_value(&mut iter, "--provider-timeout")?;
                let parsed = crate::config::parse_duration_string(&value).ok_or_else(|| {
                    StackError::SandboxFailed {
                        reason: format!("--provider-timeout `{value}` is not a valid duration"),
                    }
                })?;
                provider_timeout = Some(parsed);
            }
            "--provider-stderr" => {
                let value = next_value(&mut iter, "--provider-stderr")?;
                provider_stderr = Some(match value.as_str() {
                    "daemon" => SandboxProviderStderr::Daemon,
                    "null" => SandboxProviderStderr::Null,
                    other => {
                        return Err(StackError::SandboxFailed {
                            reason: format!(
                                "--provider-stderr expects `daemon` or `null`, got `{other}`"
                            ),
                        });
                    }
                });
            }
            "--provider-arg" => {
                provider.push(next_value(&mut iter, "--provider-arg")?);
            }
            "--" => {
                child_command = iter.collect();
                break;
            }
            other => {
                return Err(StackError::SandboxFailed {
                    reason: format!("unexpected sandbox-supervise argument `{other}`"),
                });
            }
        }
    }
    if child_command.is_empty() {
        return Err(StackError::SandboxFailed {
            reason: "sandbox-supervise requires a command after `--`".to_owned(),
        });
    }
    Ok(SuperviseOptions {
        diag_fd: diag_fd.ok_or_else(|| missing_flag("--diag-fd"))?,
        provider,
        provider_timeout: provider_timeout.ok_or_else(|| missing_flag("--provider-timeout"))?,
        provider_stderr: provider_stderr.ok_or_else(|| missing_flag("--provider-stderr"))?,
        child_command,
    })
}

pub(super) fn parse_provider_supervise_args(
    raw_args: Vec<String>,
) -> Result<ProviderSuperviseOptions> {
    let mut liveness_fd: Option<i32> = None;
    let mut provider_stderr: Option<SandboxProviderStderr> = None;
    let mut provider_command = Vec::new();
    let mut iter = raw_args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--liveness-fd" => {
                let value = next_value(&mut iter, "--liveness-fd")?;
                liveness_fd =
                    Some(
                        value
                            .parse::<i32>()
                            .map_err(|_| StackError::SandboxFailed {
                                reason: format!(
                                    "--liveness-fd expects an fd number, got `{value}`"
                                ),
                            })?,
                    );
            }
            "--provider-stderr" => {
                provider_stderr = Some(parse_provider_stderr(&next_value(
                    &mut iter,
                    "--provider-stderr",
                )?)?);
            }
            "--" => {
                provider_command = iter.collect();
                break;
            }
            other => {
                return Err(StackError::SandboxFailed {
                    reason: format!("unexpected sandbox-provider-supervise argument `{other}`"),
                });
            }
        }
    }
    if provider_command.is_empty() {
        return Err(StackError::SandboxFailed {
            reason: "sandbox-provider-supervise requires a provider command after `--`".to_owned(),
        });
    }
    Ok(ProviderSuperviseOptions {
        liveness_fd: liveness_fd.ok_or_else(|| missing_flag("--liveness-fd"))?,
        provider_stderr: provider_stderr.ok_or_else(|| missing_flag("--provider-stderr"))?,
        provider_command,
    })
}

fn parse_provider_stderr(value: &str) -> Result<SandboxProviderStderr> {
    match value {
        "daemon" => Ok(SandboxProviderStderr::Daemon),
        "null" => Ok(SandboxProviderStderr::Null),
        other => Err(StackError::SandboxFailed {
            reason: format!("--provider-stderr expects `daemon` or `null`, got `{other}`"),
        }),
    }
}

fn next_value(iter: &mut std::vec::IntoIter<String>, flag: &str) -> Result<String> {
    iter.next().ok_or_else(|| StackError::SandboxFailed {
        reason: format!("{flag} requires a value"),
    })
}

fn missing_flag(flag: &str) -> StackError {
    StackError::SandboxFailed {
        reason: format!("sandbox-supervise requires {flag}"),
    }
}

/// Inject `--sync-fd` right after the subcommand token; the value only exists
/// at supervisor runtime, so the stored wrapper argv omits it.
pub(super) fn inject_sync_fd(child_command: &mut Vec<String>, sync_fd: i32) -> Result<()> {
    let position = child_command
        .iter()
        .position(|arg| arg == SANDBOX_EXEC_SUBCOMMAND)
        .ok_or_else(|| StackError::SandboxFailed {
            reason: format!(
                "sandbox-supervise child command does not contain `{SANDBOX_EXEC_SUBCOMMAND}`"
            ),
        })?;
    child_command.insert(position + 1, "--sync-fd".to_owned());
    child_command.insert(position + 2, sync_fd.to_string());
    Ok(())
}
