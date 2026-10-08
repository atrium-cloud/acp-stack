//! Moves a runtime-staged tree into a workload-owned destination. The runtime side walks the source
//! without following symlinks and streams entries, files as already-open descriptors, to a job run
//! by the executor, which recreates each entry through the no-follow walker.

use super::*;

use std::ffi::{CStr, OsStr};
use std::fs::File;
use std::os::fd::{AsFd as _, BorrowedFd};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};

// === CONSTANTS ===

const HANDOFF_CHANNEL_CAPACITY: usize = 64;
const SOURCE_WALKER_THREAD_NAME: &str = "acps-handoff-source";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SymlinkPolicy {
    /// Recreate source symlinks verbatim; the walker never traverses them afterwards.
    Preserve,
    /// Refuse a source tree that contains a symlink.
    Reject,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HandoffOptions {
    pub symlinks: SymlinkPolicy,
    /// Accept source files with more than one link. Refused with a workload identity, where a link
    /// planted in a workload-writable source could alias a file only the runtime can read.
    pub hard_links: bool,
    /// Copied files keep their source permission bits within this mask.
    pub file_mode: u32,
    pub dir_mode: u32,
    /// Bound for the destination-side job.
    pub timeout: Duration,
}

/// The regular files a handoff copied and their size when opened.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HandoffSummary {
    pub files: u64,
    pub bytes: u64,
}

enum HandoffEntry {
    Dir {
        relative: PathBuf,
    },
    File {
        relative: PathBuf,
        file: File,
        mode: u32,
        size: u64,
    },
    Symlink {
        relative: PathBuf,
        target: PathBuf,
    },
    /// The source walk finished; a channel that closes without it means the walk aborted.
    End,
}

/// Copy the tree at `source` into `dest_rel` below `dest_anchor`. The destination must not exist or
/// must be an empty directory. The destination side runs under `executor`; the source side runs
/// with the process's own credentials. On failure the destination is removed, or emptied when it
/// existed beforehand, so a retry starts clean.
pub fn handoff_tree(
    executor: &Executor,
    source: &Path,
    dest_anchor: &Path,
    dest_rel: &Path,
    options: &HandoffOptions,
) -> Result<HandoffSummary> {
    let source_anchor = Anchor::open(source)?;
    let (sender, receiver) = sync_channel(HANDOFF_CHANNEL_CAPACITY);
    let policy = options.symlinks;
    let hard_links = options.hard_links;
    let walker = std::thread::Builder::new()
        .name(SOURCE_WALKER_THREAD_NAME.to_owned())
        .spawn(move || send_source_entries(&source_anchor, policy, hard_links, &sender))
        .map_err(|source| StackError::WorkloadFsExecutorFailed {
            reason: format!("failed to spawn the handoff source walker: {source}"),
        })?;
    let anchor_path = dest_anchor.to_path_buf();
    let destination = dest_rel.to_path_buf();
    let destination_options = *options;
    let received = executor.run(options.timeout, move || {
        receive_into_destination(&anchor_path, &destination, &destination_options, receiver)
    });
    if matches!(received, Err(StackError::WorkloadFsTimeout { .. })) {
        // The walker may be blocked sending to the abandoned destination job, so joining it could
        // outlast the timeout.
        drop(walker);
        return received;
    }
    let walked = walker
        .join()
        .map_err(|_| StackError::WorkloadFsExecutorFailed {
            reason: "the handoff source walker panicked".to_owned(),
        })?;
    // A failed walk also aborts the destination job, so the walker's error is the root cause.
    walked.and(received)
}

