//! Detached dependency-apply worker (`acps __deps-apply-run`). The settled `deps_apply_runs` row,
//! not the exit code, is the contract a hosting client polls. The worker never takes the
//! agent-config mutation flock, so a long install cannot stall config routes or agent restarts.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command as ProcessCommand, Stdio};

use crate::config::Config;
use crate::error::{Result, StackError};
use crate::runtime::dependencies::deps_apply::{
    PrivilegeEscalation, TrackedApplyRun, apply_dependencies_tracked, run_status_for_report,
};
use crate::runtime::process_runner::{current_boot_id, detach_into_new_session};
use crate::state::{DEPS_APPLY_RUN_FAILED, StateStore};

/// Internal subcommand name; not part of the supported command surface.
pub const DEPS_APPLY_WORKER_SUBCOMMAND: &str = "__deps-apply-run";

/// Filename for the worker's combined stdout/stderr inside the run's log dir.
pub const DEPS_APPLY_WORKER_LOG_FILE: &str = "child.log";

/// Parsed `__deps-apply-run` argv. Every invocation is machine-generated, so parsing is strict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerArgs {
    pub config_path: PathBuf,
    pub state_path: PathBuf,
    pub apply_run_id: String,
    pub feature: Option<String>,
    pub escalation: PrivilegeEscalation,
    pub log_dir: Option<PathBuf>,
}

fn invalid(reason: String) -> StackError {
    StackError::InvalidParam {
        field: "__deps-apply-run",
        reason,
    }
}

/// Wire form of a probed [`PrivilegeEscalation`] decision, so the worker never re-probes sudo and
/// runs exactly the mode the confirm prompt promised.
pub fn escalation_to_args(escalation: &PrivilegeEscalation) -> Vec<String> {
    match escalation {
        PrivilegeEscalation::NotNeeded => vec!["--escalation".into(), "not-needed".into()],
        PrivilegeEscalation::Sudo { sudo_path, uid } => vec![
            "--escalation".into(),
            "sudo".into(),
            "--escalation-uid".into(),
            uid.to_string(),
            "--escalation-sudo-path".into(),
            sudo_path.to_string_lossy().into_owned(),
        ],
        PrivilegeEscalation::Unavailable { uid } => vec![
            "--escalation".into(),
            "unavailable".into(),
            "--escalation-uid".into(),
            uid.to_string(),
        ],
    }
}

pub fn parse_worker_args(args: &[String]) -> Result<WorkerArgs> {
    let mut config_path = None;
    let mut state_path = None;
    let mut apply_run_id = None;
    let mut feature = None;
    let mut escalation_kind = None;
    let mut escalation_uid = None;
    let mut escalation_sudo_path = None;
    let mut log_dir = None;
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        let mut value = |name: &str| {
            iter.next()
                .cloned()
                .ok_or_else(|| invalid(format!("`{name}` requires a value")))
        };
        match flag.as_str() {
            "--config" => config_path = Some(PathBuf::from(value("--config")?)),
            "--state" => state_path = Some(PathBuf::from(value("--state")?)),
            "--apply-run-id" => apply_run_id = Some(value("--apply-run-id")?),
            "--feature" => feature = Some(value("--feature")?),
            "--escalation" => escalation_kind = Some(value("--escalation")?),
            "--escalation-uid" => {
                let raw = value("--escalation-uid")?;
                let uid = raw
                    .parse::<u32>()
                    .map_err(|_| invalid(format!("invalid `--escalation-uid` value `{raw}`")))?;
                escalation_uid = Some(uid);
            }
            "--escalation-sudo-path" => {
                escalation_sudo_path = Some(PathBuf::from(value("--escalation-sudo-path")?));
            }
            "--log-dir" => log_dir = Some(PathBuf::from(value("--log-dir")?)),
            other => return Err(invalid(format!("unknown flag `{other}`"))),
        }
    }
    let escalation = match escalation_kind.as_deref() {
        Some("not-needed") => PrivilegeEscalation::NotNeeded,
        Some("sudo") => PrivilegeEscalation::Sudo {
            sudo_path: escalation_sudo_path.ok_or_else(|| {
                invalid("`--escalation sudo` requires `--escalation-sudo-path`".into())
            })?,
            uid: escalation_uid
                .ok_or_else(|| invalid("`--escalation sudo` requires `--escalation-uid`".into()))?,
        },
        Some("unavailable") => PrivilegeEscalation::Unavailable {
            uid: escalation_uid.ok_or_else(|| {
                invalid("`--escalation unavailable` requires `--escalation-uid`".into())
            })?,
        },
        Some(other) => return Err(invalid(format!("unknown escalation kind `{other}`"))),
        None => return Err(invalid("`--escalation` is required".into())),
    };
    Ok(WorkerArgs {
        config_path: config_path.ok_or_else(|| invalid("`--config` is required".into()))?,
        state_path: state_path.ok_or_else(|| invalid("`--state` is required".into()))?,
        apply_run_id: apply_run_id.ok_or_else(|| invalid("`--apply-run-id` is required".into()))?,
        feature,
        escalation,
        log_dir,
    })
}

