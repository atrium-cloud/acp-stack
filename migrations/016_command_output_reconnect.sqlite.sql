ALTER TABLE commands ADD COLUMN last_output_event_id TEXT;
ALTER TABLE commands ADD COLUMN last_output_at TEXT;
ALTER TABLE commands ADD COLUMN last_output_seq INTEGER;
ALTER TABLE commands ADD COLUMN output_bytes INTEGER NOT NULL DEFAULT 0;
ALTER TABLE commands ADD COLUMN last_progress_at TEXT;

CREATE INDEX IF NOT EXISTS commands_last_progress_idx
    ON commands(status, last_progress_at);
