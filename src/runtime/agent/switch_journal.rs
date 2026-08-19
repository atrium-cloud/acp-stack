//! Pending-switch journal for convergent agent-primary switch retries.
//!
//! `POST /v1/agent/switch` is not atomic: the session-target DB rename and
//! the canonical config write commit the switch, but the runtime re-apply
//! (stop old agent, start new) happens after. A failure or process exit in
//! that window used to leave the new primary on disk with its agent stopped,
//! and a naive retry was rejected with "already configured". This journal is
//! the durable record that lets a same-target retry converge instead:
//!
//! - The file lives beside the canonical config and `.agent-config.lock`, so
//!   it is covered by the same agent-config mutation lock the switch handler
//!   already holds for the whole read/plan/write sequence.
//! - `phase` advances Planned -> Committed -> RuntimeApplied -> Completed.
//!   The on-disk config is the real commit marker (the DB rename strictly
//!   precedes the config write); the journal records *intent* and the
//!   pre-commit `was_running` snapshot, which is unrecoverable after a
//!   process restart.
//! - A `Completed` journal is retained, not deleted: it is what makes a
//!   same-target retry provably side-effect-free. The next different-target
//!   switch overwrites it at Planned.
//! - A journal whose switch failed before any durable mutation (e.g. the
//!   session-rename collision check) is removed instead: nothing was
//!   committed, so there is nothing to resume, and a stranded Planned entry
//!   would 409 every later switch.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::error::{Result, StackError};

pub const SWITCH_JOURNAL_FILE_NAME: &str = "agent-switch.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwitchJournal {
    /// Primary target id before the switch. For the primary-switch path this
    /// equals the old agent id; for the existing-array-target path it is the
    /// previously selected target.
    pub old_target_id: String,
    /// Target id the operator asked for (the `agent` request field).
    pub new_target_id: String,
    /// Registry agent id the switch installs. Post-commit the on-disk primary
    /// target id is rewritten to this, so retries may reference either id.
    pub target_agent_id: String,
    /// SHA-256 hex of the canonical candidate TOML. A same-target retry must
    /// reproduce the same candidate; a mismatch means the operator edited
    /// config mid-flight and the in-flight switch must not be resumed blindly.
    pub candidate_fingerprint: String,
    /// Whether the old target's agent was running when the switch committed.
    /// Journaled because a retry after a process restart can no longer
    /// observe whether the pre-switch runtime was up.
    pub was_running: bool,
    pub phase: SwitchJournalPhase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwitchJournalPhase {
    Planned,
    Committed,
    RuntimeApplied,
    Completed,
}

impl SwitchJournalPhase {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Committed => "committed",
            Self::RuntimeApplied => "runtime_applied",
            Self::Completed => "completed",
        }
    }
}

impl SwitchJournal {
    /// The request `agent` field addresses either the requested target id or,
    /// after the commit rewrote the primary target id to the agent id, the
    /// target agent id.
    pub fn requested_target_matches(&self, requested: &str) -> bool {
        requested == self.new_target_id || requested == self.target_agent_id
    }

    /// True while the switch did not finish: the runtime is in a partially
    /// applied state that a same-target retry must drive to completion.
    pub fn is_incomplete(&self) -> bool {
        self.phase != SwitchJournalPhase::Completed
    }
}

pub fn switch_journal_path(config_path: &Path) -> Result<PathBuf> {
    Ok(crate::fs_util::parent_dir(config_path)?.join(SWITCH_JOURNAL_FILE_NAME))
}

/// Load the pending-switch journal, if any. A missing file is `None`; an
/// unreadable or unparseable file is a hard error — corrupt local state must
/// not be silently ignored, because the journal is the only record of whether
/// a switch is half-applied.
pub fn load_switch_journal(config_path: &Path) -> Result<Option<SwitchJournal>> {
    let path = switch_journal_path(config_path)?;
    let content = match std::fs::read(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(StackError::ConfigRead { path, source }),
    };
    let journal = serde_json::from_slice(&content).map_err(|error| {
        StackError::AgentSwitchJournalCorrupt {
            path: path.clone(),
            reason: error.to_string(),
        }
    })?;
    Ok(Some(journal))
}

