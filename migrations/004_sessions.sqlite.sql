-- Sessions lifecycle batch: extend `sessions` with the columns the ACP bridge
-- needs to persist a session, add a `session_id` column + index on `events` so
-- ACP `session/update` notifications can be queried per session, and introduce
-- a `prompts` table for fire-and-forget prompt submissions.

ALTER TABLE sessions ADD COLUMN agent_id TEXT NOT NULL DEFAULT '';
ALTER TABLE sessions ADD COLUMN cwd TEXT NOT NULL DEFAULT '';
ALTER TABLE sessions ADD COLUMN title TEXT;
ALTER TABLE sessions ADD COLUMN metadata_json TEXT NOT NULL DEFAULT '{}'
    CHECK (json_valid(metadata_json));

CREATE INDEX IF NOT EXISTS sessions_updated_at_idx
    ON sessions (updated_at DESC, id DESC);

ALTER TABLE events ADD COLUMN session_id TEXT;

CREATE INDEX IF NOT EXISTS events_session_id_idx
    ON events (session_id, created_at, id);

CREATE TABLE IF NOT EXISTS prompts (
    id            TEXT PRIMARY KEY,
    session_id    TEXT NOT NULL REFERENCES sessions(id),
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL,
    status        TEXT NOT NULL
        CHECK (status IN ('pending', 'running', 'completed', 'errored', 'cancelled')),
    stop_reason   TEXT,
    error_code    TEXT,
    error_message TEXT,
    prompt_json   TEXT NOT NULL CHECK (json_valid(prompt_json))
);

CREATE INDEX IF NOT EXISTS prompts_session_idx
    ON prompts (session_id, created_at, id);
