//! Subagent sessions: child sessions an agent announces on a parent through
//! ACP `subagent_update`. A child is an ordinary session row of the parent's
//! target, linked to its parent and to the root of its tree by a
//! `session_subagents` row that holds the parent's latest patch for it.

use super::events::{insert_session_event_row, session_update_event};
use super::queries::insert_session_row;
use super::*;

/// `message` of the `session.subagent_announced` event.
const SUBAGENT_ANNOUNCED_EVENT_MESSAGE: &str = "ACP subagent session announced";

const SUBAGENT_COLUMNS: &str = "session_id, parent_session_id, root_session_id, title, \
     description, state_json, state_current, cancel_granted, announced_at, updated_at";

/// One field of a [`SubagentPatch`]: `Keep` leaves the stored value, `Clear`
/// nulls it, and `Set` replaces it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum FieldPatch<T> {
    #[default]
    Keep,
    Clear,
    Set(T),
}

impl<T> FieldPatch<T> {
    fn apply(self, current: Option<T>) -> Option<T> {
        match self {
            FieldPatch::Keep => current,
            FieldPatch::Clear => None,
            FieldPatch::Set(value) => Some(value),
        }
    }
}

/// The parent's patch for one child.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentPatch {
    /// The agent's id for the child session.
    pub agent_session_id: String,
    pub title: FieldPatch<String>,
    pub description: FieldPatch<String>,
    /// The agent's state object for the child, as the ACP SDK decodes it.
    pub state_json: FieldPatch<String>,
    /// Whether the client may cancel the child's work; `None` keeps the stored grant.
    pub cancel_granted: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentRecord {
    pub session_id: String,
    pub parent_session_id: String,
    pub root_session_id: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub state_json: Option<String>,
    /// The state came from the agent connection that is still live.
    pub state_current: bool,
    pub cancel_granted: bool,
    pub announced_at: String,
    pub updated_at: String,
}

/// What [`StateStore::record_subagent_update`] did with a patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubagentApplied {
    /// The child is new to its parent: its row was written, or an unprompted
    /// row the agent already listed was adopted.
    Announced {
        child_session_id: String,
        root_session_id: String,
    },
    Updated {
        child_session_id: String,
    },
    Rejected(SubagentRejection),
}

/// Why a patch was left unapplied. The verbatim update is recorded regardless.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentRejection {
    /// The patch names the parent itself.
    SelfParent,
    /// The patch names the root of the parent's tree.
    Ancestor,
    /// The child already belongs to another parent.
    Reparent,
    /// The patch names a session the client prompted.
    Prompted,
    /// The patch names a session that already has subagents of its own.
    HeadsTree,
}

impl SubagentRejection {
    pub fn as_str(self) -> &'static str {
        match self {
            SubagentRejection::SelfParent => "self_parent",
            SubagentRejection::Ancestor => "ancestor",
            SubagentRejection::Reparent => "reparent",
            SubagentRejection::Prompted => "prompted",
            SubagentRejection::HeadsTree => "heads_tree",
        }
    }
}

fn row_to_subagent(row: &rusqlite::Row<'_>) -> rusqlite::Result<SubagentRecord> {
    Ok(SubagentRecord {
        session_id: row.get(0)?,
        parent_session_id: row.get(1)?,
        root_session_id: row.get(2)?,
        title: row.get(3)?,
        description: row.get(4)?,
        state_json: row.get(5)?,
        state_current: row.get::<_, i64>(6)? != 0,
        cancel_granted: row.get::<_, i64>(7)? != 0,
        announced_at: row.get(8)?,
        updated_at: row.get(9)?,
    })
}

fn subagent_on(conn: &rusqlite::Connection, session_id: &str) -> Result<Option<SubagentRecord>> {
    Ok(conn
        .query_row(
            &format!("SELECT {SUBAGENT_COLUMNS} FROM session_subagents WHERE session_id = ?1"),
            params![session_id],
            row_to_subagent,
        )
        .optional()?)
}

fn session_on(conn: &rusqlite::Connection, session_id: &str) -> Result<SessionRecord> {
    conn.query_row(
        &format!("SELECT {SESSION_COLUMNS} FROM sessions WHERE id = ?1"),
        params![session_id],
        row_to_session,
    )
    .optional()?
    .ok_or_else(|| StackError::SessionNotFound {
        id: session_id.to_owned(),
    })
}

