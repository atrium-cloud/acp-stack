use std::path::{Path, PathBuf};

use crate::error::{Result, StackError};

use super::normalize::NormalizedShellWord;

pub(super) fn command_word_index(words: &[NormalizedShellWord]) -> Option<usize> {
    let mut index = 0;
    while index < words.len() {
        if is_shell_assignment_word(&words[index]) {
            index += 1;
            continue;
        }
        if let Some(width) = redirection_prefix_width(words, index) {
            index += width;
            continue;
        }
        if let Some(width) = shell_pipeline_prefix_width(words, index) {
            index += width;
            continue;
        }
        return Some(index);
    }
    None
}

fn is_shell_assignment_word(word: &NormalizedShellWord) -> bool {
    if word.assignment_name_constructed {
        return false;
    }
    let Some(operator_index) = word.assignment_operator_index else {
        return false;
    };
    let name = &word.text[..operator_index];
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn redirection_prefix_width(words: &[NormalizedShellWord], index: usize) -> Option<usize> {
    let word = words.get(index)?;
    let operator_end = word.redirection_operator_end?;
    if word.text.len() == operator_end && words.get(index + 1).is_some() {
        Some(2)
    } else {
        Some(1)
    }
}

fn shell_pipeline_prefix_width(words: &[NormalizedShellWord], index: usize) -> Option<usize> {
    let word = words.get(index)?;
    if word.text == "!" {
        return Some(1);
    }
    if word.text != "time" {
        return None;
    }
    let mut width = 1;
    while words
        .get(index + width)
        .is_some_and(|candidate| candidate.text == "-p")
    {
        width += 1;
    }
    Some(width)
}

pub(super) fn redirection_operator_end(word: &str, operator_prefix: bool) -> Option<usize> {
    if !operator_prefix {
        return None;
    }
    let start = word
        .char_indices()
        .find_map(|(index, ch)| matches!(ch, '<' | '>').then_some(index))?;
    let operator = &word[start..];
    let operator_len = if operator.starts_with("<<-") || operator.starts_with("<<<") {
        3
    } else if operator.starts_with(">>")
        || operator.starts_with("<>")
        || operator.starts_with("<&")
        || operator.starts_with(">&")
        || operator.starts_with(">|")
        || operator.starts_with("<<")
    {
        2
    } else {
        1
    };
    Some(start + operator_len)
}

/// Minimal shell-style glob matcher supporting `*` and `?` only — NOT a full
/// POSIX-glob implementation.
pub(super) fn glob_match(pattern: &str, input: &str) -> bool {
    let pattern_bytes = pattern.as_bytes();
    let input_bytes = input.as_bytes();
    glob_match_inner(pattern_bytes, input_bytes)
}

fn glob_match_inner(pattern: &[u8], input: &[u8]) -> bool {
    let mut p = 0;
    let mut i = 0;
    let mut star_p: Option<usize> = None;
    let mut star_i = 0;
    while i < input.len() {
        if p < pattern.len() && (pattern[p] == input[i] || pattern[p] == b'?') {
            p += 1;
            i += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star_p = Some(p);
            star_i = i;
            p += 1;
        } else if let Some(sp) = star_p {
            p = sp + 1;
            star_i += 1;
            i = star_i;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedCommandCwd {
    path: PathBuf,
    #[cfg(unix)]
    identity: FileIdentity,
}

impl ResolvedCommandCwd {
    #[cfg(not(unix))]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn display_path(&self) -> String {
        self.path.to_string_lossy().into_owned()
    }

    #[cfg(unix)]
    pub(crate) fn open_verified(&self) -> std::io::Result<std::fs::File> {
        use std::os::unix::fs::OpenOptionsExt;

        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(&self.path)?;
        let identity = FileIdentity::from_metadata(&file.metadata()?);
        if identity != self.identity {
            return Err(std::io::Error::other(
                "command cwd changed after validation",
            ));
        }
        Ok(file)
    }
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    dev: u64,
    ino: u64,
}

#[cfg(unix)]
impl FileIdentity {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
        }
    }
}

pub(crate) fn resolve_cwd_under_workspace(
    root: &Path,
    requested: &str,
) -> Result<ResolvedCommandCwd> {
    if requested.contains('\0') {
        return Err(StackError::CommandCwdOutsideWorkspace {
            requested: requested.to_owned(),
        });
    }
    let candidate = if Path::new(requested).is_absolute() {
        std::path::PathBuf::from(requested)
    } else {
        root.join(requested)
    };
    let canonical_root =
        root.canonicalize()
            .map_err(|_| StackError::CommandCwdOutsideWorkspace {
                requested: requested.to_owned(),
            })?;
    let canonical_candidate =
        candidate
            .canonicalize()
            .map_err(|_| StackError::CommandCwdOutsideWorkspace {
                requested: requested.to_owned(),
            })?;
    let metadata =
        canonical_candidate
            .metadata()
            .map_err(|_| StackError::CommandCwdOutsideWorkspace {
                requested: requested.to_owned(),
            })?;
    if !metadata.is_dir() {
        return Err(StackError::CommandCwdOutsideWorkspace {
            requested: requested.to_owned(),
        });
    }
    if !canonical_candidate.starts_with(&canonical_root) {
        return Err(StackError::CommandCwdOutsideWorkspace {
            requested: requested.to_owned(),
        });
    }
    Ok(ResolvedCommandCwd {
        path: canonical_candidate,
        #[cfg(unix)]
        identity: FileIdentity::from_metadata(&metadata),
    })
}
