ALTER TABLE commands ADD COLUMN origin text NOT NULL DEFAULT 'operator';
ALTER TABLE commands ADD COLUMN session_id text;

CREATE INDEX IF NOT EXISTS commands_session_id_idx
    ON commands (session_id)
    WHERE session_id IS NOT NULL;
