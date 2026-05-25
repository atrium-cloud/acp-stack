//! Code lane: Git-based materialization (`git clone` + `git rev-parse`).
//!
//! Drives the host `git` binary in a non-interactive mode, persists every
//! subprocess capture under the per-source log directory, and stamps a
//! Git-flavored sentinel onto the destination on success.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::config::{CodeSourceConfig, derive_code_source_name};
use crate::error::{Result, StackError};
use crate::secrets::SecretStore;

use super::common::{
    Sentinel, SentinelBody, cleanup_partial_destination, destination_is_empty_except_sentinel,
    ensure_destination_not_symlink, write_command_capture,
};
use super::{
    CAPTURE_TAG_GIT_CLONE, CAPTURE_TAG_GIT_REV_PARSE, MaterializeOutcome, SourceReport,
    WORKSPACE_STDERR_TAIL_BYTES,
};

pub(super) fn materialize_code_source(
    index: usize,
    source: &CodeSourceConfig,
    code_root: &Path,
    secrets: &SecretStore,
    log_dir: Option<&Path>,
) -> Result<SourceReport> {
    let name = derive_code_source_name(source)
        .map_err(|reason| StackError::WorkspaceCodeSourceInvalid { index, reason })?;
    let dest = code_root.join(&name);
    ensure_destination_not_symlink(&dest)?;
    let repo = source
        .repo
        .as_deref()
        .ok_or_else(|| StackError::WorkspaceCodeSourceInvalid {
            index,
            reason: "repo is required".to_owned(),
        })?;

    if dest.exists() {
        if let Some(existing) = Sentinel::read(&dest)? {
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
        if !destination_is_empty_except_sentinel(&dest)? {
            return Err(StackError::WorkspaceDestinationNotEmpty {
                dest: dest.display().to_string(),
            });
        }
    }

    std::fs::create_dir_all(&dest).map_err(|source_err| {
        StackError::WorkspaceMaterializeFailed {
            reason: format!("create dest `{}`: {source_err}", dest.display()),
        }
    })?;

    let credential = match source.credential_ref.as_deref() {
        Some(name) => Some(secrets.get(name)?.to_owned()),
        None => None,
    };

    let outcome = run_git_clone(
        repo,
        source.branch.as_deref(),
        credential.as_deref(),
        &dest,
        log_dir,
    );
    if let Err(err) = outcome {
        // Clean up the partially-clobbered destination so a rerun can retry.
        return Err(cleanup_partial_destination(&dest, err));
    }

    // Everything from here to the sentinel write is "after a successful
    // clone but before the source is durably marked done". A failure
    // here — git rev-parse, the per-step log writes that now run inside
    // it, or the sentinel write itself — leaves a populated destination
    // without a sentinel, which the next `acps init` would reject under
    // the non-empty destination guard. Funnel every failure through
    // `cleanup_partial_destination` so a retry can proceed.
    let commit = match run_git_rev_parse(&dest, log_dir) {
        Ok(commit) => commit,
        Err(err) => return Err(cleanup_partial_destination(&dest, err)),
    };
    let sentinel = Sentinel::new(SentinelBody::Git {
        repo: repo.to_owned(),
        branch: source.branch.clone(),
        commit,
    });
    if let Err(err) = sentinel.write(&dest) {
        return Err(cleanup_partial_destination(&dest, err));
    }

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

pub(super) fn run_git_clone(
    repo: &str,
    branch: Option<&str>,
    credential: Option<&str>,
    dest: &Path,
    log_dir: Option<&Path>,
) -> Result<()> {
    let mut cmd = Command::new("git");
    cmd.arg("clone").arg("--depth").arg("1");
    if let Some(branch) = branch {
        cmd.arg("--branch").arg(branch);
    }
    cmd.arg("--").arg(repo).arg(dest);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    // Force non-interactive auth. If we pass a credential, it goes via
    // GIT_ASKPASS so the token never lands in process args (which would
    // leak through ps/audit logs). The askpass helper just echoes the
    // token; this only fires when libcurl asks for a password, so
    // unauthenticated clones still work.
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd.env("GIT_HTTP_LOW_SPEED_LIMIT", "1000");
    cmd.env("GIT_HTTP_LOW_SPEED_TIME", "60");
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
    // Persist captured streams to disk so a failed clone is auditable
    // without re-running. Successful clones land the same capture so the
    // operator can inspect what git actually did. Write before the
    // exit-status check; the failure tail in `WorkspaceCommandFailed`
    // remains the primary error surface, but the full capture is the
    // audit copy.
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

pub(super) fn run_git_rev_parse(dest: &Path, log_dir: Option<&Path>) -> Result<String> {
    let output = Command::new("git")
        .arg("rev-parse")
        .arg("HEAD")
        .current_dir(dest)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
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
    // Leak the TempDir so the askpass helper survives long enough for
    // `git` to exec it. Acceptable in `acps init` (one-shot CLI). Newer
    // `tempfile` deprecates `into_path` in favor of `keep`.
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
