-- Standardize the terminal-state spelling on `cancelled` (double-l), matching
-- the ACP protocol and the sessions/prompts columns that already use it.
-- Data-only: no table, column, or index is introduced.
--
-- `updated_at` is deliberately left alone. This is a spelling rename of an
-- existing terminal state, not a new state transition, so touching the
-- timestamp would fabricate activity that never happened.
--
-- Historical `events.payload_json` blobs, `events.message` text, and
-- `permission_decisions.reason` values are deliberately not rewritten: they
-- are an append-only audit record of what was emitted at the time.

UPDATE commands SET status = 'cancelled' WHERE status = 'canceled';

UPDATE permission_requests SET status = 'cancelled' WHERE status = 'canceled';

UPDATE permission_decisions SET decision = 'cancelled' WHERE decision = 'canceled';

UPDATE events SET kind = 'command.cancelled' WHERE kind = 'command.canceled';

UPDATE events SET kind = 'permission.cancelled' WHERE kind = 'permission.canceled';
