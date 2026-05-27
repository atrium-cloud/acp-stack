//! Safe archive extractor for untrusted tarballs/zips.
//!
//! Built for Phase 4 workspace data ingestion: take an archive downloaded
//! from an untrusted source and unpack it into a destination directory,
//! rejecting every entry that could escape the destination, mask kernel
//! features (devices, FIFOs), or run away on size.
//!
//! Supported formats: tar, tar.gz, zip. Format is detected by magic bytes,
//! not by file extension, so a renamed archive cannot smuggle its way
//! through. Unsupported formats fail fast with `ArchiveUnsupportedFormat`.
//!
//! Safety rules enforced for every entry:
//!
//! * No `..` segments, no absolute paths, no root or drive prefixes.
//! * No symlinks, no hardlinks. (These are how tar escape exploits work.)
//! * No FIFOs, character/block devices, or other special types.
//! * Per-entry size cap and cumulative extracted size cap. Streams abort
//!   mid-entry when either is hit.

use std::fs::File;
use std::io::{BufReader, Read, Seek, Write};
use std::path::{Component, Path, PathBuf};

use flate2::read::GzDecoder;
use tar::{Archive as TarArchive, EntryType};

use crate::error::{Result, StackError};

const DEFAULT_MAX_TOTAL_BYTES: u64 = 1024 * 1024 * 1024; // 1 GiB
const DEFAULT_MAX_ENTRY_BYTES: u64 = 512 * 1024 * 1024; // 512 MiB
const COPY_CHUNK_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone)]
pub struct ExtractOpts {
    pub max_total_bytes: u64,
    pub max_entry_bytes: u64,
}

