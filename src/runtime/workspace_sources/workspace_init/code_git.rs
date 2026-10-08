//! Code lane: Git-based materialization. `git clone` and `git rev-parse` run as the runtime in a
//! staging directory, capturing every subprocess, and the checkout is handed off into the workspace
//! with its symlinks preserved and a Git sentinel stamped on success.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::config::{CodeSourceConfig, derive_code_source_name};
use crate::error::{Result, StackError};
use crate::runtime::process_runner::{HostExec, forward_host_env};
use crate::secrets::SecretStore;
use crate::workload_fs::SymlinkPolicy;

use super::common::{
    MaterializeContext, Sentinel, SentinelBody, StagingDir, write_command_capture,
};
use super::{
    CAPTURE_TAG_GIT_CLONE, CAPTURE_TAG_GIT_REV_PARSE, CODE_LANE_DIR, MaterializeOutcome,
    SourceReport, WORKSPACE_STDERR_TAIL_BYTES,
};

// === CONSTANTS ===

const GIT_PROGRAM: &str = "git";
const GIT_HTTP_LOW_SPEED_LIMIT: &str = "1000";
const GIT_HTTP_LOW_SPEED_TIME_SECS: &str = "60";
/// Host variables a clone keeps despite the host-exec cleared env, so SSH remotes still
/// authenticate through the operator's agent or a custom SSH command and HTTPS remotes still
/// reach the network through the host's proxy and trust store.
const GIT_FORWARDED_HOST_ENV: &[&str] = &[
    "SSH_AUTH_SOCK",
    "GIT_SSH_COMMAND",
    "GIT_SSH",
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
    "ALL_PROXY",
    "all_proxy",
    "NO_PROXY",
    "no_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "GIT_SSL_CAINFO",
    "GIT_SSL_CAPATH",
];

// Every spawned git MUST drop these: an inherited GIT_DIR or GIT_INDEX_FILE (observed
// under a pre-commit hook) silently redirects clone/rev-parse at the launcher's
// repository. HostExec already clears the env; removing them by name keeps that true
// whatever the forwarded list grows to.
const GIT_REPO_SCOPE_ENV_VARS: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_PREFIX",
    "GIT_CONFIG_PARAMETERS",
];

fn scrub_repo_scope_env(cmd: &mut Command) {
    for var in GIT_REPO_SCOPE_ENV_VARS {
        cmd.env_remove(var);
    }
}

pub(super) fn materialize_code_source(
    index: usize,
    source: &CodeSourceConfig,
    context: &MaterializeContext,
    secrets: &SecretStore,
    log_dir: Option<&Path>,
) -> Result<SourceReport> {
    let name = derive_code_source_name(source)
        .map_err(|reason| StackError::WorkspaceCodeSourceInvalid { index, reason })?;
    let relative = Path::new(CODE_LANE_DIR).join(&name);
    let dest = context.destination.display(&relative);
    let repo = source
        .repo
        .as_deref()
        .ok_or_else(|| StackError::WorkspaceCodeSourceInvalid {
            index,
            reason: "repo is required".to_owned(),
        })?;

    if let Some(existing) = context.destination.read_sentinel(&relative)? {
        if let SentinelBody::Git {
            repo: existing_repo,
            branch: existing_branch,
            ..
        } = &existing.body
            && existing_repo == repo
            && existing_branch.as_deref() == source.branch.as_deref()
        {
            return Ok(SourceReport {
                name,
                destination: dest,
                outcome: MaterializeOutcome::Verified,
                log_dir: None,
            });
        }
        return Err(StackError::WorkspaceDestinationNotEmpty {
            dest: dest.display().to_string(),
        });
    }
    context.destination.prepare(&relative)?;

    let credential = match source.credential_ref.as_deref() {
        Some(name) => Some(secrets.get(name)?.to_owned()),
        None => None,
    };

    let staging = StagingDir::create(context.host.home())?;
    run_git_clone(
        &context.host,
        repo,
        source.branch.as_deref(),
        credential.as_deref(),
        staging.path(),
        log_dir,
    )?;
    let commit = run_git_rev_parse(&context.host, staging.path(), log_dir)?;
    let sentinel = Sentinel::new(SentinelBody::Git {
        repo: repo.to_owned(),
        branch: source.branch.clone(),
        commit,
    });
    context
        .destination
        .install(staging.path(), &relative, SymlinkPolicy::Preserve, |_| {
            sentinel
        })?;

    Ok(SourceReport {
        name,
        destination: dest,
        outcome: MaterializeOutcome::Created,
        log_dir: log_dir.map(Path::to_path_buf),
    })
}

pub(super) fn tail_stderr_bytes(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let trimmed = text.trim();
    if trimmed.len() <= WORKSPACE_STDERR_TAIL_BYTES {
        return trimmed.to_owned();
    }
    let start = trimmed.len() - WORKSPACE_STDERR_TAIL_BYTES;
    let mut cutoff = start;
    while cutoff < trimmed.len() && !trimmed.is_char_boundary(cutoff) {
        cutoff += 1;
    }
    trimmed[cutoff..].to_owned()
}

