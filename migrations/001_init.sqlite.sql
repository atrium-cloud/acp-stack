CREATE TABLE IF NOT EXISTS events (
    id TEXT PRIMARY KEY,
    created_at TEXT NOT NULL,
    level TEXT NOT NULL,
    kind TEXT NOT NULL,
    message TEXT NOT NULL,
    payload_json TEXT NOT NULL CHECK (json_valid(payload_json))
);

CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    status TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS commands (
    id TEXT PRIMARY KEY,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    status TEXT NOT NULL,
    command TEXT NOT NULL,
    exit_status INTEGER
);

CREATE TABLE IF NOT EXISTS agent_lifecycle (
    id TEXT PRIMARY KEY,
    created_at TEXT NOT NULL,
    event_kind TEXT NOT NULL,
    message TEXT NOT NULL,
    payload_json TEXT NOT NULL CHECK (json_valid(payload_json))
);

CREATE TABLE IF NOT EXISTS auth_failures (
    id TEXT PRIMARY KEY,
    created_at TEXT NOT NULL,
    client_label TEXT,
    reason TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS installer_runs (
    id TEXT PRIMARY KEY,
    started_at TEXT NOT NULL,
    finished_at TEXT,
    status TEXT NOT NULL,
    stdout TEXT NOT NULL,
    stderr TEXT NOT NULL,
    exit_status INTEGER
);