/// Persist a phase transition atomically so a crash between phases never
/// leaves a half-written journal that would read as corrupt.
pub fn persist_switch_journal(config_path: &Path, journal: &SwitchJournal) -> Result<()> {
    let path = switch_journal_path(config_path)?;
    let content =
        serde_json::to_vec(journal).map_err(|error| StackError::AgentSwitchJournalCorrupt {
            path: path.clone(),
            reason: format!("failed to serialize switch journal: {error}"),
        })?;
    crate::fs_util::atomic_write_owner_only(&path, &content)
}

/// Remove the journal, tolerating a missing file. Only for a switch that
/// failed before any durable mutation (nothing to resume): a journal past the
/// commit boundary must be retained for the convergent retry.
pub fn remove_switch_journal(config_path: &Path) -> Result<()> {
    let path = switch_journal_path(config_path)?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(StackError::FileRemove { path, source }),
    }
}

/// SHA-256 hex of the canonical candidate TOML. `sha2` matches the installer
/// and supervisor hashing already used across the runtime.
pub fn candidate_fingerprint(canonical_toml: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(canonical_toml.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_journal(phase: SwitchJournalPhase) -> SwitchJournal {
        SwitchJournal {
            old_target_id: "opencode".to_owned(),
            new_target_id: "kimi".to_owned(),
            target_agent_id: "kimi".to_owned(),
            candidate_fingerprint: candidate_fingerprint("canonical"),
            was_running: true,
            phase,
        }
    }

    #[test]
    fn persist_then_load_round_trips_every_phase() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let config_path = tempdir.path().join("acps-config.toml");
        for phase in [
            SwitchJournalPhase::Planned,
            SwitchJournalPhase::Committed,
            SwitchJournalPhase::RuntimeApplied,
            SwitchJournalPhase::Completed,
        ] {
            let journal = sample_journal(phase);
            persist_switch_journal(&config_path, &journal).expect("persist");
            let loaded = load_switch_journal(&config_path)
                .expect("load")
                .expect("journal present");
            assert_eq!(loaded, journal);
        }
    }

    #[test]
    fn missing_journal_reads_as_none() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let config_path = tempdir.path().join("acps-config.toml");
        assert_eq!(load_switch_journal(&config_path).expect("load"), None);
    }

    #[test]
    fn remove_journal_clears_entry_and_tolerates_absence() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let config_path = tempdir.path().join("acps-config.toml");
        persist_switch_journal(&config_path, &sample_journal(SwitchJournalPhase::Planned))
            .expect("persist");
        remove_switch_journal(&config_path).expect("remove");
        assert_eq!(load_switch_journal(&config_path).expect("load"), None);
        // Removing again (no journal on disk) is not an error.
        remove_switch_journal(&config_path).expect("remove absent");
    }

    #[test]
    fn corrupt_journal_is_a_hard_error() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let config_path = tempdir.path().join("acps-config.toml");
        std::fs::write(tempdir.path().join(SWITCH_JOURNAL_FILE_NAME), b"{not json")
            .expect("write corrupt journal");

        let error = load_switch_journal(&config_path).expect_err("corrupt journal must fail");
        assert!(
            matches!(error, StackError::AgentSwitchJournalCorrupt { .. }),
            "unexpected error: {error:?}"
        );
    }

    #[test]
    fn requested_target_matches_requested_id_or_agent_id() {
        let journal = SwitchJournal {
            new_target_id: "work-kimi".to_owned(),
            ..sample_journal(SwitchJournalPhase::Planned)
        };
        assert!(journal.requested_target_matches("work-kimi"));
        assert!(journal.requested_target_matches("kimi"));
        assert!(!journal.requested_target_matches("amp"));
    }

    #[test]
    fn fingerprint_is_stable_hex_sha256() {
        let fingerprint = candidate_fingerprint("canonical");
        assert_eq!(fingerprint.len(), 64);
        assert!(fingerprint.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(fingerprint, candidate_fingerprint("canonical"));
        assert_ne!(fingerprint, candidate_fingerprint("other"));
    }
}