impl Default for ExtractOpts {
    fn default() -> Self {
        Self {
            max_total_bytes: DEFAULT_MAX_TOTAL_BYTES,
            max_entry_bytes: DEFAULT_MAX_ENTRY_BYTES,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExtractReport {
    pub entries_written: usize,
    pub bytes_written: u64,
    /// `Some(name)` when every extracted entry shared the same top-level
    /// directory; otherwise `None`. Callers can use this to flatten a single
    /// top-level wrapper into the destination's parent name.
    pub top_level_dir: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetectedFormat {
    TarGz,
    Tar,
    Zip,
}

/// Extract `archive` into `dest`. The destination is created if missing; it
/// must be empty if it already exists, and must not be (or contain) a
/// symlink — `File::create` would otherwise happily follow a swap-in
/// symlink directory and write entries outside `dest`.
pub fn extract_archive(archive: &Path, dest: &Path, opts: &ExtractOpts) -> Result<ExtractReport> {
    let format = detect_format(archive)?;
    enforce_dest_invariants(dest)?;
    std::fs::create_dir_all(dest).map_err(|source| StackError::ArchiveReadFailed {
        reason: format!(
            "could not create destination `{}`: {source}",
            dest.display()
        ),
    })?;

    let dest_canonical = dest
        .canonicalize()
        .map_err(|source| StackError::ArchiveReadFailed {
            reason: format!(
                "could not canonicalize destination `{}`: {source}",
                dest.display()
            ),
        })?;

    match format {
        DetectedFormat::TarGz => extract_tar(archive, &dest_canonical, opts, true),
        DetectedFormat::Tar => extract_tar(archive, &dest_canonical, opts, false),
        DetectedFormat::Zip => extract_zip(archive, &dest_canonical, opts),
    }
}

fn enforce_dest_invariants(dest: &Path) -> Result<()> {
    let metadata = match std::fs::symlink_metadata(dest) {
        Ok(m) => m,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(StackError::ArchiveReadFailed {
                reason: format!("stat dest `{}`: {source}", dest.display()),
            });
        }
    };
    if metadata.file_type().is_symlink() {
        return Err(StackError::ArchiveUnsafeEntry {
            kind: "destination-symlink",
            name: dest.display().to_string(),
        });
    }
    if !metadata.is_dir() {
        return Err(StackError::ArchiveReadFailed {
            reason: format!(
                "destination `{}` exists and is not a directory",
                dest.display()
            ),
        });
    }
    let mut read_dir = std::fs::read_dir(dest).map_err(|source| StackError::ArchiveReadFailed {
        reason: format!("read_dir `{}`: {source}", dest.display()),
    })?;
    if read_dir
        .next()
        .transpose()
        .map_err(|source| StackError::ArchiveReadFailed {
            reason: format!("read_dir entry `{}`: {source}", dest.display()),
        })?
        .is_some()
    {
        return Err(StackError::ArchiveReadFailed {
            reason: format!("destination `{}` is not empty", dest.display()),
        });
    }
    Ok(())
}

fn detect_format(archive: &Path) -> Result<DetectedFormat> {
    let mut file = File::open(archive).map_err(|source| StackError::ArchiveReadFailed {
        reason: format!("could not open `{}`: {source}", archive.display()),
    })?;
    let mut head = [0u8; 264];
    let read = file
        .read(&mut head)
        .map_err(|source| StackError::ArchiveReadFailed {
            reason: format!("could not read header: {source}"),
        })?;
    let head = &head[..read];
    if head.starts_with(&[0x1f, 0x8b]) {
        return Ok(DetectedFormat::TarGz);
    }
    if head.starts_with(b"PK\x03\x04") || head.starts_with(b"PK\x05\x06") {
        return Ok(DetectedFormat::Zip);
    }
    // tar magic at offset 257: "ustar" (POSIX) or "ustar\0" (GNU) — both
    // start with "ustar". Without that, treat as unsupported (we do not
    // accept ambiguous "plain stream" tars to avoid false positives).
    if head.len() >= 263 && &head[257..262] == b"ustar" {
        return Ok(DetectedFormat::Tar);
    }
    Err(StackError::ArchiveUnsupportedFormat)
}

fn extract_tar(
    archive: &Path,
    dest: &Path,
    opts: &ExtractOpts,
    gzipped: bool,
) -> Result<ExtractReport> {
    let file = File::open(archive).map_err(|source| StackError::ArchiveReadFailed {
        reason: format!("could not open `{}`: {source}", archive.display()),
    })?;
    let buffered = BufReader::new(file);
    let mut archive: Box<dyn Read> = if gzipped {
        Box::new(GzDecoder::new(buffered))
    } else {
        Box::new(buffered)
    };
    // The tar crate accepts `Read` but its high-level API consumes the
    // archive value, so we route through it directly.
    let mut tar = TarArchive::new(&mut archive);

    let mut total_bytes: u64 = 0;
    let mut entries_written = 0usize;
    let mut top_levels: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    let entries = tar
        .entries()
        .map_err(|source| StackError::ArchiveReadFailed {
            reason: format!("tar entries: {source}"),
        })?;

    for entry in entries {
        let mut entry = entry.map_err(|source| StackError::ArchiveReadFailed {
            reason: format!("tar entry: {source}"),
        })?;
        let raw_path = entry
            .path()
            .map_err(|source| StackError::ArchiveReadFailed {
                reason: format!("tar entry path: {source}"),
            })?
            .into_owned();
        let entry_type = entry.header().entry_type();
        reject_unsafe_entry(entry_type, &raw_path)?;
        let safe_path = sanitize_relative_path(&raw_path)?;
        if is_tar_metadata_entry(entry_type) {
            continue;
        }
        if let Some(top) = safe_path.components().next().and_then(|c| match c {
            Component::Normal(name) => name.to_str(),
            _ => None,
        }) {
            top_levels.insert(top.to_owned());
        }
        let target = dest.join(&safe_path);
        ensure_inside(&target, dest, &raw_path)?;

        match entry_type {
            EntryType::Directory => {
                std::fs::create_dir_all(&target).map_err(|source| {
                    StackError::ArchiveReadFailed {
                        reason: format!("create dir `{}`: {source}", target.display()),
                    }
                })?;
            }
            EntryType::Regular | EntryType::Continuous | EntryType::GNUSparse => {
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).map_err(|source| {
                        StackError::ArchiveReadFailed {
                            reason: format!("create dir `{}`: {source}", parent.display()),
                        }
                    })?;
                }
                let written = copy_capped(
                    &mut entry,
                    &target,
                    opts.max_entry_bytes,
                    opts.max_total_bytes - total_bytes,
                )?;
                total_bytes = total_bytes.saturating_add(written);
                if total_bytes > opts.max_total_bytes {
                    return Err(StackError::ArchiveTooLarge {
                        limit: opts.max_total_bytes,
                    });
                }
                entries_written += 1;
            }
            // Symlink/hardlink already rejected in reject_unsafe_entry.
            other => {
                return Err(StackError::ArchiveUnsafeEntry {
                    kind: entry_kind_label(other),
                    name: raw_path.display().to_string(),
                });
            }
        }
    }

    Ok(ExtractReport {
        entries_written,
        bytes_written: total_bytes,
        top_level_dir: if top_levels.len() == 1 {
            top_levels.into_iter().next()
        } else {
            None
        },
    })
}

fn extract_zip(archive: &Path, dest: &Path, opts: &ExtractOpts) -> Result<ExtractReport> {
    let file = File::open(archive).map_err(|source| StackError::ArchiveReadFailed {
        reason: format!("could not open `{}`: {source}", archive.display()),
    })?;
    let mut zip = zip::ZipArchive::new(BufReader::new(file)).map_err(|source| {
        StackError::ArchiveReadFailed {
            reason: format!("zip open: {source}"),
        }
    })?;

    let mut total_bytes: u64 = 0;
    let mut entries_written = 0usize;
    let mut top_levels: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    for index in 0..zip.len() {
        let mut entry = zip
            .by_index(index)
            .map_err(|source| StackError::ArchiveReadFailed {
                reason: format!("zip entry {index}: {source}"),
            })?;
        let raw_name = entry.name().to_owned();
        if entry.is_symlink() {
            return Err(StackError::ArchiveUnsafeEntry {
                kind: "symlink",
                name: raw_name,
            });
        }
        let safe_path = sanitize_relative_path(Path::new(&raw_name))?;
        if let Some(top) = safe_path.components().next().and_then(|c| match c {
            Component::Normal(name) => name.to_str(),
            _ => None,
        }) {
            top_levels.insert(top.to_owned());
        }
        let target = dest.join(&safe_path);
        ensure_inside(&target, dest, Path::new(&raw_name))?;

        if entry.is_dir() {
            std::fs::create_dir_all(&target).map_err(|source| StackError::ArchiveReadFailed {
                reason: format!("create dir `{}`: {source}", target.display()),
            })?;
            continue;
        }

        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|source| StackError::ArchiveReadFailed {
                reason: format!("create dir `{}`: {source}", parent.display()),
            })?;
        }
        let written = copy_capped(
            &mut entry,
            &target,
            opts.max_entry_bytes,
            opts.max_total_bytes - total_bytes,
        )?;
        total_bytes = total_bytes.saturating_add(written);
        if total_bytes > opts.max_total_bytes {
            return Err(StackError::ArchiveTooLarge {
                limit: opts.max_total_bytes,
            });
        }
        entries_written += 1;
    }

    Ok(ExtractReport {
        entries_written,
        bytes_written: total_bytes,
        top_level_dir: if top_levels.len() == 1 {
            top_levels.into_iter().next()
        } else {
            None
        },
    })
}

