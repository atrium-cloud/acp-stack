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
    id TEXT PRIMARY KEY,
    started_at TEXT NOT NULL,
    finished_at TEXT,
    status TEXT NOT NULL CHECK (status IN ('pending','running','succeeded','failed')),
    runtime_user TEXT,
    agent_id TEXT,
    args_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(args_json))
);

CREATE INDEX IF NOT EXISTS idx_init_runs_started_at
    ON init_runs(started_at DESC, id DESC);

CREATE TABLE IF NOT EXISTS init_steps (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES init_runs(id),
    ordinal INTEGER NOT NULL,
    kind TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('pending','running','succeeded','skipped','failed')),
    started_at TEXT,
    finished_at TEXT,
    log_dir TEXT,
    error_kind TEXT,
    error_detail TEXT,
    payload_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(payload_json)),
    UNIQUE(run_id, ordinal)
);

CREATE INDEX IF NOT EXISTS idx_init_steps_run_id
    ON init_steps(run_id, ordinal);
