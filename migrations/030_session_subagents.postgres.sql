-- Subagent sessions: a child session the agent announced on a parent through
-- ACP `subagent_update`. The child keeps its own `sessions` row; this row
-- links it to its parent and to the root of its tree and holds the parent's
-- latest patch for it.

CREATE TABLE IF NOT EXISTS session_subagents (
    session_id        text PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
    parent_session_id text NOT NULL REFERENCES sessions(id),
    root_session_id   text NOT NULL REFERENCES sessions(id),
    title             text,
    description       text,
    state_json        text,
    state_current     integer NOT NULL DEFAULT 0 CHECK (state_current IN (0, 1)),
    cancel_granted    integer NOT NULL DEFAULT 0 CHECK (cancel_granted IN (0, 1)),
    announced_at      timestamptz NOT NULL,
    updated_at        timestamptz NOT NULL
);

ALTER TABLE session_subagents ENABLE ROW LEVEL SECURITY;
REVOKE ALL ON TABLE session_subagents FROM PUBLIC;

DO $$
DECLARE
    api_role_name text;
BEGIN
    FOREACH api_role_name IN ARRAY ARRAY['anon', 'authenticated'] LOOP
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = api_role_name) THEN
            EXECUTE format('REVOKE ALL ON TABLE session_subagents FROM %I', api_role_name);
        END IF;
    END LOOP;
END $$;

CREATE INDEX IF NOT EXISTS session_subagents_parent_idx
    ON session_subagents (parent_session_id);

CREATE INDEX IF NOT EXISTS session_subagents_root_idx
    ON session_subagents (root_session_id);
