ALTER TABLE events ADD COLUMN source TEXT NOT NULL DEFAULT 'system';

CREATE INDEX IF NOT EXISTS idx_events_source ON events(source);
CREATE INDEX IF NOT EXISTS idx_events_created_kind ON events(created_at, kind);
CREATE INDEX IF NOT EXISTS idx_events_kind_created ON events(kind, created_at);
