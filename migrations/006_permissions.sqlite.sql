CREATE TABLE permission_requests (
  id TEXT PRIMARY KEY,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  status TEXT NOT NULL,
  source TEXT NOT NULL,
  requester TEXT,
  subject_id TEXT,
  detail_json TEXT NOT NULL CHECK (json_valid(detail_json)),
  expires_at TEXT
);

CREATE INDEX idx_permission_requests_status_created
  ON permission_requests(status, created_at);

CREATE TABLE permission_decisions (
  id TEXT PRIMARY KEY,
  request_id TEXT NOT NULL REFERENCES permission_requests(id),
  created_at TEXT NOT NULL,
  decision TEXT NOT NULL,
  deciding_principal TEXT,
  reason TEXT
);

CREATE INDEX idx_permission_decisions_request
  ON permission_decisions(request_id);
