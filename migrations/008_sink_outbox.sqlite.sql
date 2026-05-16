-- Local outbox for the Supabase logging sink.
--
-- Every persistence call site that runs while external logging is enabled
-- enqueues an outbox row in the same transaction that writes the source row,
-- so a crash between the source INSERT and the enqueue cannot drop a row from
-- the delivery pipeline. The background worker selects pending rows ordered by
-- `(status, next_attempt_at, created_at)` and POSTs them to PostgREST with
-- `Prefer: resolution=merge-duplicates,return=minimal`, making replay safe.

CREATE TABLE IF NOT EXISTS sink_outbox (
    id TEXT PRIMARY KEY,
    source_table TEXT NOT NULL,
    source_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    status TEXT NOT NULL
        CHECK (status IN ('pending', 'sending', 'sent', 'failed')),
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt_at TEXT,
    last_error TEXT,
    last_attempt_at TEXT
);

CREATE INDEX IF NOT EXISTS sink_outbox_pending_idx
    ON sink_outbox (status, next_attempt_at, created_at);

CREATE TABLE IF NOT EXISTS sink_failures_summary (
    window_started_at TEXT PRIMARY KEY,
    failure_count INTEGER NOT NULL,
    last_error TEXT,
    last_observed_at TEXT NOT NULL
);