/// Pre-order walk, so every directory reaches the destination before its contents.
fn send_source_entries(
    source: &Anchor,
    policy: SymlinkPolicy,
    hard_links: bool,
    sender: &SyncSender<HandoffEntry>,
) -> Result<()> {
    let mut pending = vec![PathBuf::new()];
    while let Some(relative) = pending.pop() {
        // Reopened from the anchor per directory so a wide tree never holds one descriptor per
        // pending directory.
        let components = walk::components_of(source.path(), &relative)?;
        let directory = walk::open_directory_chain(source, &components, None)?;
        let display = source.path().join(&relative);
        let mut names = walk::read_names(directory.as_fd(), &display)?;
        names.sort();
        for name in names {
            let entry_relative = relative.join(OsStr::from_bytes(name.as_bytes()));
            let entry = source_entry(
                source,
                directory.as_fd(),
                &name,
                entry_relative,
                policy,
                hard_links,
            )?;
            if let HandoffEntry::Dir { relative } = &entry {
                pending.push(relative.clone());
            }
            if sender.send(entry).is_err() {
                // The destination job stopped early and reports its own error.
                return Ok(());
            }
        }
    }
    if sender.send(HandoffEntry::End).is_err() {
        tracing::debug!("handoff destination job stopped before the source walk finished");
    }
    Ok(())
}

fn source_entry(
    source: &Anchor,
    directory: BorrowedFd<'_>,
    name: &CStr,
    relative: PathBuf,
    policy: SymlinkPolicy,
    hard_links: bool,
) -> Result<HandoffEntry> {
    let display = source.path().join(&relative);
    let status = walk::lstat_at(directory, name)
        .map_err(|error| walk::failure(&display, "fstatat", error))?
        .ok_or_else(|| StackError::WorkloadFsNotFound {
            path: display.clone(),
        })?;
    match status.kind {
        EntryKind::Dir => Ok(HandoffEntry::Dir { relative }),
        EntryKind::File => {
            let (descriptor, status) =
                walk::open_regular_file(directory, name, &display, hard_links)?;
            Ok(HandoffEntry::File {
                relative,
                file: File::from(descriptor),
                mode: status.mode,
                size: status.size,
            })
        }
        EntryKind::Symlink => match policy {
            SymlinkPolicy::Preserve => {
                let target = walk::read_link_at(directory, name)
                    .map_err(|error| walk::failure(&display, "readlinkat", error))?;
                Ok(HandoffEntry::Symlink { relative, target })
            }
            SymlinkPolicy::Reject => Err(StackError::WorkloadFsSymlinkRefused { path: display }),
        },
        EntryKind::Other => Err(StackError::WorkloadFsNotRegular { path: display }),
    }
}

fn receive_into_destination(
    anchor_path: &Path,
    destination: &Path,
    options: &HandoffOptions,
    receiver: Receiver<HandoffEntry>,
) -> Result<HandoffSummary> {
    let anchor = Anchor::open(anchor_path)?;
    let created = prepare_destination(&anchor, destination, options.dir_mode)?;
    let received = create_entries(&anchor, destination, options, &receiver);
    // Release a walker blocked on a full channel before cleaning up.
    drop(receiver);
    if received.is_err() {
        discard_destination(&anchor, destination, created);
    }
    received
}

/// Returns whether this job created the destination directory.
fn prepare_destination(anchor: &Anchor, destination: &Path, dir_mode: u32) -> Result<bool> {
    let display = anchor.path().join(destination);
    match stat(anchor, destination)? {
        None => {
            create_dir_all(anchor, destination, dir_mode)?;
            Ok(true)
        }
        Some(info) => match info.kind {
            EntryKind::Dir if list_dir(anchor, destination)?.is_empty() => Ok(false),
            EntryKind::Symlink => Err(StackError::WorkloadFsSymlinkRefused { path: display }),
            _ => Err(StackError::WorkloadFsDestinationNotEmpty { path: display }),
        },
    }
}

fn create_entries(
    anchor: &Anchor,
    destination: &Path,
    options: &HandoffOptions,
    receiver: &Receiver<HandoffEntry>,
) -> Result<HandoffSummary> {
    let mut summary = HandoffSummary::default();
    for entry in receiver.iter() {
        match entry {
            HandoffEntry::Dir { relative } => {
                walk::create_directory(anchor, &destination.join(relative), options.dir_mode)?;
            }
            HandoffEntry::File {
                relative,
                mut file,
                mode,
                size,
            } => {
                let write_options = WriteOptions {
                    create_parents: false,
                    file_mode: mode & options.file_mode,
                    dir_mode: options.dir_mode,
                    require_existing_owner: None,
                };
                walk::write_new_from_reader(
                    anchor,
                    &destination.join(relative),
                    &mut file,
                    &write_options,
                )?;
                summary.files += 1;
                summary.bytes = summary.bytes.saturating_add(size);
            }
            HandoffEntry::Symlink { relative, target } => {
                symlink(anchor, &destination.join(relative), &target)?;
            }
            HandoffEntry::End => return Ok(summary),
        }
    }
    Err(StackError::WorkloadFsExecutorFailed {
        reason: "the handoff source walk ended without completing".to_owned(),
    })
}

