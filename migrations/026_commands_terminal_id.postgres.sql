ALTER TABLE commands ADD COLUMN terminal_id text;

CREATE INDEX IF NOT EXISTS commands_terminal_id_idx
    ON commands (terminal_id)
    WHERE terminal_id IS NOT NULL;
