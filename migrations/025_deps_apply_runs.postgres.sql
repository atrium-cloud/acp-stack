-- deps_apply_runs records one row per dependency-apply invocation, keyed by
-- the apply_run_id that already groups the per-action installer_runs rows.
-- The partial unique index makes "at most one live apply" a transactional
-- guarantee across processes (daemon, CLI, init, detached init child).

CREATE TABLE IF NOT EXISTS deps_apply_runs (
  id text PRIMARY KEY,
  started_at timestamptz NOT NULL,
  finished_at timestamptz,
  status text NOT NULL,
  origin text NOT NULL,
  init_run_id text,
  feature text,
  pid bigint,
  boot_id text,
  total bigint NOT NULL DEFAULT 0,
  completed bigint NOT NULL DEFAULT 0,
  installed bigint NOT NULL DEFAULT 0,
  already_present bigint NOT NULL DEFAULT 0,
  privilege_required bigint NOT NULL DEFAULT 0,
  failed bigint NOT NULL DEFAULT 0,
  current_dep text,
  log_dir text,
  error_code text,
  error_detail text,
  payload_json jsonb NOT NULL DEFAULT '{}'::jsonb
);

ALTER TABLE deps_apply_runs ENABLE ROW LEVEL SECURITY;
REVOKE ALL ON TABLE deps_apply_runs FROM PUBLIC;

DO $$
DECLARE
    api_role_name text;
BEGIN
    FOREACH api_role_name IN ARRAY ARRAY['anon', 'authenticated'] LOOP
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = api_role_name) THEN
            EXECUTE format('REVOKE ALL ON TABLE deps_apply_runs FROM %I', api_role_name);
        END IF;
    END LOOP;
END $$;

CREATE UNIQUE INDEX IF NOT EXISTS idx_deps_apply_runs_single_running
ON deps_apply_runs(status) WHERE status = 'running';

CREATE INDEX IF NOT EXISTS idx_deps_apply_runs_started
ON deps_apply_runs(started_at DESC, id DESC);