fn discard_destination(anchor: &Anchor, destination: &Path, created: bool) {
    let discarded = if created {
        remove_tree(anchor, destination)
    } else {
        clear_directory(anchor, destination)
    };
    if let Err(error) = discarded {
        tracing::warn!(%error, "failed to discard a partial workload handoff destination");
    }
}

fn clear_directory(anchor: &Anchor, destination: &Path) -> Result<()> {
    for (name, _) in list_dir(anchor, destination)? {
        remove_tree(anchor, &destination.join(name))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    const FILE_MODE_MASK: u32 = 0o750;

    fn options(symlinks: SymlinkPolicy) -> HandoffOptions {
        HandoffOptions {
            symlinks,
            hard_links: false,
            file_mode: FILE_MODE_MASK,
            dir_mode: OWNER_ONLY_DIR_MODE,
            timeout: DEFAULT_JOB_TIMEOUT,
        }
    }

    fn mode_of(path: &Path) -> u32 {
        std::fs::symlink_metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777
    }

    fn staged_tree() -> tempfile::TempDir {
        let source = tempfile::tempdir().expect("source");
        let root = source.path();
        std::fs::create_dir_all(root.join("bin")).expect("bin");
        std::fs::create_dir_all(root.join("docs/nested")).expect("docs");
        std::fs::write(root.join("bin/tool"), b"#!/bin/sh\necho tool\n").expect("tool");
        std::fs::set_permissions(
            root.join("bin/tool"),
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("chmod tool");
        std::fs::write(root.join("docs/readme.md"), b"# readme\n").expect("readme");
        std::fs::write(root.join("docs/nested/deep.txt"), b"deep").expect("deep");
        source
    }

    #[test]
    fn handoff_copies_the_tree_keeping_source_permissions_within_the_mask() {
        let source = staged_tree();
        let destination = tempfile::tempdir().expect("destination");

        handoff_tree(
            &Executor::Process,
            source.path(),
            destination.path(),
            Path::new("workspace/src"),
            &options(SymlinkPolicy::Reject),
        )
        .expect("handoff");

        let root = destination.path().join("workspace/src");
        assert_eq!(
            std::fs::read(root.join("bin/tool")).expect("tool"),
            b"#!/bin/sh\necho tool\n"
        );
        assert_eq!(
            std::fs::read(root.join("docs/nested/deep.txt")).expect("deep"),
            b"deep"
        );
        assert_eq!(mode_of(&root.join("bin/tool")), 0o755 & FILE_MODE_MASK);
        assert_eq!(
            mode_of(&root.join("docs/readme.md")),
            mode_of(&source.path().join("docs/readme.md")) & FILE_MODE_MASK
        );
        assert_eq!(mode_of(&root.join("docs/nested")), OWNER_ONLY_DIR_MODE);
    }

    #[test]
    fn handoff_preserves_symlinks_verbatim() {
        let source = staged_tree();
        std::os::unix::fs::symlink("docs/readme.md", source.path().join("readme")).expect("link");
        std::os::unix::fs::symlink("/nonexistent/target", source.path().join("docs/dangling"))
            .expect("dangling");
        let destination = tempfile::tempdir().expect("destination");

        handoff_tree(
            &Executor::Process,
            source.path(),
            destination.path(),
            Path::new("tree"),
            &options(SymlinkPolicy::Preserve),
        )
        .expect("handoff");

        let root = destination.path().join("tree");
        assert_eq!(
            std::fs::read_link(root.join("readme")).expect("readme link"),
            PathBuf::from("docs/readme.md")
        );
        assert_eq!(
            std::fs::read_link(root.join("docs/dangling")).expect("dangling link"),
            PathBuf::from("/nonexistent/target")
        );
    }

    #[test]
    fn handoff_rejects_symlinks_and_discards_the_partial_destination() {
        let source = staged_tree();
        std::os::unix::fs::symlink("docs/readme.md", source.path().join("readme")).expect("link");
        let destination = tempfile::tempdir().expect("destination");

        let error = handoff_tree(
            &Executor::Process,
            source.path(),
            destination.path(),
            Path::new("tree"),
            &options(SymlinkPolicy::Reject),
        )
        .expect_err("symlink rejected");

        assert!(matches!(error, StackError::WorkloadFsSymlinkRefused { .. }));
        assert!(!destination.path().join("tree").exists());
    }

    #[test]
    fn handoff_refuses_a_non_empty_destination_and_accepts_an_empty_one() {
        let source = staged_tree();
        let destination = tempfile::tempdir().expect("destination");
        std::fs::create_dir(destination.path().join("occupied")).expect("occupied");
        std::fs::write(destination.path().join("occupied/keep"), b"keep").expect("keep");
        std::fs::create_dir(destination.path().join("empty")).expect("empty");

        let error = handoff_tree(
            &Executor::Process,
            source.path(),
            destination.path(),
            Path::new("occupied"),
            &options(SymlinkPolicy::Reject),
        )
        .expect_err("non-empty destination");
        assert!(matches!(
            error,
            StackError::WorkloadFsDestinationNotEmpty { .. }
        ));
        assert_eq!(
            std::fs::read(destination.path().join("occupied/keep")).expect("keep"),
            b"keep"
        );

        handoff_tree(
            &Executor::Process,
            source.path(),
            destination.path(),
            Path::new("empty"),
            &options(SymlinkPolicy::Reject),
        )
        .expect("empty destination");
        assert!(destination.path().join("empty/bin/tool").is_file());
    }

    #[test]
    fn handoff_refuses_hard_links_in_the_source() {
        let source = staged_tree();
        std::fs::hard_link(
            source.path().join("docs/readme.md"),
            source.path().join("docs/second"),
        )
        .expect("hard link");
        let destination = tempfile::tempdir().expect("destination");

        let error = handoff_tree(
            &Executor::Process,
            source.path(),
            destination.path(),
            Path::new("tree"),
            &options(SymlinkPolicy::Preserve),
        )
        .expect_err("hard link refused");
        assert!(matches!(
            error,
            StackError::WorkloadFsHardLinkRefused { .. }
        ));
        assert!(!destination.path().join("tree").exists());

        handoff_tree(
            &Executor::Process,
            source.path(),
            destination.path(),
            Path::new("tree"),
            &HandoffOptions {
                hard_links: true,
                ..options(SymlinkPolicy::Preserve)
            },
        )
        .expect("hard links accepted when allowed");
        assert_eq!(
            std::fs::read(destination.path().join("tree/docs/second")).expect("second"),
            b"# readme\n"
        );
    }

    #[test]
    fn destination_refuses_an_entry_routed_through_a_created_symlink() {
        let destination = tempfile::tempdir().expect("destination");
        let outside = tempfile::tempdir().expect("outside");
        let payload_source = tempfile::NamedTempFile::new().expect("payload");
        std::fs::write(payload_source.path(), b"payload").expect("payload content");

        let (sender, receiver) = sync_channel(HANDOFF_CHANNEL_CAPACITY);
        let entries = vec![
            HandoffEntry::Dir {
                relative: PathBuf::from("dir"),
            },
            HandoffEntry::Symlink {
                relative: PathBuf::from("escape"),
                target: outside.path().to_path_buf(),
            },
            HandoffEntry::File {
                relative: PathBuf::from("escape/payload"),
                file: File::open(payload_source.path()).expect("open payload"),
                mode: 0o644,
                size: 0,
            },
            HandoffEntry::End,
        ];
        for entry in entries {
            sender.send(entry).expect("queue entry");
        }

        let error = receive_into_destination(
            destination.path(),
            Path::new("tree"),
            &options(SymlinkPolicy::Preserve),
            receiver,
        )
        .expect_err("routed through a symlink");

        assert!(matches!(error, StackError::WorkloadFsSymlinkRefused { .. }));
        assert!(!outside.path().join("payload").exists());
        assert!(!destination.path().join("tree").exists());
    }
}