fn reject_unsafe_entry(entry_type: EntryType, name: &Path) -> Result<()> {
    let label = entry_kind_label(entry_type);
    match entry_type {
        EntryType::Symlink => Err(StackError::ArchiveUnsafeEntry {
            kind: "symlink",
            name: name.display().to_string(),
        }),
        EntryType::Link => Err(StackError::ArchiveUnsafeEntry {
            kind: "hardlink",
            name: name.display().to_string(),
        }),
        EntryType::Char | EntryType::Block | EntryType::Fifo => {
            Err(StackError::ArchiveUnsafeEntry {
                kind: label,
                name: name.display().to_string(),
            })
        }
        EntryType::Regular
        | EntryType::Directory
        | EntryType::Continuous
        | EntryType::GNUSparse
        | EntryType::XHeader
        | EntryType::XGlobalHeader
        | EntryType::GNULongName
        | EntryType::GNULongLink => Ok(()),
        _ => Err(StackError::ArchiveUnsafeEntry {
            kind: label,
            name: name.display().to_string(),
        }),
    }
}

fn is_tar_metadata_entry(entry_type: EntryType) -> bool {
    matches!(
        entry_type,
        EntryType::XHeader
            | EntryType::XGlobalHeader
            | EntryType::GNULongName
            | EntryType::GNULongLink
    )
}

