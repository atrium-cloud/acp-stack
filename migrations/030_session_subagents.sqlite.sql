-- Subagent sessions: a child session the agent announced on a parent through
-- ACP `subagent_update`. The child keeps its own `sessions` row; this row
-- links it to its parent and to the root of its tree and holds the parent's
-- latest patch for it.

CREATE TABLE IF NOT EXISTS session_subagents (
    session_id        TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
    parent_session_id TEXT NOT NULL REFERENCES sessions(id),
    root_session_id   TEXT NOT NULL REFERENCES sessions(id),
    title             TEXT,
    description       TEXT,
    state_json        TEXT,
    state_current     INTEGER NOT NULL DEFAULT 0 CHECK (state_current IN (0, 1)),
    cancel_granted    INTEGER NOT NULL DEFAULT 0 CHECK (cancel_granted IN (0, 1)),
    announced_at      TEXT NOT NULL,
    updated_at        TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS session_subagents_parent_idx
    ON session_subagents (parent_session_id);

CREATE INDEX IF NOT EXISTS session_subagents_root_idx
    ON session_subagents (root_session_id);
