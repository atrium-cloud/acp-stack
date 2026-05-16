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