fn entry_kind_label(entry_type: EntryType) -> &'static str {
    match entry_type {
        EntryType::Regular => "regular",
        EntryType::Link => "hardlink",
        EntryType::Symlink => "symlink",
        EntryType::Char => "char-device",
        EntryType::Block => "block-device",
        EntryType::Directory => "directory",
        EntryType::Fifo => "fifo",
        EntryType::Continuous => "continuous",
        EntryType::GNUSparse => "gnu-sparse",
        EntryType::XHeader => "pax-local-header",
        EntryType::XGlobalHeader => "pax-global-header",
        EntryType::GNULongName => "gnu-long-name",
        EntryType::GNULongLink => "gnu-long-link",
        _ => "unknown",
    }
}

fn sanitize_relative_path(raw: &Path) -> Result<PathBuf> {
    let mut sanitized = PathBuf::new();
    for component in raw.components() {
        match component {
            Component::Normal(part) => sanitized.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(StackError::ArchiveUnsafeEntry {
                    kind: "parent-traversal",
                    name: raw.display().to_string(),
                });
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(StackError::ArchiveUnsafeEntry {
                    kind: "absolute-path",
                    name: raw.display().to_string(),
                });
            }
        }
    }
    if sanitized.as_os_str().is_empty() {
        return Err(StackError::ArchiveUnsafeEntry {
            kind: "empty-path",
            name: raw.display().to_string(),
        });
    }
    Ok(sanitized)
}

fn ensure_inside(target: &Path, dest_canonical: &Path, raw: &Path) -> Result<()> {
    // Belt-and-suspenders: sanitize_relative_path already rejects traversal
    // before we build `target`. A second textual check guards against host
    // OS quirks (case folding, NTFS reserved names) once we add Windows
    // support later.
    if !target.starts_with(dest_canonical) {
        return Err(StackError::ArchiveUnsafeEntry {
            kind: "escape",
            name: raw.display().to_string(),
        });
    }
    Ok(())
}

fn copy_capped<R: Read>(
    reader: &mut R,
    target: &Path,
    max_entry: u64,
    remaining_total: u64,
) -> Result<u64> {
    let mut file = File::create(target).map_err(|source| StackError::ArchiveReadFailed {
        reason: format!("create `{}`: {source}", target.display()),
    })?;
    let mut buf = vec![0u8; COPY_CHUNK_BYTES];
    let mut written: u64 = 0;
    loop {
        let read = reader
            .read(&mut buf)
            .map_err(|source| StackError::ArchiveReadFailed {
                reason: format!("read entry `{}`: {source}", target.display()),
            })?;
        if read == 0 {
            break;
        }
        written = written
            .checked_add(read as u64)
            .ok_or(StackError::ArchiveTooLarge { limit: max_entry })?;
        if written > max_entry {
            return Err(StackError::ArchiveTooLarge { limit: max_entry });
        }
        if written > remaining_total {
            return Err(StackError::ArchiveTooLarge {
                limit: remaining_total,
            });
        }
        file.write_all(&buf[..read])
            .map_err(|source| StackError::ArchiveReadFailed {
                reason: format!("write entry `{}`: {source}", target.display()),
            })?;
    }
    file.flush()
        .map_err(|source| StackError::ArchiveReadFailed {
            reason: format!("flush `{}`: {source}", target.display()),
        })?;
    Ok(written)
}

