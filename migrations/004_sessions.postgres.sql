-- Sessions lifecycle batch: extend `sessions` with the columns the ACP bridge
-- needs to persist a session, add a `session_id` column + index on `events` so
-- ACP `session/update` notifications can be queried per session, and introduce
-- a `prompts` table for fire-and-forget prompt submissions.

ALTER TABLE sessions ADD COLUMN agent_id text NOT NULL DEFAULT '';
ALTER TABLE sessions ADD COLUMN cwd text NOT NULL DEFAULT '';
ALTER TABLE sessions ADD COLUMN title text;
ALTER TABLE sessions ADD COLUMN metadata_json jsonb NOT NULL DEFAULT '{}'::jsonb;

CREATE INDEX IF NOT EXISTS sessions_updated_at_idx
    ON sessions (updated_at DESC, id DESC);

ALTER TABLE events ADD COLUMN session_id text;

CREATE INDEX IF NOT EXISTS events_session_id_idx
    ON events (session_id, created_at, id);

CREATE TABLE IF NOT EXISTS prompts (
    id            text PRIMARY KEY,
    session_id    text NOT NULL REFERENCES sessions(id),
    created_at    timestamptz NOT NULL,
    updated_at    timestamptz NOT NULL,
    status        text NOT NULL
        CHECK (status IN ('pending', 'running', 'completed', 'errored', 'cancelled')),
    stop_reason   text,
    error_code    text,
    error_message text,
    prompt_json   jsonb NOT NULL
);

ALTER TABLE prompts ENABLE ROW LEVEL SECURITY;
REVOKE ALL ON TABLE prompts FROM PUBLIC;

DO $$
DECLARE
    api_role_name text;
BEGIN
    FOREACH api_role_name IN ARRAY ARRAY['anon', 'authenticated'] LOOP
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = api_role_name) THEN
            EXECUTE format('REVOKE ALL ON TABLE prompts FROM %I', api_role_name);
        END IF;
    END LOOP;
END $$;

CREATE INDEX IF NOT EXISTS prompts_session_idx
    ON prompts (session_id, created_at, id);
