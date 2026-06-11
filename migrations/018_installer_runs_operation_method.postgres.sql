-- installer_runs.operation distinguishes original installs from later
-- update attempts. method records the source path used by the step when known.
ALTER TABLE installer_runs ADD COLUMN operation text NOT NULL DEFAULT 'install';
ALTER TABLE installer_runs ADD COLUMN method text;

-- Queries key on (agent_id, step) and order by started_at; no query filters by
-- operation, so it is not part of the index.
CREATE INDEX IF NOT EXISTS idx_installer_runs_agent_step
ON installer_runs(agent_id, step, started_at);
