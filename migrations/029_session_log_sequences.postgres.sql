-- Per-session event sequence: `seq` numbers a session's events 1, 2, 3, ...
-- in log order. Rows without a session scope keep a NULL `seq`.

ALTER TABLE events ADD COLUMN seq bigint;

UPDATE events
SET seq = numbered.seq
FROM (
    SELECT id,
           ROW_NUMBER() OVER (PARTITION BY session_id ORDER BY created_at, id) AS seq
    FROM events
    WHERE session_id IS NOT NULL
) AS numbered
WHERE events.id = numbered.id;

CREATE UNIQUE INDEX IF NOT EXISTS events_session_seq_idx
    ON events (session_id, seq);

-- Change feed: `change_seq` stamps a session's latest activity from one
-- process-wide counter. The counter row holds the last value issued, so it
-- keeps climbing when the session holding the highest value is deleted, and
-- `pruned_through`, the highest `change_seq` of a pruned tombstone.

ALTER TABLE sessions ADD COLUMN change_seq bigint NOT NULL DEFAULT 0;

UPDATE sessions
SET change_seq = numbered.change_seq
FROM (
    SELECT id,
           ROW_NUMBER() OVER (ORDER BY updated_at, id) AS change_seq
    FROM sessions
) AS numbered
WHERE sessions.id = numbered.id;

CREATE INDEX IF NOT EXISTS sessions_change_seq_idx
    ON sessions (change_seq);

CREATE TABLE IF NOT EXISTS session_change_counter (
    id             integer PRIMARY KEY CHECK (id = 1),
    change_seq     bigint NOT NULL,
    pruned_through bigint NOT NULL DEFAULT 0
);

ALTER TABLE session_change_counter ENABLE ROW LEVEL SECURITY;
REVOKE ALL ON TABLE session_change_counter FROM PUBLIC;

DO $$
DECLARE
    api_role_name text;
BEGIN
    FOREACH api_role_name IN ARRAY ARRAY['anon', 'authenticated'] LOOP
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = api_role_name) THEN
            EXECUTE format('REVOKE ALL ON TABLE session_change_counter FROM %I', api_role_name);
        END IF;
    END LOOP;
END $$;

INSERT INTO session_change_counter (id, change_seq, pruned_through)
SELECT 1, COUNT(*), 0 FROM sessions;

-- Delete tombstones: a deleted session keeps one row here, stamped with the
-- `change_seq` of its deletion, so the change feed reports the deletion.

CREATE TABLE IF NOT EXISTS session_tombstones (
    session_id text PRIMARY KEY,
    deleted_at timestamptz NOT NULL,
    change_seq bigint NOT NULL
);

ALTER TABLE session_tombstones ENABLE ROW LEVEL SECURITY;
REVOKE ALL ON TABLE session_tombstones FROM PUBLIC;

DO $$
DECLARE
    api_role_name text;
BEGIN
    FOREACH api_role_name IN ARRAY ARRAY['anon', 'authenticated'] LOOP
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = api_role_name) THEN
            EXECUTE format('REVOKE ALL ON TABLE session_tombstones FROM %I', api_role_name);
        END IF;
    END LOOP;
END $$;

CREATE INDEX IF NOT EXISTS session_tombstones_change_seq_idx
    ON session_tombstones (change_seq);

CREATE INDEX IF NOT EXISTS session_tombstones_deleted_at_idx
    ON session_tombstones (deleted_at);
