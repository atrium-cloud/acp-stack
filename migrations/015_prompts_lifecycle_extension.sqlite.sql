-- Extend the prompts lifecycle: add `stalled` as a terminal status and
-- introduce `failure_class` + `failure_detail_json` so terminal `errored`
-- and `stalled` rows can carry an internal failure taxonomy (Phase 2 will
-- start populating these). SQLite cannot alter a CHECK constraint in place,
-- so we rebuild the table following the standard recipe. The Postgres
-- migration mirrors the same rebuild so the dialect-parity test sees a
-- single CREATE TABLE shape per side.

ALTER TABLE prompts RENAME TO prompts_legacy_004;

CREATE TABLE prompts (
    id            TEXT PRIMARY KEY,
    session_id    TEXT NOT NULL REFERENCES sessions(id),
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL,
    status        TEXT NOT NULL
        CHECK (status IN ('pending', 'running', 'completed', 'errored', 'cancelled', 'stalled')),
    stop_reason   TEXT,
    error_code    TEXT,
    error_message TEXT,
    prompt_json   TEXT NOT NULL CHECK (json_valid(prompt_json)),
    failure_class TEXT,
    failure_detail_json TEXT
);

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
