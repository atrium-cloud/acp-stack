ALTER TABLE commands ADD COLUMN started_at TEXT;
ALTER TABLE commands ADD COLUMN finished_at TEXT;
ALTER TABLE commands ADD COLUMN cwd TEXT;
ALTER TABLE commands ADD COLUMN env_json TEXT CHECK (env_json IS NULL OR json_valid(env_json));
ALTER TABLE commands ADD COLUMN duration_ms INTEGER;
ALTER TABLE commands ADD COLUMN truncated INTEGER NOT NULL DEFAULT 0;