/// A git command on the host-exec env: runtime HOME, managed PATH, host-exec cwd.
fn git_command(host: &HostExec) -> Result<Command> {
    let git = host.resolve_program(GIT_PROGRAM, &[])?;
    let mut cmd = host.command(&git, &[]);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    scrub_repo_scope_env(&mut cmd);
    for name in GIT_FORWARDED_HOST_ENV {
        forward_host_env(&mut cmd, name);
    }
    // In a test build on a developer machine the system and global git config (url.insteadOf
    // rewrites) must not steer or serve a fixture clone.
    if crate::dev_gates::fixture_guards_active() {
        cmd.env("GIT_CONFIG_NOSYSTEM", "1");
        cmd.env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        );
    }
    Ok(cmd)
}

pub(super) fn run_git_clone(
    host: &HostExec,
    repo: &str,
    branch: Option<&str>,
    credential: Option<&str>,
    dest: &Path,
    log_dir: Option<&Path>,
) -> Result<()> {
    let mut cmd = git_command(host)?;
    // The handoff refuses hard-linked files, and a clone from a local path hardlinks its objects.
    cmd.arg("clone")
        .arg("--depth")
        .arg("1")
        .arg("--no-hardlinks");
    if let Some(branch) = branch {
        cmd.arg("--branch").arg(branch);
    }
    cmd.arg("--").arg(repo).arg(dest);
    cmd.env("GIT_HTTP_LOW_SPEED_LIMIT", GIT_HTTP_LOW_SPEED_LIMIT);
    cmd.env("GIT_HTTP_LOW_SPEED_TIME", GIT_HTTP_LOW_SPEED_TIME_SECS);

    // A credential travels via GIT_ASKPASS so the token never lands in process args,
    // where ps and audit logs would expose it.
    if let Some(token) = credential {
        cmd.env("ACP_STACK_GIT_TOKEN", token);
        let helper_path = write_askpass_helper()?;
        cmd.env("GIT_ASKPASS", &helper_path);
    }

    let output = cmd
        .output()
        .map_err(|source| StackError::WorkspaceMaterializeFailed {
            reason: format!("spawning `git clone` failed: {source}"),
        })?;
    // Written before the exit-status check so a failed clone is auditable without
    // re-running.
    write_command_capture(
        log_dir,
        CAPTURE_TAG_GIT_CLONE,
        &output.stdout,
        &output.stderr,
    )?;
    if !output.status.success() {
        return Err(StackError::WorkspaceCommandFailed {
            command: "git clone",
            exit: output.status.code(),
            stderr_tail: tail_stderr_bytes(&output.stderr),
        });
    }
    Ok(())
}

pub(super) fn run_git_rev_parse(
    host: &HostExec,
    repo_dir: &Path,
    log_dir: Option<&Path>,
) -> Result<String> {
    let mut cmd = git_command(host)?;
    cmd.arg("rev-parse").arg("HEAD").current_dir(repo_dir);
    let output = cmd
        .output()
        .map_err(|source| StackError::WorkspaceMaterializeFailed {
            reason: format!("spawning `git rev-parse` failed: {source}"),
        })?;
    write_command_capture(
        log_dir,
        CAPTURE_TAG_GIT_REV_PARSE,
        &output.stdout,
        &output.stderr,
    )?;
    if !output.status.success() {
        return Err(StackError::WorkspaceCommandFailed {
            command: "git rev-parse HEAD",
            exit: output.status.code(),
            stderr_tail: tail_stderr_bytes(&output.stderr),
        });
    }
    let commit = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if commit.is_empty() {
        return Err(StackError::WorkspaceCommandFailed {
            command: "git rev-parse HEAD",
            exit: output.status.code(),
            stderr_tail: "command produced no output".to_owned(),
        });
    }
    Ok(commit)
}

pub(super) fn write_askpass_helper() -> Result<PathBuf> {
    let dir =
        tempfile::TempDir::new().map_err(|source| StackError::WorkspaceMaterializeFailed {
            reason: format!("create askpass tempdir: {source}"),
        })?;
    // Leak the TempDir so the helper survives long enough for `git` to exec it;
    // acceptable in the one-shot `acps init` CLI.
    let path = dir.keep().join("askpass.sh");
    let script = "#!/bin/sh\nprintf %s \"$ACP_STACK_GIT_TOKEN\"\n";
    std::fs::write(&path, script).map_err(|source| StackError::WorkspaceMaterializeFailed {
        reason: format!("write askpass `{}`: {source}", path.display()),
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path)
            .map_err(|source| StackError::WorkspaceMaterializeFailed {
                reason: format!("stat askpass `{}`: {source}", path.display()),
            })?
            .permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(&path, perms).map_err(|source| {
            StackError::WorkspaceMaterializeFailed {
                reason: format!("chmod askpass `{}`: {source}", path.display()),
            }
        })?;
    }
    Ok(path)
}
