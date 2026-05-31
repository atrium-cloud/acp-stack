ALTER TABLE prompts ADD COLUMN message_id TEXT;
ALTER TABLE prompts ADD COLUMN message_id_acknowledged INTEGER NOT NULL DEFAULT 0
    CHECK (message_id_acknowledged IN (0, 1));

CREATE UNIQUE INDEX IF NOT EXISTS prompts_session_message_id_idx
    ON prompts(session_id, message_id)
    WHERE message_id IS NOT NULL;
