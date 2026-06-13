-- Local outbox for the Supabase logging sink. The sink table itself is kept
-- in the shared dialect manifest so a future on-Postgres state store would
-- carry the same delivery bookkeeping shape; today only the SQLite copy is
-- written by the runtime.

CREATE TABLE IF NOT EXISTS sink_outbox (
    id text PRIMARY KEY,
    source_table text NOT NULL,
    source_id text NOT NULL,
    created_at timestamptz NOT NULL,
    status text NOT NULL
        CHECK (status IN ('pending', 'sending', 'sent', 'failed')),
    attempts bigint NOT NULL DEFAULT 0,
    next_attempt_at timestamptz,
    last_error text,
    last_attempt_at timestamptz
);

CREATE INDEX IF NOT EXISTS sink_outbox_pending_idx
    ON sink_outbox (status, next_attempt_at, created_at);

CREATE TABLE IF NOT EXISTS sink_failures_summary (
    window_started_at timestamptz PRIMARY KEY,
    failure_count bigint NOT NULL,
    last_error text,
    last_observed_at timestamptz NOT NULL
);

ALTER TABLE sink_outbox ENABLE ROW LEVEL SECURITY;
ALTER TABLE sink_failures_summary ENABLE ROW LEVEL SECURITY;

REVOKE ALL ON TABLE sink_outbox, sink_failures_summary
FROM PUBLIC;

DO $$
DECLARE
    api_role_name text;
BEGIN
    FOREACH api_role_name IN ARRAY ARRAY['anon', 'authenticated'] LOOP
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = api_role_name) THEN
            EXECUTE format(
                'REVOKE ALL ON TABLE sink_outbox, sink_failures_summary FROM %I',
                api_role_name
            );
        END IF;
    END LOOP;
END $$;
