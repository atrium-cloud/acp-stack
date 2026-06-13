-- Extend the prompts lifecycle: add `stalled` as a terminal status and
-- introduce `failure_class` + `failure_detail_json` so terminal `errored`
-- and `stalled` rows can carry an internal failure taxonomy (Phase 2 will
-- start populating these). We mirror the SQLite table-rebuild so the
-- dialect-parity test sees a single CREATE TABLE shape per side; this also
-- avoids hardcoding the auto-generated `prompts_status_check` constraint
-- name. `session_turns` depends on prompts and must be dropped/recreated.

DROP VIEW IF EXISTS session_turns;

ALTER TABLE prompts RENAME TO prompts_legacy_004;

CREATE TABLE prompts (
    id            text PRIMARY KEY,
    session_id    text NOT NULL REFERENCES sessions(id),
    created_at    timestamptz NOT NULL,
    updated_at    timestamptz NOT NULL,
    status        text NOT NULL
        CHECK (status IN ('pending', 'running', 'completed', 'errored', 'cancelled', 'stalled')),
    stop_reason   text,
    error_code    text,
    error_message text,
    prompt_json   jsonb NOT NULL,
    failure_class text,
    failure_detail_json text
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

INSERT INTO prompts
    (id, session_id, created_at, updated_at, status,
     stop_reason, error_code, error_message, prompt_json,
     failure_class, failure_detail_json)
SELECT
    id, session_id, created_at, updated_at, status,
    stop_reason, error_code, error_message, prompt_json,
    NULL, NULL
FROM prompts_legacy_004;

DROP TABLE IF EXISTS prompts_legacy_004;

CREATE INDEX IF NOT EXISTS prompts_session_idx
    ON prompts (session_id, created_at, id);

CREATE INDEX IF NOT EXISTS prompts_status_updated_at_idx
    ON prompts (status, updated_at);

CREATE OR REPLACE VIEW session_turns
WITH (security_invoker = true) AS
SELECT id, session_id, status, stop_reason, error_code, error_message,
       created_at, updated_at, prompt_json
FROM prompts;

REVOKE ALL ON TABLE session_turns FROM PUBLIC;

DO $$
DECLARE
    api_role_name text;
BEGIN
    FOREACH api_role_name IN ARRAY ARRAY['anon', 'authenticated'] LOOP
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = api_role_name) THEN
            EXECUTE format('REVOKE ALL ON TABLE session_turns FROM %I', api_role_name);
        END IF;
    END LOOP;
END $$;
