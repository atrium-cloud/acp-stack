-- Replace the minimal placeholder `auth_failures` schema from migration 001 with
-- the full 0.0.1 auth schema. Preserve any manually-created or experimental
-- legacy rows because this table is audit/security related.
ALTER TABLE auth_failures RENAME TO auth_failures_legacy_001;

CREATE TABLE auth_failures (
    id TEXT PRIMARY KEY,
    created_at TEXT NOT NULL,
    key_kind TEXT NOT NULL,
    reason TEXT NOT NULL,
    client_ip TEXT,
    route TEXT,
    payload_json TEXT NOT NULL CHECK (json_valid(payload_json))
);

INSERT INTO auth_failures
    (id, created_at, key_kind, reason, client_ip, route, payload_json)
SELECT
    id,
    created_at,
    'unknown',
    reason,
    client_label,
    NULL,
    json_object(
        'legacy_client_label', client_label,
        'reason', reason
    )
FROM auth_failures_legacy_001;

DROP TABLE auth_failures_legacy_001;
