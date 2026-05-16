ALTER TABLE commands ADD COLUMN started_at timestamptz;
ALTER TABLE commands ADD COLUMN finished_at timestamptz;
ALTER TABLE commands ADD COLUMN cwd text;
ALTER TABLE commands ADD COLUMN env_json jsonb;
ALTER TABLE commands ADD COLUMN duration_ms bigint;
ALTER TABLE commands ADD COLUMN truncated bigint NOT NULL DEFAULT 0;
