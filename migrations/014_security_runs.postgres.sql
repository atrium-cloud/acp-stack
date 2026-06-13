-- Durable history of security self-check runs.
--
-- `security_runs` records each `GET /v1/security/check` invocation with an
-- aggregate verdict and counts. `security_findings` records the individual
-- findings emitted by that run; `ordinal` preserves the orchestrator's emit
-- order so the operator-facing show view replays a run as it was produced.
--
-- `status` is always terminal (`succeeded` when no critical findings were
-- emitted, `failed` otherwise) because the check is synchronous and the row is
-- written after `crate::security::check()` returns. `inputs_json` captures the
-- redacted shape of `SecurityCheckInputs` (no key material) so a historical run
-- remains reinterpretable after operator config changes.

CREATE TABLE IF NOT EXISTS security_runs (
    id                 text PRIMARY KEY,
    started_at         text NOT NULL,
    finished_at        text NOT NULL,
    status             text NOT NULL CHECK (status IN ('succeeded','failed')),
    ok                 integer NOT NULL CHECK (ok IN (0, 1)),
    critical_count     integer NOT NULL,
    warning_count      integer NOT NULL,
    auth_failure_count integer NOT NULL,
    inputs_json        text NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_security_runs_started_at
    ON security_runs(started_at DESC, id DESC);

CREATE TABLE IF NOT EXISTS security_findings (
    run_id       text NOT NULL REFERENCES security_runs(id),
    ordinal      integer NOT NULL,
    code         text NOT NULL,
    severity     text NOT NULL CHECK (severity IN ('warning','critical')),
    message      text NOT NULL,
    details_json text,
    remediation  text,
    PRIMARY KEY (run_id, ordinal)
);

ALTER TABLE security_runs ENABLE ROW LEVEL SECURITY;
ALTER TABLE security_findings ENABLE ROW LEVEL SECURITY;

REVOKE ALL ON TABLE security_runs, security_findings
FROM PUBLIC;

DO $$
DECLARE
    api_role_name text;
BEGIN
    FOREACH api_role_name IN ARRAY ARRAY['anon', 'authenticated'] LOOP
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = api_role_name) THEN
            EXECUTE format(
                'REVOKE ALL ON TABLE security_runs, security_findings FROM %I',
                api_role_name
            );
        END IF;
    END LOOP;
END $$;

CREATE INDEX IF NOT EXISTS idx_security_findings_run
    ON security_findings(run_id, ordinal);
