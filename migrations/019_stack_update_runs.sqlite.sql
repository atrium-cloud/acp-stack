-- stack_update_runs records every `acps update` check/install attempt for
-- acp-stack itself. It is separate from installer_runs, which is scoped to
-- managed agent/dependency installation.

CREATE TABLE IF NOT EXISTS stack_update_runs (
  id TEXT PRIMARY KEY,
  started_at TEXT NOT NULL,
  finished_at TEXT,
  operation TEXT NOT NULL,
  status TEXT NOT NULL,
  current_version TEXT NOT NULL,
  target_version TEXT,
  target_tag TEXT,
  classification TEXT,
  breaking INTEGER NOT NULL DEFAULT 0,
  major_upgrade INTEGER NOT NULL DEFAULT 0,
  policy TEXT NOT NULL,
  auto INTEGER NOT NULL DEFAULT 0,
  message TEXT,
  payload_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(payload_json))
);

CREATE INDEX IF NOT EXISTS idx_stack_update_runs_started
ON stack_update_runs(started_at DESC, id DESC);
