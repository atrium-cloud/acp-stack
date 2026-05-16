-- Replace the minimal placeholder `auth_failures` schema from migration 001 with
-- the full 0.0.1 auth schema. Preserve any manually-created or experimental
-- legacy rows because this table is audit/security related.
ALTER TABLE auth_failures RENAME TO auth_failures_legacy_001;

CREATE TABLE auth_failures (
    id text PRIMARY KEY,
    created_at timestamptz NOT NULL,
    key_kind text NOT NULL,
    reason text NOT NULL,
    client_ip text,
    route text,
    payload_json jsonb NOT NULL
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
    jsonb_build_object(
        'legacy_client_label', client_label,
        'reason', reason
    )
FROM auth_failures_legacy_001;

DROP TABLE auth_failures_legacy_001;
