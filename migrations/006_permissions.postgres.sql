CREATE TABLE permission_requests (
  id text PRIMARY KEY,
  created_at timestamptz NOT NULL,
  updated_at timestamptz NOT NULL,
  status text NOT NULL,
  source text NOT NULL,
  requester text,
  subject_id text,
  detail_json jsonb NOT NULL,
  expires_at timestamptz
);

CREATE INDEX idx_permission_requests_status_created
  ON permission_requests(status, created_at);

CREATE TABLE permission_decisions (
  id text PRIMARY KEY,
  request_id text NOT NULL REFERENCES permission_requests(id),
  created_at timestamptz NOT NULL,
  decision text NOT NULL,
  deciding_principal text,
  reason text
);

CREATE INDEX idx_permission_decisions_request
  ON permission_decisions(request_id);
