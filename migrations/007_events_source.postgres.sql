ALTER TABLE events ADD COLUMN source text NOT NULL DEFAULT 'system';

CREATE INDEX IF NOT EXISTS idx_events_source ON events(source);
CREATE INDEX IF NOT EXISTS idx_events_created_kind ON events(created_at, kind);
CREATE INDEX IF NOT EXISTS idx_events_kind_created ON events(kind, created_at);

-- Analytics views over the raw mirror. Names mirror the spec's initial
-- Supabase table list (docs/specs/state-logging.md). These views exist only
-- in the Postgres dialect because Supabase consumes them for hosted dashboards;
-- the SQLite store is queried by acpctl via the raw tables.
CREATE OR REPLACE VIEW session_turns
WITH (security_invoker = true) AS
SELECT id, session_id, status, stop_reason, error_code, error_message,
       created_at, updated_at, prompt_json
FROM prompts;

CREATE OR REPLACE VIEW permissions
WITH (security_invoker = true) AS
SELECT
    r.id            AS request_id,
    r.created_at    AS requested_at,
    r.updated_at    AS request_updated_at,
    r.status,
    r.source,
    r.requester,
    r.subject_id,
    r.detail_json,
    r.expires_at,
    d.id            AS decision_id,
    d.created_at    AS decided_at,
    d.decision,
    d.deciding_principal,
    d.reason
FROM permission_requests AS r
LEFT JOIN permission_decisions AS d ON d.request_id = r.id;

CREATE OR REPLACE VIEW agent_events
WITH (security_invoker = true) AS
SELECT id, created_at, event_kind AS kind, message, payload_json,
       'agent_lifecycle'::text AS source
FROM agent_lifecycle
UNION ALL
SELECT id, created_at, kind, message, payload_json, source
FROM events
WHERE kind LIKE 'agent.%';

CREATE OR REPLACE VIEW security_events
WITH (security_invoker = true) AS
SELECT id, created_at, key_kind AS kind, reason AS message, payload_json,
       'auth_failures'::text AS source
FROM auth_failures
UNION ALL
SELECT id, created_at, kind, message, payload_json, source
FROM events
WHERE kind LIKE 'security.%';

CREATE OR REPLACE VIEW connection_events
WITH (security_invoker = true) AS
SELECT id, created_at, kind, message, payload_json, source, session_id
FROM events
WHERE kind IN ('api.request', 'ws.client_connected', 'ws.client_disconnected');

CREATE OR REPLACE VIEW usage_metrics
WITH (security_invoker = true) AS
SELECT id, created_at, kind, message, payload_json, source, session_id
FROM events
WHERE kind = 'usage.reported';

REVOKE ALL ON TABLE session_turns, permissions, agent_events,
    security_events, connection_events, usage_metrics
FROM PUBLIC;

DO $$
DECLARE
    api_role_name text;
BEGIN
    FOREACH api_role_name IN ARRAY ARRAY['anon', 'authenticated'] LOOP
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = api_role_name) THEN
            EXECUTE format(
                'REVOKE ALL ON TABLE session_turns, permissions, agent_events, security_events, connection_events, usage_metrics FROM %I',
                api_role_name
            );
        END IF;
    END LOOP;
END $$;