/// Apply `patch` from `parent_session_id` inside the caller's transaction.
fn apply_subagent_patch(
    conn: &rusqlite::Connection,
    parent_session_id: &str,
    patch: SubagentPatch,
) -> Result<SubagentApplied> {
    let parent = session_on(conn, parent_session_id)?;
    let root_session_id = subagent_on(conn, &parent.id)?.map_or_else(
        || parent.id.clone(),
        |association| association.root_session_id,
    );
    let existing = conn
        .query_row(
            &format!(
                "SELECT {SESSION_COLUMNS} FROM sessions \
                 WHERE target_id = ?1 AND agent_session_id = ?2"
            ),
            params![parent.target_id, patch.agent_session_id],
            row_to_session,
        )
        .optional()?;
    let now = current_timestamp();
    let (child_session_id, stored) = match existing {
        None => {
            let child = SessionRecord {
                id: next_session_id(),
                target_id: parent.target_id.clone(),
                agent_session_id: patch.agent_session_id.clone(),
                created_at: now.clone(),
                updated_at: now.clone(),
                status: SESSION_STATUS_AVAILABLE.to_owned(),
                agent_id: parent.agent_id.clone(),
                cwd: parent.cwd.clone(),
                title: None,
                metadata_json: "{}".to_owned(),
                change_seq: 0,
                event_seq: 0,
            };
            insert_session_row(conn, &child, next_change_seq(conn)?)?;
            (child.id, None)
        }
        Some(existing) => {
            if existing.id == parent.id {
                return Ok(SubagentApplied::Rejected(SubagentRejection::SelfParent));
            }
            if existing.id == root_session_id {
                return Ok(SubagentApplied::Rejected(SubagentRejection::Ancestor));
            }
            match subagent_on(conn, &existing.id)? {
                Some(stored) if stored.parent_session_id == parent.id => {
                    (existing.id, Some(stored))
                }
                Some(_) => return Ok(SubagentApplied::Rejected(SubagentRejection::Reparent)),
                None => {
                    let (prompted, heads_tree): (bool, bool) = conn.query_row(
                        r#"
                        SELECT EXISTS (SELECT 1 FROM prompts WHERE session_id = ?1),
                               EXISTS (SELECT 1 FROM session_subagents WHERE parent_session_id = ?1)
                        "#,
                        params![existing.id],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )?;
                    if prompted {
                        return Ok(SubagentApplied::Rejected(SubagentRejection::Prompted));
                    }
                    if heads_tree {
                        return Ok(SubagentApplied::Rejected(SubagentRejection::HeadsTree));
                    }
                    bump_session_change(conn, &existing.id)?;
                    (existing.id, None)
                }
            }
        }
    };

    let state_current = match &patch.state_json {
        FieldPatch::Keep => stored.as_ref().is_some_and(|stored| stored.state_current),
        FieldPatch::Clear => false,
        FieldPatch::Set(_) => true,
    };
    let cancel_granted = patch
        .cancel_granted
        .unwrap_or_else(|| stored.as_ref().is_some_and(|stored| stored.cancel_granted));
    let (title, description, state_json) = match &stored {
        Some(stored) => (
            stored.title.clone(),
            stored.description.clone(),
            stored.state_json.clone(),
        ),
        None => (None, None, None),
    };
    let title = patch.title.apply(title);
    let description = patch.description.apply(description);
    let state_json = patch.state_json.apply(state_json);
    if stored.is_some() {
        conn.execute(
            r#"
            UPDATE session_subagents
            SET title = ?1, description = ?2, state_json = ?3, state_current = ?4,
                cancel_granted = ?5, updated_at = ?6
            WHERE session_id = ?7
            "#,
            params![
                title,
                description,
                state_json,
                state_current,
                cancel_granted,
                now,
                child_session_id
            ],
        )?;
        bump_session_change(conn, &child_session_id)?;
        return Ok(SubagentApplied::Updated { child_session_id });
    }
    conn.execute(
        &format!(
            "INSERT INTO session_subagents ({SUBAGENT_COLUMNS}) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)"
        ),
        params![
            child_session_id,
            parent.id,
            root_session_id,
            title,
            description,
            state_json,
            state_current,
            cancel_granted,
            now
        ],
    )?;
    Ok(SubagentApplied::Announced {
        child_session_id,
        root_session_id,
    })
}

impl StateStore {
    /// Record one ACP `subagent_update` that arrived on `parent_session_id`: its verbatim
    /// `session.update` row, the patch applied to the child it names, and for a new child
    /// its session row and a `session.subagent_announced` event on the parent, all in one
    /// transaction.
    pub fn record_subagent_update(
        &self,
        parent_session_id: &str,
        payload_json: &str,
        patch: SubagentPatch,
    ) -> Result<SubagentApplied> {
        let mut update_event =
            session_update_event(parent_session_id, EVENT_KIND_SESSION_UPDATE, payload_json)?;
        let agent_session_id = patch.agent_session_id.clone();
        let tx = self.begin_immediate("session_subagents")?;
        update_event.seq =
            Some(self.insert_session_update_row(&tx, parent_session_id, &update_event)?);
        let applied = apply_subagent_patch(&tx, parent_session_id, patch)?;
        let announced_event = match &applied {
            SubagentApplied::Announced {
                child_session_id,
                root_session_id,
            } => {
                let mut event = Event {
                    id: next_event_id(),
                    created_at: current_timestamp(),
                    level: "info".to_owned(),
                    kind: EVENT_KIND_SESSION_SUBAGENT_ANNOUNCED.to_owned(),
                    message: SUBAGENT_ANNOUNCED_EVENT_MESSAGE.to_owned(),
                    payload_json: serde_json::json!({
                        "child_session_id": child_session_id,
                        "agent_session_id": agent_session_id,
                        "root_session_id": root_session_id,
                    })
                    .to_string(),
                    source: EVENT_SOURCE_SYSTEM.to_owned(),
                    session_id: Some(parent_session_id.to_owned()),
                    seq: None,
                };
                event.seq = Some(insert_session_event_row(&tx, parent_session_id, &event)?);
                if self.external_logging_enabled() {
                    sink_outbox::enqueue(&tx, "sessions", child_session_id, &event.created_at)?;
                    sink_outbox::enqueue(&tx, "events", &event.id, &event.created_at)?;
                }
                Some(event)
            }
            _ => None,
        };
        tx.commit()?;
        self.publish_session_event(parent_session_id, &update_event);
        if let Some(event) = &announced_event {
            self.publish_session_event(parent_session_id, event);
        }
        Ok(applied)
    }

    pub fn get_subagent(&self, session_id: &str) -> Result<Option<SubagentRecord>> {
        subagent_on(self.connection(), session_id)
    }
}

#[cfg(test)]
mod tests;
