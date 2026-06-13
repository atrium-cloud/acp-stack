-- Top-level init run state machine.
--
-- `init_runs` records each `acps init` invocation so a crash partway through
-- can be resumed. `init_steps` records one row per phase that executes or
-- resumes (secrets, agent install, provider config, workspace lanes, headless
-- config, edge artifacts, testflight). The orchestrator verifies the
-- postcondition of any prior `succeeded` row and skips it on the next run;
-- `failed`/`pending`/`running` rows are re-executed.
--
-- `installer_runs` (added in migration 001 and extended through 011) is the
-- per-installer-step ledger and is not replaced by this table. `init_steps`
-- with kind `agent_install` points back to the matching `installer_runs.id`
-- via `payload_json.installer_run_id` so the operator can correlate a
-- top-level init step with its underlying install attempts.

CREATE TABLE IF NOT EXISTS init_runs (
    id text PRIMARY KEY,
    started_at text NOT NULL,
    finished_at text,
    status text NOT NULL CHECK (status IN ('pending','running','succeeded','failed')),
    runtime_user text,
    agent_id text,
    args_json text NOT NULL DEFAULT '{}'
);

CREATE INDEX IF NOT EXISTS idx_init_runs_started_at
    ON init_runs(started_at DESC, id DESC);

CREATE TABLE IF NOT EXISTS init_steps (
    id text PRIMARY KEY,
    run_id text NOT NULL REFERENCES init_runs(id),
    ordinal integer NOT NULL,
    kind text NOT NULL,
    status text NOT NULL CHECK (status IN ('pending','running','succeeded','skipped','failed')),
    started_at text,
    finished_at text,
    log_dir text,
    error_kind text,
    error_detail text,
    payload_json text NOT NULL DEFAULT '{}',
    UNIQUE(run_id, ordinal)
);

ALTER TABLE init_runs ENABLE ROW LEVEL SECURITY;
ALTER TABLE init_steps ENABLE ROW LEVEL SECURITY;

REVOKE ALL ON TABLE init_runs, init_steps
FROM PUBLIC;

DO $$
DECLARE
    api_role_name text;
BEGIN
    FOREACH api_role_name IN ARRAY ARRAY['anon', 'authenticated'] LOOP
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = api_role_name) THEN
            EXECUTE format(
                'REVOKE ALL ON TABLE init_runs, init_steps FROM %I',
                api_role_name
            );
        END IF;
    END LOOP;
END $$;

CREATE INDEX IF NOT EXISTS idx_init_steps_run_id
    ON init_steps(run_id, ordinal);
