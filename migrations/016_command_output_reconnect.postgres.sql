ALTER TABLE commands ADD COLUMN last_output_event_id text;
ALTER TABLE commands ADD COLUMN last_output_at timestamptz;
ALTER TABLE commands ADD COLUMN last_output_seq bigint;
ALTER TABLE commands ADD COLUMN output_bytes bigint NOT NULL DEFAULT 0;
ALTER TABLE commands ADD COLUMN last_progress_at timestamptz;

CREATE INDEX IF NOT EXISTS commands_last_progress_idx
    ON commands(status, last_progress_at);
