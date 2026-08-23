-- deps_apply_runs records one row per dependency-apply invocation, keyed by
-- the apply_run_id that already groups the per-action installer_runs rows.
-- The partial unique index makes "at most one live apply" a transactional
-- guarantee across processes (daemon, CLI, init, detached init child).

CREATE TABLE IF NOT EXISTS deps_apply_runs (
  id TEXT PRIMARY KEY,
  started_at TEXT NOT NULL,
  finished_at TEXT,
  status TEXT NOT NULL,
  origin TEXT NOT NULL,
  init_run_id TEXT,
  feature TEXT,
  pid INTEGER,
  boot_id TEXT,
  total INTEGER NOT NULL DEFAULT 0,
  completed INTEGER NOT NULL DEFAULT 0,
  installed INTEGER NOT NULL DEFAULT 0,
  already_present INTEGER NOT NULL DEFAULT 0,
  privilege_required INTEGER NOT NULL DEFAULT 0,
  failed INTEGER NOT NULL DEFAULT 0,
  current_dep TEXT,
  log_dir TEXT,
  error_code TEXT,
  error_detail TEXT,
  payload_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(payload_json))
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_deps_apply_runs_single_running
ON deps_apply_runs(status) WHERE status = 'running';

CREATE INDEX IF NOT EXISTS idx_deps_apply_runs_started
ON deps_apply_runs(started_at DESC, id DESC);
