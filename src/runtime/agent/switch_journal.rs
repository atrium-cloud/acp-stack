//! Pending-switch journal (`Planned -> Committed -> RuntimeApplied -> Completed`) that lets a same-target `POST /v1/agent/switch` retry converge after a failure in the non-atomic commit window.
//! It lives beside the canonical config, so the switch handler's agent-config mutation lock already covers it.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::error::{Result, StackError};

pub const SWITCH_JOURNAL_FILE_NAME: &str = "agent-switch.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwitchJournal {
    /// Primary target id before the switch.
    pub old_target_id: String,
    /// Target id the operator asked for (the `agent` request field).
    pub new_target_id: String,
    /// Registry agent id the switch installs; post-commit the on-disk primary target id is rewritten to this, so retries may reference either id.
    pub target_agent_id: String,
    /// SHA-256 hex of the canonical candidate TOML. A mismatch means the operator edited config mid-flight, so the in-flight switch must not be resumed blindly.
    pub candidate_fingerprint: String,
    /// Whether the old target's agent was running when the switch committed, which a retry after a process restart can no longer observe.
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
    /// Whether the request `agent` field addresses this journal's requested target id or its target agent id.
    pub fn requested_target_matches(&self, requested: &str) -> bool {
        requested == self.new_target_id || requested == self.target_agent_id
    }

    /// True while the runtime is in a partially applied state a same-target retry must drive to completion.
    pub fn is_incomplete(&self) -> bool {
        self.phase != SwitchJournalPhase::Completed
    }
}

pub fn switch_journal_path(config_path: &Path) -> Result<PathBuf> {
    Ok(crate::fs_util::parent_dir(config_path)?.join(SWITCH_JOURNAL_FILE_NAME))
}

/// Load the pending-switch journal. A missing file is `None`; an unparseable one is a hard error, because the journal is the only record of whether a switch is half-applied.
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

/// Persist a phase transition atomically, so a crash between phases never leaves a half-written journal.
pub fn persist_switch_journal(config_path: &Path, journal: &SwitchJournal) -> Result<()> {
    let path = switch_journal_path(config_path)?;
    let content =
        serde_json::to_vec(journal).map_err(|error| StackError::AgentSwitchJournalCorrupt {
            path: path.clone(),
            reason: format!("failed to serialize switch journal: {error}"),
        })?;
    crate::fs_util::atomic_write_owner_only(&path, &content)
}

/// Remove the journal, tolerating a missing file. Only for a switch that failed before any durable mutation: a journal past the commit boundary MUST be retained for the convergent retry.
pub fn remove_switch_journal(config_path: &Path) -> Result<()> {
    let path = switch_journal_path(config_path)?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(StackError::FileRemove { path, source }),
    }
}

/// SHA-256 hex of the canonical candidate TOML.
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
