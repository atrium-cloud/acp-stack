CREATE TABLE IF NOT EXISTS events (
    id text PRIMARY KEY,
    created_at timestamptz NOT NULL,
    level text NOT NULL,
    kind text NOT NULL,
    message text NOT NULL,
    payload_json jsonb NOT NULL
);

CREATE TABLE IF NOT EXISTS sessions (
    id text PRIMARY KEY,
    created_at timestamptz NOT NULL,
    updated_at timestamptz NOT NULL,
    status text NOT NULL
);

CREATE TABLE IF NOT EXISTS commands (
    id text PRIMARY KEY,
    created_at timestamptz NOT NULL,
    updated_at timestamptz NOT NULL,
    status text NOT NULL,
    command text NOT NULL,
    exit_status bigint
);

CREATE TABLE IF NOT EXISTS agent_lifecycle (
    id text PRIMARY KEY,
    created_at timestamptz NOT NULL,
    event_kind text NOT NULL,
    message text NOT NULL,
    payload_json jsonb NOT NULL
);

CREATE TABLE IF NOT EXISTS auth_failures (
    id text PRIMARY KEY,
    created_at timestamptz NOT NULL,
    client_label text,
    reason text NOT NULL
);

CREATE TABLE IF NOT EXISTS installer_runs (
    id text PRIMARY KEY,
    started_at timestamptz NOT NULL,
    finished_at timestamptz,
    status text NOT NULL,
    stdout text NOT NULL,
    stderr text NOT NULL,
    exit_status bigint
);

ALTER TABLE events ENABLE ROW LEVEL SECURITY;
ALTER TABLE sessions ENABLE ROW LEVEL SECURITY;
ALTER TABLE commands ENABLE ROW LEVEL SECURITY;
ALTER TABLE agent_lifecycle ENABLE ROW LEVEL SECURITY;
ALTER TABLE auth_failures ENABLE ROW LEVEL SECURITY;
ALTER TABLE installer_runs ENABLE ROW LEVEL SECURITY;

REVOKE ALL ON TABLE events, sessions, commands, agent_lifecycle,
    auth_failures, installer_runs
FROM PUBLIC;

DO $$
DECLARE
    api_role_name text;
BEGIN
    FOREACH api_role_name IN ARRAY ARRAY['anon', 'authenticated'] LOOP
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = api_role_name) THEN
            EXECUTE format(
                'REVOKE ALL ON TABLE events, sessions, commands, agent_lifecycle, auth_failures, installer_runs FROM %I',
                api_role_name
            );
        END IF;
    END LOOP;
END $$;
