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
    id                 TEXT PRIMARY KEY,
    started_at         TEXT NOT NULL,
    finished_at        TEXT NOT NULL,
    status             TEXT NOT NULL CHECK (status IN ('succeeded','failed')),
    ok                 INTEGER NOT NULL CHECK (ok IN (0, 1)),
    critical_count     INTEGER NOT NULL,
    warning_count      INTEGER NOT NULL,
    auth_failure_count INTEGER NOT NULL,
    inputs_json        TEXT NOT NULL CHECK (json_valid(inputs_json))
);

CREATE INDEX IF NOT EXISTS idx_security_runs_started_at
    ON security_runs(started_at DESC, id DESC);

CREATE TABLE IF NOT EXISTS security_findings (
    run_id       TEXT NOT NULL REFERENCES security_runs(id),
    ordinal      INTEGER NOT NULL,
    code         TEXT NOT NULL,
    severity     TEXT NOT NULL CHECK (severity IN ('warning','critical')),
    message      TEXT NOT NULL,
    details_json TEXT CHECK (details_json IS NULL OR json_valid(details_json)),
    remediation  TEXT,
    PRIMARY KEY (run_id, ordinal)
);

CREATE INDEX IF NOT EXISTS idx_security_findings_run
    ON security_findings(run_id, ordinal);
