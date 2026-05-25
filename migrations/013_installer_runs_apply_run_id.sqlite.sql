-- installer_runs.apply_run_id groups every row written by one `acps deps apply`
-- invocation. Legacy rows keep NULL and are interpreted with the pre-013
-- timestamp fallback by health/status code.

ALTER TABLE installer_runs ADD COLUMN apply_run_id TEXT;

CREATE INDEX IF NOT EXISTS idx_installer_runs_deps_apply_run
ON installer_runs(agent_id, step, apply_run_id, started_at);
