//! Binary swap for the self-updater: stage inside the install directory so the swap
//! is a rename, then roll the previous binaries back on any failure past the first.

use super::*;

pub(super) fn install_archive(bytes: &[u8], binary_dir: &Path) -> Result<()> {
    let stage = tempfile::Builder::new()
        .prefix("acp-stack-update-")
        .tempdir_in(binary_dir)
        .map_err(|source| StackError::DirectoryCreate {
            path: binary_dir.to_path_buf(),
            source,
        })?;
    let decoder = flate2::read::GzDecoder::new(Cursor::new(bytes));
    let mut archive = tar::Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|source| StackError::GithubReleaseArchiveExtract {
            repo: REPOSITORY.to_owned(),
            reason: format!("failed to read release archive: {source}"),
        })?;
    let mut found = Vec::new();
    for entry in entries {
        let mut entry = entry.map_err(|source| StackError::GithubReleaseArchiveExtract {
            repo: REPOSITORY.to_owned(),
            reason: format!("failed to read archive entry: {source}"),
        })?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry
            .path()
            .map_err(|source| StackError::GithubReleaseArchiveExtract {
                repo: REPOSITORY.to_owned(),
                reason: format!("failed to read archive entry path: {source}"),
            })?
            .into_owned();
        let Some(leaf) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !BINARIES.contains(&leaf) || found.iter().any(|binary| binary == leaf) {
            continue;
        }
        let dest = stage.path().join(leaf);
        entry
            .unpack(&dest)
            .map_err(|source| StackError::GithubReleaseArchiveExtract {
                repo: REPOSITORY.to_owned(),
                reason: format!("failed to extract `{leaf}` from release archive: {source}"),
            })?;
        found.push(leaf.to_owned());
    }
    for binary in BINARIES {
        let staged = stage.path().join(binary);
        if !found.iter().any(|found| found.as_str() == *binary) || !staged.is_file() {
            return Err(StackError::GithubReleaseArchiveExtract {
                repo: REPOSITORY.to_owned(),
                reason: format!("release archive missing regular-file `{binary}`"),
            });
        }
        set_executable(&staged)?;
    }
    replace_binaries(stage.path(), binary_dir)?;
    Ok(())
}

fn replace_binaries(stage: &Path, binary_dir: &Path) -> Result<()> {
    let backups = tempfile::Builder::new()
        .prefix("acp-stack-update-backup-")
        .tempdir_in(binary_dir)
        .map_err(|source| StackError::DirectoryCreate {
            path: binary_dir.to_path_buf(),
            source,
        })?;
    let mut backed_up: Vec<(PathBuf, PathBuf)> = Vec::new();
    for binary in BINARIES {
        let dest = binary_dir.join(binary);
        let backup = backups.path().join(binary);
        if let Err(source) = fs::rename(&dest, &backup) {
            return Err(rollback_and_report(dest, source, &[], &backed_up));
        }
        backed_up.push((dest, backup));
    }
    for binary in REMOVED_BINARIES {
        let dest = binary_dir.join(binary);
        let backup = backups.path().join(binary);
        match fs::symlink_metadata(&dest) {
            Ok(_) => {
                if let Err(source) = fs::rename(&dest, &backup) {
                    return Err(rollback_and_report(dest, source, &[], &backed_up));
                }
                backed_up.push((dest, backup));
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(rollback_and_report(dest, source, &[], &backed_up));
            }
        }
    }

    let mut installed = Vec::new();
    for binary in BINARIES {
        let staged = stage.join(binary);
        let dest = binary_dir.join(binary);
        if let Err(source) = fs::rename(&staged, &dest) {
            return Err(rollback_and_report(dest, source, &installed, &backed_up));
        }
        installed.push(dest);
    }
    Ok(())
}

/// Undo whatever the swap managed to do, then describe the original failure. Every
/// abort path in `replace_binaries` MUST go through here, or a partially swapped
/// install directory outlives the error.
fn rollback_and_report(
    path: PathBuf,
    source: std::io::Error,
    installed: &[PathBuf],
    backed_up: &[(PathBuf, PathBuf)],
) -> StackError {
    let rollback_errors = rollback_binary_swap(installed, backed_up);
    StackError::StackUpdateBinarySwap {
        path,
        source,
        rollback_errors,
    }
}

fn rollback_binary_swap(installed: &[PathBuf], backed_up: &[(PathBuf, PathBuf)]) -> Vec<String> {
    let mut errors = Vec::new();
    for dest in installed.iter().rev() {
        if dest.exists()
            && let Err(err) = fs::remove_file(dest)
        {
            errors.push(format!("failed to remove {}: {err}", dest.display()));
        }
    }
    for (dest, backup) in backed_up.iter().rev() {
        if backup.exists()
            && let Err(err) = fs::rename(backup, dest)
        {
            errors.push(format!(
                "failed to restore {} from {}: {err}",
                dest.display(),
                backup.display()
            ));
        }
    }
    errors
}

pub(super) fn install_binary_dir() -> Result<PathBuf> {
    // Test seam: `fixture_path` returns `None` unless built with `test-fixtures`,
    // so production always falls through to `current_exe`.
    if let Some(dir) = fixture_path(INSTALL_BINARY_DIR_ENV) {
        return Ok(dir);
    }
    let exe = std::env::current_exe().map_err(|source| StackError::ConfigRead {
        path: PathBuf::from("current_exe"),
        source,
    })?;
    exe.parent()
        .map(Path::to_path_buf)
        .ok_or(StackError::MissingParentDir { path: exe })
}

pub(super) fn directory_is_writable(path: &Path) -> bool {
    let probe = path.join(format!(".acps-update-write-test-{}", std::process::id()));
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = fs::remove_file(probe);
            true
        }
        Err(_) => false,
    }
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)
        .map_err(|source| StackError::ConfigRead {
            path: path.to_path_buf(),
            source,
        })?
        .permissions();
    perms.set_mode(perms.mode() | 0o755);
    fs::set_permissions(path, perms).map_err(|source| StackError::PermissionSet {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}
