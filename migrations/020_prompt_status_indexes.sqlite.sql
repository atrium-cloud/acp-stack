CREATE INDEX IF NOT EXISTS prompts_created_at_idx
    ON prompts (created_at, session_id, id);

CREATE INDEX IF NOT EXISTS prompts_updated_at_idx
    ON prompts (updated_at, session_id, id);
