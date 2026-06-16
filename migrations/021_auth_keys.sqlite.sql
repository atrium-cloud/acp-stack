CREATE TABLE auth_keys (
    key_kind TEXT PRIMARY KEY CHECK (key_kind IN ('session', 'admin')),
    algorithm TEXT NOT NULL,
    salt TEXT NOT NULL,
    digest TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
