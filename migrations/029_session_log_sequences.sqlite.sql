-- Per-session event sequence: `seq` numbers a session's events 1, 2, 3, ...
-- in log order. Rows without a session scope keep a NULL `seq`.

ALTER TABLE events ADD COLUMN seq INTEGER;

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

ALTER TABLE sessions ADD COLUMN change_seq INTEGER NOT NULL DEFAULT 0;

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
    id             INTEGER PRIMARY KEY CHECK (id = 1),
    change_seq     INTEGER NOT NULL,
    pruned_through INTEGER NOT NULL DEFAULT 0
);

INSERT INTO session_change_counter (id, change_seq, pruned_through)
SELECT 1, COUNT(*), 0 FROM sessions;

-- Delete tombstones: a deleted session keeps one row here, stamped with the
-- `change_seq` of its deletion, so the change feed reports the deletion.

CREATE TABLE IF NOT EXISTS session_tombstones (
    session_id TEXT PRIMARY KEY,
    deleted_at TEXT NOT NULL,
    change_seq INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS session_tombstones_change_seq_idx
    ON session_tombstones (change_seq);

CREATE INDEX IF NOT EXISTS session_tombstones_deleted_at_idx
    ON session_tombstones (deleted_at);
