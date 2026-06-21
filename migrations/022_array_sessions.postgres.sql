ALTER TABLE sessions ADD COLUMN target_id TEXT NOT NULL DEFAULT '';
ALTER TABLE sessions ADD COLUMN agent_session_id TEXT NOT NULL DEFAULT '';

UPDATE sessions
SET target_id = agent_id
WHERE target_id = '';

UPDATE sessions
SET agent_session_id = id
WHERE agent_session_id = '';

CREATE INDEX IF NOT EXISTS sessions_target_updated_at_idx
    ON sessions (target_id, updated_at DESC, id DESC);

CREATE UNIQUE INDEX IF NOT EXISTS sessions_target_agent_session_idx
    ON sessions (target_id, agent_session_id);