// `Seek` is required for `zip::ZipArchive::new` to function — keep an
// explicit reference so dead-code analysis does not silently drop it from
// the dependency surface if `tar` ever stops requiring it.
#[allow(dead_code)]
fn _seek_marker<T: Seek>(_: T) {}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Cursor;
    use tempfile::tempdir;

    fn write_tar_gz<F>(builder_fn: F) -> Vec<u8>
    where
        F: FnOnce(&mut tar::Builder<&mut Vec<u8>>),
    {
        let mut buf = Vec::new();
        {
            let mut inner = Vec::new();
            {
                let mut tar_builder = tar::Builder::new(&mut inner);
                builder_fn(&mut tar_builder);
                tar_builder.finish().expect("tar finish");
            }
            let mut gz = GzEncoder::new(&mut buf, Compression::default());
            gz.write_all(&inner).expect("gz write");
            gz.finish().expect("gz finish");
        }
        buf
    }

    fn write_zip<F>(builder_fn: F) -> Vec<u8>
    where
        F: FnOnce(&mut zip::ZipWriter<Cursor<&mut Vec<u8>>>),
    {
        let mut buf = Vec::new();
        {
            let cursor = Cursor::new(&mut buf);
            let mut zip = zip::ZipWriter::new(cursor);
            builder_fn(&mut zip);
            zip.finish().expect("zip finish");
        }
        buf
    }

    fn write_archive_to_tmp(bytes: &[u8], name: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).expect("write");
        (dir, path)
    }

    #[test]
    fn extracts_simple_tar_gz_with_single_top_level() {
        let bytes = write_tar_gz(|t| {
            let mut header = tar::Header::new_gnu();
            header.set_size(11);
            header.set_mode(0o644);
            header.set_cksum();
            t.append_data(
                &mut header.clone(),
                "wrapper/hello.txt",
                &b"hello, acp\n"[..],
            )
            .unwrap();
            let mut header = tar::Header::new_gnu();
            header.set_size(6);
            header.set_mode(0o644);
            header.set_cksum();
            t.append_data(&mut header, "wrapper/sub/x.txt", &b"sub-x\n"[..])
                .unwrap();
        });
        let (_dir, archive) = write_archive_to_tmp(&bytes, "ok.tar.gz");
        let dest = tempdir().expect("dest");
        let report = extract_archive(&archive, dest.path(), &ExtractOpts::default()).expect("ok");
        assert_eq!(report.entries_written, 2);
        assert_eq!(report.top_level_dir.as_deref(), Some("wrapper"));
        assert!(dest.path().join("wrapper").join("hello.txt").is_file());
        assert!(
            dest.path()
                .join("wrapper")
                .join("sub")
                .join("x.txt")
                .is_file()
        );
    }

    #[test]
    fn rejects_tar_with_parent_dir_traversal() {
        let bytes = write_tar_gz(|t| {
            // tar::Header::set_path refuses `..`; build the header manually.
            let mut header = tar::Header::new_old();
            let name_bytes = header.as_old_mut().name.as_mut();
            name_bytes.fill(0);
            let traversal = b"../escape.txt";
            name_bytes[..traversal.len()].copy_from_slice(traversal);
            header.set_size(3);
            header.set_mode(0o644);
            header.set_entry_type(tar::EntryType::Regular);
            header.set_cksum();
            t.append(&header, &b"esc"[..]).unwrap();
        });
        let (_dir, archive) = write_archive_to_tmp(&bytes, "bad.tar.gz");
        let dest = tempdir().expect("dest");
        let err =
            extract_archive(&archive, dest.path(), &ExtractOpts::default()).expect_err("rejected");
        assert!(
            matches!(
                err,
                StackError::ArchiveUnsafeEntry { kind, .. } if kind == "parent-traversal"
            ),
            "got: {err}"
        );
    }

    #[test]
    fn rejects_tar_with_absolute_path() {
        let bytes = write_tar_gz(|t| {
            let mut header = tar::Header::new_gnu();
            header.set_size(3);
            header.set_mode(0o644);
            header.set_cksum();
            // tar's append_data strips the leading slash by design, so build
            // a synthetic header manually.
            let mut h = tar::Header::new_gnu();
            h.set_path("etc/passwd").unwrap();
            h.set_size(3);
            h.set_mode(0o644);
            // Manually set the name field bytes to include a leading slash.
            let name_bytes = h.as_old_mut().name.as_mut();
            name_bytes.fill(0);
            let abs = b"/etc/passwd";
            name_bytes[..abs.len()].copy_from_slice(abs);
            h.set_cksum();
            t.append(&h, &b"esc"[..]).unwrap();
        });
        let (_dir, archive) = write_archive_to_tmp(&bytes, "abs.tar.gz");
        let dest = tempdir().expect("dest");
        let err =
            extract_archive(&archive, dest.path(), &ExtractOpts::default()).expect_err("rejected");
        assert!(matches!(
            err,
            StackError::ArchiveUnsafeEntry { kind, .. } if kind == "absolute-path"
        ));
    }

    #[test]
    fn rejects_tar_with_symlink() {
        let bytes = write_tar_gz(|t| {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_path("link").unwrap();
            header.set_link_name("../target").unwrap();
            header.set_size(0);
            header.set_mode(0o644);
            header.set_cksum();
            t.append(&header, &[][..]).unwrap();
        });
        let (_dir, archive) = write_archive_to_tmp(&bytes, "sym.tar.gz");
        let dest = tempdir().expect("dest");
        let err =
            extract_archive(&archive, dest.path(), &ExtractOpts::default()).expect_err("rejected");
        assert!(matches!(
            err,
            StackError::ArchiveUnsafeEntry { kind, .. } if kind == "symlink"
        ));
    }

    #[test]
    fn ignores_tar_metadata_entries() {
        let bytes = write_tar_gz(|t| {
            let pax_data = b"13 comment=x\n";
            let mut pax_header = tar::Header::new_ustar();
            pax_header.set_entry_type(tar::EntryType::XGlobalHeader);
            pax_header.set_path("pax_global_header").unwrap();
            pax_header.set_size(pax_data.len() as u64);
            pax_header.set_mode(0o644);
            pax_header.set_cksum();
            t.append(&pax_header, &pax_data[..]).unwrap();

            let mut header = tar::Header::new_gnu();
            header.set_size(11);
            header.set_mode(0o644);
            header.set_cksum();
            t.append_data(&mut header, "wrapper/hello.txt", &b"hello, acp\n"[..])
                .unwrap();
        });
        let (_dir, archive) = write_archive_to_tmp(&bytes, "pax.tar.gz");
        let dest = tempdir().expect("dest");
        let report = extract_archive(&archive, dest.path(), &ExtractOpts::default()).expect("ok");
        assert_eq!(report.entries_written, 1);
        assert_eq!(report.top_level_dir.as_deref(), Some("wrapper"));
        assert!(dest.path().join("wrapper").join("hello.txt").is_file());
        assert!(!dest.path().join("pax_global_header").exists());
    }

    #[test]
    fn enforces_total_size_cap() {
        let bytes = write_tar_gz(|t| {
            let body = vec![0u8; 4 * 1024];
            for index in 0..4 {
                let mut header = tar::Header::new_gnu();
                header.set_size(body.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                t.append_data(&mut header, format!("file-{index}.bin"), &body[..])
                    .unwrap();
            }
        });
        let (_dir, archive) = write_archive_to_tmp(&bytes, "big.tar.gz");
        let dest = tempdir().expect("dest");
        let opts = ExtractOpts {
            max_total_bytes: 8 * 1024,
            ..ExtractOpts::default()
        };
        let err = extract_archive(&archive, dest.path(), &opts).expect_err("too large");
        assert!(matches!(err, StackError::ArchiveTooLarge { .. }));
    }

    #[test]
    fn extracts_zip_with_multiple_top_levels() {
        let bytes = write_zip(|z| {
            let options: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            z.start_file::<&str, ()>("a/x.txt", options).unwrap();
            z.write_all(b"x").unwrap();
            z.start_file::<&str, ()>("b/y.txt", options).unwrap();
            z.write_all(b"y").unwrap();
        });
        let (_dir, archive) = write_archive_to_tmp(&bytes, "ok.zip");
        let dest = tempdir().expect("dest");
        let report = extract_archive(&archive, dest.path(), &ExtractOpts::default()).expect("ok");
        assert_eq!(report.entries_written, 2);
        assert_eq!(report.top_level_dir, None);
        assert!(dest.path().join("a/x.txt").is_file());
        assert!(dest.path().join("b/y.txt").is_file());
    }

    #[test]
    fn rejects_zip_with_parent_dir() {
        let bytes = write_zip(|z| {
            let options: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            z.start_file::<&str, ()>("../escape.txt", options).unwrap();
            z.write_all(b"x").unwrap();
        });
        let (_dir, archive) = write_archive_to_tmp(&bytes, "bad.zip");
        let dest = tempdir().expect("dest");
        let err =
            extract_archive(&archive, dest.path(), &ExtractOpts::default()).expect_err("rejected");
        assert!(matches!(
            err,
            StackError::ArchiveUnsafeEntry { kind, .. } if kind == "parent-traversal"
        ));
    }

    #[test]
    fn rejects_unsupported_format() {
        let (_dir, archive) =
            write_archive_to_tmp(b"This is plain text, not an archive.\n", "plain.txt");
        let dest = tempdir().expect("dest");
        let err =
            extract_archive(&archive, dest.path(), &ExtractOpts::default()).expect_err("rejected");
        assert!(matches!(err, StackError::ArchiveUnsupportedFormat));
    }
}