/// Entry point for the hidden subcommand.
pub fn run_worker(args: Vec<String>) -> Result<()> {
    let args = parse_worker_args(&args)?;
    let config = Config::load_from_path(&args.config_path)?;
    let store = StateStore::open(&args.state_path)?;
    store.migrate()?;
    // Double-stamp: if the parent dies before its own stamp lands, this one keeps the row from
    // aging out through the null-pid grace window mid-install.
    store.stamp_deps_apply_child(
        &args.apply_run_id,
        i64::from(std::process::id()),
        current_boot_id().as_deref(),
        args.log_dir.as_deref().and_then(|dir| dir.to_str()),
    )?;
    let mut stdout = std::io::stdout();
    let report = apply_dependencies_tracked(
        &config,
        &store,
        TrackedApplyRun::Adopt {
            apply_run_id: &args.apply_run_id,
        },
        args.feature.as_deref(),
        &config.workspace.default_shell,
        &args.escalation,
        |current, total, name| {
            // Diagnostic only — machine-readable progress lives on the run row, so a broken log
            // pipe must not abort the install.
            if let Err(error) = writeln!(
                stdout,
                "progress: applying dependency {current}/{total}: {name}"
            ) {
                tracing::warn!(%error, "deps apply worker: failed to write progress line");
            }
            Ok(())
        },
    )?;
    if run_status_for_report(&report) == DEPS_APPLY_RUN_FAILED {
        return Err(StackError::DepsApplyFailed {
            summary: "background dependency apply produced failing actions".to_owned(),
            apply_run_id: report.apply_run_id,
            retry_command: "acps deps apply --yes",
        });
    }
    Ok(())
}

/// Spawn the detached worker, returning the child pid so the caller can stamp the run row.
#[allow(clippy::too_many_arguments)]
pub fn spawn_detached_worker(
    config_path: &std::path::Path,
    state_path: &std::path::Path,
    apply_run_id: &str,
    feature: Option<&str>,
    escalation: &PrivilegeEscalation,
    log_dir: &std::path::Path,
) -> Result<u32> {
    let current_exe = std::env::current_exe().map_err(|source| StackError::ServeIo { source })?;
    let log_path = log_dir.join(DEPS_APPLY_WORKER_LOG_FILE);
    let log_file = std::fs::File::create(&log_path).map_err(|source| StackError::ConfigWrite {
        path: log_path.clone(),
        source,
    })?;
    let log_file_stderr = log_file
        .try_clone()
        .map_err(|source| StackError::ConfigWrite {
            path: log_path,
            source,
        })?;
    let mut command = ProcessCommand::new(current_exe);
    command
        .arg(DEPS_APPLY_WORKER_SUBCOMMAND)
        .arg("--config")
        .arg(config_path)
        .arg("--state")
        .arg(state_path)
        .arg("--apply-run-id")
        .arg(apply_run_id)
        .arg("--log-dir")
        .arg(log_dir)
        .args(escalation_to_args(escalation))
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_file_stderr));
    if let Some(feature) = feature {
        command.arg("--feature").arg(feature);
    }
    // New session so the worker survives the init process and its group exiting.
    detach_into_new_session(&mut command);
    let mut child = command
        .spawn()
        .map_err(|source| StackError::ServeIo { source })?;
    let pid = child.id();
    // setsid changes session, not parenthood, so an unreaped worker sits as a zombie — and
    // `kill(pid, 0)` reports a zombie as live, blinding the abandoned-run reconcile.
    std::thread::spawn(move || match child.wait() {
        Ok(status) => {
            tracing::debug!(pid, %status, "deps apply worker exited");
        }
        Err(error) => {
            tracing::warn!(pid, %error, "deps apply worker could not be reaped");
        }
    });
    Ok(pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_args() -> Vec<String> {
        [
            "--config",
            "/tmp/acps-config.toml",
            "--state",
            "/tmp/state.sqlite",
            "--apply-run-id",
            "dap_test",
            "--escalation",
            "not-needed",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    }

    #[test]
    fn parse_worker_args_round_trips_escalation_forms() {
        let parsed = parse_worker_args(&base_args()).expect("parse");
        assert_eq!(parsed.apply_run_id, "dap_test");
        assert_eq!(parsed.escalation, PrivilegeEscalation::NotNeeded);
        assert!(parsed.feature.is_none());

        for escalation in [
            PrivilegeEscalation::NotNeeded,
            PrivilegeEscalation::Sudo {
                sudo_path: PathBuf::from("/usr/bin/sudo"),
                uid: 1000,
            },
            PrivilegeEscalation::Unavailable { uid: 1000 },
        ] {
            let mut args: Vec<String> = [
                "--config",
                "/tmp/acps-config.toml",
                "--state",
                "/tmp/state.sqlite",
                "--apply-run-id",
                "dap_test",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect();
            args.extend(escalation_to_args(&escalation));
            let parsed = parse_worker_args(&args).expect("parse");
            assert_eq!(parsed.escalation, escalation);
        }
    }

    #[test]
    fn parse_worker_args_rejects_missing_required_flags() {
        for missing in ["--config", "--state", "--apply-run-id", "--escalation"] {
            let mut args = base_args();
            let position = args
                .iter()
                .position(|arg| arg == missing)
                .expect("flag present in base args");
            args.drain(position..position + 2);
            assert!(
                parse_worker_args(&args).is_err(),
                "dropping `{missing}` must fail parsing"
            );
        }
    }

    #[test]
    fn parse_worker_args_rejects_unknown_flags() {
        let mut args = base_args();
        args.push("--surprise".to_owned());
        assert!(parse_worker_args(&args).is_err());
    }
}
