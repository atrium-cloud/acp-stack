-- stack_update_runs records every `acps update` check/install attempt for
-- acp-stack itself. It is separate from installer_runs, which is scoped to
-- managed agent/dependency installation.

CREATE TABLE IF NOT EXISTS stack_update_runs (
  id text PRIMARY KEY,
  started_at timestamptz NOT NULL,
  finished_at timestamptz,
  operation text NOT NULL,
  status text NOT NULL,
  current_version text NOT NULL,
  target_version text,
  target_tag text,
  classification text,
  breaking boolean NOT NULL DEFAULT false,
  major_upgrade boolean NOT NULL DEFAULT false,
  policy text NOT NULL,
  auto boolean NOT NULL DEFAULT false,
  message text,
  payload_json jsonb NOT NULL DEFAULT '{}'::jsonb
);

ALTER TABLE stack_update_runs ENABLE ROW LEVEL SECURITY;
REVOKE ALL ON TABLE stack_update_runs FROM PUBLIC;

DO $$
DECLARE
    api_role_name text;
BEGIN
    FOREACH api_role_name IN ARRAY ARRAY['anon', 'authenticated'] LOOP
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = api_role_name) THEN
            EXECUTE format('REVOKE ALL ON TABLE stack_update_runs FROM %I', api_role_name);
        END IF;
    END LOOP;
END $$;

CREATE INDEX IF NOT EXISTS idx_stack_update_runs_started
ON stack_update_runs(started_at DESC, id DESC);
