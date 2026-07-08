ALTER TABLE commands ADD COLUMN origin TEXT NOT NULL DEFAULT 'operator';
ALTER TABLE commands ADD COLUMN session_id TEXT;

CREATE INDEX IF NOT EXISTS commands_session_id_idx
    ON commands (session_id)
    WHERE session_id IS NOT NULL;
