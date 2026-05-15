-- Latest-only snapshot of the ACP `initialize` response per configured agent.
--
-- We deliberately store one row per agent_id, not a history of every capture.
-- `GET /v1/agent/capabilities` is on the hot path; history is dead weight
-- until a UI consumes it, and `agent_lifecycle` already records every
-- `agent.started` event for trace purposes.
CREATE TABLE IF NOT EXISTS agent_capabilities (
    agent_id TEXT PRIMARY KEY,
    captured_at TEXT NOT NULL,
    capabilities_json TEXT NOT NULL CHECK (json_valid(capabilities_json))
);
